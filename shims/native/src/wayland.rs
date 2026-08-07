//! Hosting the Wayland compositor for a guest.
//!
//! [`gex_wayland`] is the protocol and knows nothing about gex. This is the
//! part that joins it to a guest: it binds a name in the guest's unix-socket
//! namespace, pumps bytes between each connected client and its compositor
//! state, and answers the one question the protocol cannot -- where a
//! client's pixels actually are.
//!
//! # Where the pixels are
//!
//! A client allocates a buffer with `memfd_create`, maps it `MAP_SHARED`, and
//! sends the descriptor. In gex a shared mapping of an anonymous file *is* a
//! range of the guest's own memory, and the host can read guest memory
//! directly, so the compositor reads the client's framebuffer with no copy and
//! no round trip. That is the whole reason the mapping had to be made to alias
//! rather than to copy.
//!
//! # When it runs
//!
//! At the end of every syscall. A client writes a request and then waits for
//! the reply, so the pump has to have run between those two things or the wait
//! never ends -- and the write is itself a syscall, so it has.

use gex_kernel::file::OpenFile;
use gex_kernel::syscalls::Kernel;
use gex_kernel::unix::{HostListener, UnixSocket};
use gex_kernel::GuestMem;
use gex_wayland::{Buffers, Client, Frame};
use std::sync::{Arc, Mutex};

/// One connected client: its socket, its protocol state, and the descriptors
/// it has sent.
struct Session {
    socket: Arc<UnixSocket>,
    client: Client,
    /// Descriptors received over `SCM_RIGHTS`, in arrival order.
    ///
    /// The protocol refers to one by the index it arrived at rather than by
    /// the client's own descriptor number, which means nothing on this side.
    fds: Vec<Arc<OpenFile>>,
    /// Event bytes the client's socket had no room for yet, and how much of
    /// them has gone.
    ///
    /// Wayland events are small and a socket holds 256 KB, so this is
    /// usually empty. It is here because the same code in the virgl device
    /// dropped the tail of a short write and reasoned that it could not
    /// happen -- and then a framebuffer read-back, 360,000 bytes in one
    /// reply, made it happen. A dropped tail does not fail: it desynchronises
    /// the stream, and the client waits for bytes nobody is going to send.
    out: Vec<u8>,
    sent: usize,
}

/// The compositor, as the guest sees it: a name that accepts connections.
#[derive(Clone)]
pub struct Wayland {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    sessions: Mutex<Vec<Session>>,
    /// Frames committed since the host last collected them.
    frames: Mutex<Vec<Frame>>,
    size: Mutex<(i32, i32)>,
    /// Where to write each frame, and how many have been written.
    out: Mutex<Option<String>>,
    written: Mutex<u32>,
    /// The GPU compositor, when one could be started.
    ///
    /// Optional on purpose: a machine with no graphics adapter is a machine
    /// gex should still run on, and the difference is only where the
    /// compositing happens.
    gpu: Mutex<Option<gex_gpu::Compositor>>,
}

impl std::fmt::Debug for Wayland {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Wayland")
    }
}

impl Wayland {
    pub fn new(width: i32, height: i32) -> Wayland {
        Wayland {
            inner: Arc::new(Inner {
                size: Mutex::new((width, height)),
                ..Default::default()
            }),
        }
    }

    /// Composite on the GPU rather than by handing single frames to the host.
    ///
    /// Returns what the adapter turned out to be, or the reason there is
    /// none. Either way the compositor keeps working; this only decides
    /// whether surfaces are drawn onto a screen by a graphics processor or
    /// handed over one at a time.
    pub fn enable_gpu(&self, width: i32, height: i32) -> Result<String, String> {
        match gex_gpu::Compositor::new(width.max(1) as u32, height.max(1) as u32) {
            Ok(c) => {
                let name = c.adapter.clone();
                *self.inner.gpu.lock().unwrap() = Some(c);
                Ok(name)
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Write every committed frame as a PPM at `prefix-NNN.ppm`.
    ///
    /// A window would be better and a canvas better still, but a file is what
    /// a test can compare and what works on a machine with no display.
    pub fn write_frames_to(&self, prefix: &str) {
        *self.inner.out.lock().unwrap() = Some(prefix.to_string());
    }

    /// Write out what was drawn.
    ///
    /// With a GPU that is the whole screen, every surface composited onto it,
    /// which is what a screen is. Without one it is a single client buffer,
    /// because nothing here would otherwise combine them.
    fn save(&self, frames: &[Frame]) {
        let Some(prefix) = self.inner.out.lock().unwrap().clone() else {
            return;
        };
        let mut gpu = self.inner.gpu.lock().unwrap();
        let (width, height, rgb) = if let Some(c) = gpu.as_mut() {
            match c.draw(frames).and_then(|()| c.read_back()) {
                Ok(rgba) => {
                    let (w, h) = c.size();
                    (w as i32, h as i32, gex_gpu::rgba_to_rgb(&rgba))
                }
                Err(e) => {
                    eprintln!("gex: the GPU compositor failed: {e}");
                    return;
                }
            }
        } else {
            let Some(frame) = frames.last() else { return };
            (frame.width, frame.height, gex_wayland::to_rgb(frame))
        };
        let mut n = self.inner.written.lock().unwrap();
        let path = format!("{prefix}-{:03}.ppm", *n);
        *n += 1;
        let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
        out.extend_from_slice(&rgb);
        if let Err(e) = std::fs::write(&path, out) {
            eprintln!("gex: could not write {path}: {e}");
        }
    }

    /// Everything committed since the last call.
    pub fn take_frames(&self) -> Vec<Frame> {
        std::mem::take(&mut self.inner.frames.lock().unwrap())
    }

    pub fn client_count(&self) -> usize {
        self.inner.sessions.lock().unwrap().len()
    }

    /// Move bytes in both directions for every client, and collect commits.
    ///
    /// The kernel is passed because a buffer's pixels are found through its
    /// shared mappings, and guest memory because that is where they are.
    pub fn pump(&self, kernel: &Kernel, mem: &GuestMem<'_>) {
        let mut sessions = self.inner.sessions.lock().unwrap();
        let mut produced = Vec::new();
        sessions.retain_mut(|s| {
            let mut gone = false;
            // A big read because a toolkit's startup is a few kilobytes of
            // requests in one go.
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = match s.socket.recv(&mut buf, 32, false) {
                    Ok(0) => {
                        gone = true;
                        break;
                    }
                    Ok(n) => n as usize,
                    // Nothing more waiting.
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
            s.out.extend_from_slice(&s.client.take_outgoing());
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
            produced.extend(s.client.take_frames());
            !gone && s.client.failed.is_none()
        });
        drop(sessions);
        if !produced.is_empty() {
            // One screen per batch of commits, not one per surface: a
            // compositor draws everything that is up, together.
            self.save(&produced);
            self.inner.frames.lock().unwrap().extend(produced);
        }
    }
}

impl HostListener for Wayland {
    fn accept(&self, peer: Arc<UnixSocket>) {
        let mut client = Client::new();
        let (w, h) = *self.inner.size.lock().unwrap();
        client.set_default_size(w, h);
        self.inner.sessions.lock().unwrap().push(Session {
            socket: peer,
            client,
            fds: Vec::new(),
            out: Vec::new(),
            sent: 0,
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
    fn map(&self, fd: u32, size: usize) -> Option<Vec<u8>> {
        let file = self.fds.get(fd as usize)?;
        // The client mapped this shared, so the guest has it somewhere in its
        // address space and that mapping is the live one. Reading the file's
        // own bytes instead would give whatever was there before the client
        // started drawing, which is zeroes.
        let map = self
            .kernel
            .shared_maps
            .iter()
            .find(|m| Arc::ptr_eq(&m.file, file))?;
        let len = size.min(map.len as usize);
        self.mem.read(map.start, len).ok().map(|b| b.to_vec())
    }
}
