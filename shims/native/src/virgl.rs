//! A virtio-gpu renderer in the guest's socket namespace (native debug host).
//!
//! Stock guest Mesa's virgl driver connects to a UNIX socket and speaks vtest.
//! This binds that name and hands each client's stream to `gex_virgl`, run on
//! the host's OpenGL. Pump between the send syscall and the wait, or a guest
//! blocked with nobody servicing its socket never wakes.

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
    /// Reply bytes the guest's socket had no room for yet. A vtest reply can
    /// exceed the socket's 256 KB (a 300x300 `TRANSFER_GET` read-back is 360,000
    /// bytes); dropping the remainder leaves Mesa blocked forever waiting for it.
    out: Vec<u8>,
    /// How much of `out` has gone.
    sent: usize,
}

struct Inner {
    /// `None` until the first client connects, so a guest that never renders
    /// pays no host GL context cost.
    renderer: Option<Virgl>,
    /// Why the renderer could not be created, reported once not per connection.
    broken: Option<String>,
    sessions: Vec<Session>,
    verbose: bool,
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
    /// startup.
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
        // One destructure: the sessions are walked while the renderer is held
        // mutably, and the two share a struct.
        let Inner {
            renderer,
            sessions,
            verbose,
            ..
        } = &mut *inner;
        let verbose = *verbose;
        let Some(renderer) = renderer.as_mut() else {
            // Drop the clients rather than leave them blocked on a reply that
            // is never coming.
            sessions.clear();
            return;
        };
        // The GL context belongs to whichever thread last used it, rarely this
        // one, so rebind it here.
        renderer.bind_here();
        sessions.retain_mut(|s| {
            let mut gone = false;
            // Mesa submits a whole command buffer in one write: tens of KB for
            // a frame with real geometry.
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

            let produced = s.state.run(renderer);
            gex_virgl::vtest::record_replies(&produced);
            s.out.extend_from_slice(&produced);
            // As much as the socket will take; the rest waits for the next pump.
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
            if let Some(why) = &s.state.failed {
                if verbose {
                    eprintln!("[gex] virgl client dropped: {why}");
                }
                return false;
            }
            !gone
        });
        // Unbound before the lock drops, so the next thread to pump can take it.
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
            out: Vec::new(),
            sent: 0,
        });
    }
}
