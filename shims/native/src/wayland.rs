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
use gex_kernel::unix::{HostListener, MemFd, UnixSocket};
use gex_kernel::vfs::FileHandle;
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
    /// Presents counted for `GEX_FPS` (count since the last report, and when).
    fps: Mutex<Option<(u32, std::time::Instant)>>,
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
        let Some(path) = self.frame_path(&prefix) else { return };
        let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
        out.extend_from_slice(&rgb);
        if let Err(e) = std::fs::write(&path, out) {
            eprintln!("gex: could not write {path}: {e}");
        }
    }

    /// Count a presented frame and say where to write it, if anywhere. A
    /// game presents tens of frames a second, so a run of minutes cannot keep
    /// them all: `GEX_FRAME_EVERY=N` keeps every Nth, and the prefix `null`
    /// keeps none. `GEX_FPS=1` prints presents per second to stderr, which is
    /// what a measurement script reads.
    fn frame_path(&self, prefix: &str) -> Option<String> {
        if std::env::var_os("GEX_FPS").is_some_and(|v| v != "0") {
            let mut fps = self.inner.fps.lock().unwrap();
            let (count, since) = fps.get_or_insert((0, std::time::Instant::now()));
            *count += 1;
            let dt = since.elapsed().as_secs_f64();
            if dt >= 1.0 {
                eprintln!("[gex] fps {:.1}", *count as f64 / dt);
                *count = 0;
                *since = std::time::Instant::now();
            }
        }
        let mut n = self.inner.written.lock().unwrap();
        let index = *n;
        *n += 1;
        let every: u32 = std::env::var("GEX_FRAME_EVERY").ok().and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
        if prefix == "null" || index % every != 0 {
            return None;
        }
        Some(format!("{prefix}-{index:03}.ppm"))
    }

    /// Write a frame drawn somewhere other than a client buffer (a GL
    /// client's host surface): RGBA rows, top first.
    pub fn present_external(&self, width: i32, height: i32, rgba: &[u8]) {
        let Some(prefix) = self.inner.out.lock().unwrap().clone() else {
            return;
        };
        let Some(path) = self.frame_path(&prefix) else { return };
        let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
        out.extend(rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]));
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
            s.out.extend_from_slice(&s.client.take_outgoing());
            // A `wl_keyboard.keymap` event owes a descriptor for its `h`
            // argument: an in-memory file holding the keymap, staged on the
            // socket so it rides the next send (as the browser host does).
            if let Some(km) = s.client.take_keymap() {
                let memfd = Arc::new(MemFd::new("gex-keymap"));
                let _ = memfd.write_at(0, &km);
                let handle: Arc<dyn FileHandle> = memfd;
                let file = OpenFile::new(handle, "anon:wl-keymap".into(), 0, true, false);
                s.socket.stage_send(vec![file]);
            }
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
        let file = self.fds.get(fd as usize)?;
        // The client mapped this shared, so the live pixels are in the guest's
        // memory; the file's own bytes are what was there before it drew (zero).
        let map = self
            .kernel
            .shared_maps
            .iter()
            .find(|m| Arc::ptr_eq(&m.file, file))?;
        let len = size.min(map.len as usize);
        self.mem.read(map.start, len).ok().map(|b| b.to_vec())
    }
}
