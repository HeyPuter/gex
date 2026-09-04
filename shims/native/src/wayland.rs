//! Hosting the Wayland compositor for a guest (native debug host).
//!
//! [`gex_wayland`] is the protocol; this joins it to a guest -- binds a name in
//! the unix-socket namespace, pumps bytes between each client and its
//! compositor state, and answers where a client's pixels are. A client maps its
//! buffer `MAP_SHARED`, which in gex aliases a range of guest memory, so the
//! compositor reads the framebuffer directly with no copy. Pumped at the end of
//! every syscall, so a client's wait always has a serviced reply.

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
    /// Descriptors received over `SCM_RIGHTS`, in arrival order: the protocol
    /// refers to one by arrival index, not the client's descriptor number.
    fds: Vec<Arc<OpenFile>>,
    /// Event bytes the client's socket had no room for yet, and how many have
    /// gone. Usually empty (events are small), but a dropped tail does not fail
    /// -- it desyncs the stream and leaves the client waiting forever.
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
    /// The GPU compositor, when one could be started. Optional: a machine with
    /// no graphics adapter still runs, compositing on the host instead.
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

    /// Composite on the GPU rather than handing single frames to the host.
    /// Returns the adapter, or the reason there is none; either way the
    /// compositor keeps working.
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

    /// Write every committed frame as a PPM at `prefix-NNN.ppm` -- a file is
    /// what a test can compare and what works with no display.
    pub fn write_frames_to(&self, prefix: &str) {
        *self.inner.out.lock().unwrap() = Some(prefix.to_string());
    }

    /// Write out what was drawn: with a GPU, the whole composited screen;
    /// without one, a single client buffer (nothing here would combine them).
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

    /// Move bytes in both directions for every client, and collect commits. The
    /// kernel locates a buffer's pixels through its shared mappings; guest
    /// memory is where they are.
    pub fn pump(&self, kernel: &Kernel, mem: &GuestMem<'_>) {
        let mut sessions = self.inner.sessions.lock().unwrap();
        let mut produced = Vec::new();
        sessions.retain_mut(|s| {
            let mut gone = false;
            // A big read: a toolkit's startup is a few KB of requests at once.
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
            // A `get_keyboard` produced a `wl_keyboard.keymap` whose fd rides
            // the socket's ancillary data: back the payload with a memfd and
            // stage it, so the send below carries it with the message. Without
            // it every keyboard-using client fails its first roundtrip with
            // "file descriptor expected" (EINVAL) -- GTK and Qt both do.
            if let Some(km) = s.client.take_keymap() {
                let memfd = Arc::new(gex_kernel::unix::MemFd::new("gex-keymap"));
                let _ = gex_kernel::vfs::FileHandle::write_at(&*memfd, 0, &km);
                let handle: Arc<dyn gex_kernel::vfs::FileHandle> = memfd;
                let file = OpenFile::new(handle, "/memfd:gex-keymap".to_string(), 0, true, false);
                s.socket.stage_send(vec![file]);
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
            // One screen per batch of commits, not one per surface.
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
        let Some(file) = self.fds.get(fd as usize) else {
            if std::env::var_os("GEX_WL_TRACE").is_some() {
                eprintln!("[wl-map] fd {fd} not in session fds (have {})", self.fds.len());
            }
            return None;
        };
        // When the pump is running for the process that owns this pool, the
        // live pixels are in that process's guest memory (fast, no copy). But
        // the compositor pumps every session on any process's syscall, so a
        // client's commit can be processed while another process's memory is
        // mounted -- then this pool is not in *this* kernel's shared maps.
        // Fall back to the file behind the descriptor: gex keeps a
        // single-writer shared mapping coherent with its file at each
        // safepoint (see Kernel::sync_shared_maps), so the drawn pixels are
        // there regardless of whose memory is mounted.
        if let Some(map) = self
            .kernel
            .shared_maps
            .iter()
            .find(|m| Arc::ptr_eq(&m.file, file))
        {
            let len = size.min(map.len as usize);
            if let Ok(bytes) = self.mem.read(map.start, len) {
                return Some(bytes.to_vec());
            }
        }
        let mut buf = vec![0u8; size];
        match gex_kernel::vfs::FileHandle::read_at(&*file.handle, 0, &mut buf) {
            Ok(n) if n > 0 => {
                if std::env::var_os("GEX_WL_TRACE").is_some() {
                    eprintln!("[wl-map] fd {fd} read {n} bytes from the file (cross-process)");
                }
                Some(buf)
            }
            _ => {
                if std::env::var_os("GEX_WL_TRACE").is_some() {
                    eprintln!("[wl-map] fd {fd}: no shared map here and file empty");
                }
                None
            }
        }
    }
}
