//! A virtio-gpu renderer in the guest's socket namespace.
//!
//! Stock Mesa in the guest, told to use its virgl driver, connects to a UNIX
//! socket and speaks the vtest protocol over it. This binds that name and
//! services the connections, handing each client's command stream to
//! `gex_virgl`, which runs it on the host's real OpenGL.
//!
//! The shape is the compositor's, for the same reason: a client writes a
//! request and then waits for the reply, and the write is a syscall, so the
//! pump has to run between the syscall that sends and the one that waits. A
//! guest blocked with nobody servicing its socket is a guest that never wakes.
//!
//! What makes this worth the trouble is what is *not* here. There is no guest
//! driver, no shim library, no interposed loader -- the guest runs Debian's
//! own Mesa and Debian's own binaries. Everything that makes desktop OpenGL
//! work, from immediate mode to display lists, happens inside that Mesa before
//! a byte reaches this file.

use gex_kernel::unix::{HostListener, UnixSocket};
use gex_virgl::{vtest, Virgl};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Narrate connections and pumps. The protocol trace alone cannot tell a
/// client that was never serviced from one that never spoke.
fn tracing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("GEX_VIRGL_TRACE").is_some())
}

/// Where Mesa's vtest winsys looks unless `VTEST_SOCKET_NAME` says otherwise.
pub const DEFAULT_SOCKET: &str = "/tmp/.virgl_test";

struct Session {
    socket: Arc<UnixSocket>,
    state: vtest::Session,
}

struct Inner {
    /// `None` until the first client connects. Bringing up a GL context costs
    /// a Mesa initialisation on the host, and a guest that never renders
    /// should not pay it.
    renderer: Option<Virgl>,
    /// Why the renderer could not be created, said once rather than per
    /// connection.
    broken: Option<String>,
    sessions: Vec<Session>,
    verbose: bool,
    /// How many clients have ever connected, so a session that vanishes can
    /// be told from one that never arrived.
    accepted: u64,
}

#[derive(Clone)]
pub struct VirglDevice {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for VirglDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VirglDevice")
    }
}

impl VirglDevice {
    pub fn new(verbose: bool) -> VirglDevice {
        VirglDevice {
            inner: Arc::new(Mutex::new(Inner {
                renderer: None,
                broken: None,
                sessions: Vec::new(),
                verbose,
                accepted: 0,
            })),
        }
    }

    /// Whether this build could render at all, so the caller can say so at
    /// startup instead of leaving the guest to discover it.
    pub fn available() -> bool {
        gex_virgl::AVAILABLE
    }

    /// Move bytes in both directions for every client.
    pub fn pump(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if tracing() {
            // Only when it changes, or every blocked read would print.
            static LAST: AtomicU64 = AtomicU64::new(u64::MAX);
            let now = (inner.accepted << 8) | inner.sessions.len() as u64;
            if LAST.swap(now, Ordering::Relaxed) != now {
                eprintln!(
                    "[virgl] pump: {} live sessions, {} accepted in total",
                    inner.sessions.len(),
                    inner.accepted
                );
            }
        }
        if inner.sessions.is_empty() {
            return;
        }
        if inner.renderer.is_none() && inner.broken.is_none() {
            match Virgl::new() {
                Ok(v) => {
                    if inner.verbose {
                        eprintln!("[gex] {}", v.describe());
                    }
                    inner.renderer = Some(v);
                }
                Err(e) => {
                    eprintln!("gex: no host renderer ({e}); the guest has no GPU");
                    inner.broken = Some(e);
                }
            }
        }
        // One destructure, because the sessions are walked while the renderer
        // is held mutably and the two live in the same struct.
        let Inner {
            renderer,
            sessions,
            verbose,
            ..
        } = &mut *inner;
        let verbose = *verbose;
        let Some(renderer) = renderer.as_mut() else {
            // Nothing can be answered. Drop the clients rather than leave
            // them blocked on a reply that is never coming.
            sessions.clear();
            return;
        };
        // The context belongs to whichever thread last used it, and that is
        // rarely this one: a shell runs on one host thread and the program it
        // started on another, and both pump.
        renderer.bind_here();
        sessions.retain_mut(|s| {
            let mut gone = false;
            // Mesa submits a whole command buffer in one write, and those run
            // to tens of kilobytes for a frame with real geometry.
            let mut buf = vec![0u8; 256 * 1024];
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
                s.state.feed(&buf[..n]);
                if n < buf.len() {
                    break;
                }
            }

            let out = s.state.run(renderer);
            if !out.is_empty() {
                let _ = s.socket.send(&out);
            }
            if let Some(why) = &s.state.failed {
                if verbose {
                    eprintln!("[gex] virgl client dropped: {why}");
                }
                return false;
            }
            !gone
        });
        // Handed back before the lock is, so whichever thread pumps next can
        // take it. Holding it would leave that thread with none.
        renderer.unbind();
    }
}

impl HostListener for VirglDevice {
    fn accept(&self, peer: Arc<UnixSocket>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.accepted += 1;
        if tracing() {
            eprintln!("[virgl] client {} connected", inner.accepted);
        }
        inner.sessions.push(Session {
            socket: peer,
            state: vtest::Session::new(),
        });
    }
}
