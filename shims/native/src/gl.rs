//! The guest's graphics device: `/dev/gex-gl` (native debug host).
//!
//! An `ioctl` node whose call reaches the host's OpenGL. The commands are
//! already in guest memory and the host can read it, so the call carries an
//! address rather than data -- one host call per frame however many GL calls
//! it holds.

use gex_gl::host::{read_header, Frame, Gl};
use gex_gl::proto::{error, Header, MAGIC};
use gex_gl::trace;
use gex_kernel::errno::{Errno, SysResult};
use gex_kernel::vfs::{FileHandle, FileType, Metadata};
use gex_kernel::GuestMem;
use std::io::Write;
use std::sync::{Arc, Mutex};

/// `_IOWR('g', 1, ...)`: hand over the batch whose header is at `arg`. A private
/// number, not DRM's, since this is not a DRM device.
pub const GEX_GL_SUBMIT: u64 = 0xc008_6701;
/// `_IOR('g', 2, ...)`: how big a batch the device will take.
pub const GEX_GL_LIMITS: u64 = 0x8008_6702;
/// As [`GEX_GL_SUBMIT`], but returns once the batch is handed over: it runs
/// on the device's GL thread while the guest goes on (`GEX_GL_SYNC=1` makes
/// it a plain submit). A failure is reported by the next submit.
pub const GEX_GL_SUBMIT_ASYNC: u64 = 0xc008_6704;

/// The largest batch the device will read in one go.
const MAX_BATCH: u32 = 32 * 1024 * 1024;

/// The device, and the host GL context behind it. The context is used on
/// whichever thread submits (a GL thread's channel round trip measured 23 of a
/// 24us GL call; `Gl` documents why this is sound), and made lazily on first
/// submit so a guest that never draws pays no EGL cost.
pub struct GlDevice {
    gl: Mutex<Option<Result<Gl, String>>>,
    size: (i32, i32),
    /// Where `GEX_GL_CAPTURE` said to record the command stream, and the file
    /// once one batch has arrived. Recording is a debugging aid: a guest whose
    /// trace cannot be written still runs.
    capture: Mutex<Capture>,
    /// Where presented frames go (`--wayland-out`), if anywhere.
    sink: Mutex<Option<FrameSink>>,
    /// The thread every batch runs on (it owns the EGL context), started at
    /// the first submit.
    worker: Mutex<Option<std::sync::mpsc::Sender<Job>>>,
    /// Asynchronous batches handed over and not run yet, and the first
    /// failure among those run since the last submit looked.
    pending: Arc<(Mutex<(usize, Option<u32>)>, std::sync::Condvar)>,
    /// This device, for the GL thread to hold (ioctls get `&self`).
    me: std::sync::Weak<GlDevice>,
    /// Batches run on a GL thread of their own, and asynchronous submits are
    /// taken (`set_threaded`, for a display that drops frames). Otherwise
    /// every batch runs on the submitting thread, as it always did, and
    /// `GEX_GL_SUBMIT_ASYNC` is refused (the guest then submits normally).
    threaded: std::sync::atomic::AtomicBool,
    /// The guest's memory (`set_memory`): a synchronous batch is read where
    /// it is in it.
    memory: Mutex<Option<gex_mem::GuestMemory>>,
}

/// `GEX_GL_SYNC=1`: every batch runs before its submit returns.
fn sync_only() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("GEX_GL_SYNC").as_deref() == Ok("1"))
}

/// One batch for the GL thread; `done` takes the result back to a
/// synchronous submit: (reply, status, index of the failed command).
struct Job {
    batch: Batch,
    done: Option<std::sync::mpsc::Sender<(Vec<u8>, u32, u32)>>,
}

/// A batch's bytes: a copy (an asynchronous submit, whose ring the guest
/// goes on filling), or the ring itself in guest memory (a synchronous one:
/// the submitting thread waits for the reply, so the ring is not rewritten
/// meanwhile, and its megabytes of texture data are not copied once more).
enum Batch {
    Owned(Vec<u8>),
    /// The guest's memory, and the batch's range in it.
    Ring(gex_mem::GuestMemory, std::ops::Range<usize>),
}

impl Batch {
    fn bytes(&self) -> &[u8] {
        match self {
            Batch::Owned(v) => v,
            Batch::Ring(m, r) => &m.bytes()[r.clone()],
        }
    }
}

/// What a presented frame is handed to.
pub type FrameSink = Box<dyn Fn(Frame) + Send + Sync>;

/// The process's device, once `--gl` made one, so the host can route its
/// frames once the compositor exists.
pub static DEVICE: std::sync::OnceLock<Arc<GlDevice>> = std::sync::OnceLock::new();

/// The recording half of the device.
#[derive(Default)]
enum Capture {
    /// `GEX_GL_CAPTURE` was unset, or the file stopped being writable.
    #[default]
    Off,
    /// Asked for, not opened yet: the header carries the surface size, so it
    /// is written when the first batch proves a guest is really drawing.
    Requested(std::path::PathBuf),
    On(std::fs::File),
}

impl std::fmt::Debug for GlDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GlDevice")
    }
}

impl GlDevice {
    pub fn new(width: i32, height: i32) -> Arc<GlDevice> {
        let capture = match std::env::var_os("GEX_GL_CAPTURE") {
            Some(path) if !path.is_empty() => Capture::Requested(path.into()),
            _ => Capture::Off,
        };
        Arc::new_cyclic(|me| GlDevice {
            gl: Mutex::new(None),
            size: (width, height),
            capture: Mutex::new(capture),
            sink: Mutex::new(None),
            worker: Mutex::new(None),
            pending: Arc::new((Mutex::new((0, None)), std::sync::Condvar::new())),
            me: me.clone(),
            threaded: std::sync::atomic::AtomicBool::new(false),
            memory: Mutex::new(None),
        })
    }

    /// Append one batch to the recording, if there is one.
    ///
    /// Recorded before execution rather than after, so a batch that makes the
    /// host GL abort is in the file that gets replayed.
    fn record(&self, batch: &[u8], reply_capacity: u32) {
        let mut capture = self.capture.lock().unwrap();
        if let Capture::Requested(path) = &*capture {
            *capture = match std::fs::File::create(path).and_then(|mut file| {
                file.write_all(&trace::header(self.size.0 as u32, self.size.1 as u32))?;
                Ok(file)
            }) {
                Ok(file) => Capture::On(file),
                Err(e) => {
                    eprintln!("gex: cannot record GL to {}: {e}", path.display());
                    Capture::Off
                }
            };
        }
        if let Capture::On(file) = &mut *capture {
            if let Err(e) = file
                .write_all(&trace::record(batch, reply_capacity))
                .and_then(|()| file.flush())
            {
                eprintln!("gex: GL recording stopped: {e}");
                *capture = Capture::Off;
            }
        }
    }

    /// Hand every presented frame to `sink` (read back from the host's
    /// surface at each PRESENT).
    pub fn set_frame_sink(&self, sink: FrameSink) {
        *self.sink.lock().unwrap() = Some(sink);
    }

    /// The guest's memory, for reading synchronous batches in place.
    pub fn set_memory(&self, m: gex_mem::GuestMemory) {
        *self.memory.lock().unwrap() = Some(m);
    }

    /// Run batches on a GL thread and take asynchronous submits: for a
    /// display that drops frames, where a frame's GL can overlap the guest
    /// building the next. Before the first batch.
    pub fn set_threaded(&self) {
        self.threaded.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn threaded(&self) -> bool {
        self.threaded.load(std::sync::atomic::Ordering::Relaxed) && !sync_only()
    }

    /// What the host's GL turned out to be, once a guest has asked for it.
    pub fn describe(&self) -> Option<String> {
        match &*self.gl.lock().unwrap() {
            Some(Ok(gl)) => Some(format!("{} / {}", gl.renderer, gl.version)),
            Some(Err(e)) => Some(format!("unavailable: {e}")),
            None => None,
        }
    }

    /// Run `batch` on this thread: (reply, status, index of the failed
    /// command). The GL thread's half.
    fn execute(&self, batch: &[u8]) -> (Vec<u8>, u32, u32) {
        let mut slot = self.gl.lock().unwrap();
        if slot.is_none() {
            *slot = Some(Gl::new(self.size.0, self.size.1));
        }
        let sink = self.sink.lock().unwrap();
        match slot.as_mut().unwrap() {
            Ok(gl) => {
                gl.keep_frames(sink.is_some());
                let (reply, status, executed) = gl.run(batch);
                if let (Some(sink), Some(frame)) = (sink.as_ref(), gl.take_frame()) {
                    sink(frame);
                }
                (reply.bytes().to_vec(), status, executed)
            }
            // No GL is reported in the header, not as a failed ioctl, so the
            // guest falls back as it would for a driver that refused to load.
            Err(e) => {
                static SAID: std::sync::Once = std::sync::Once::new();
                SAID.call_once(|| eprintln!("gex-gl: no host GL context: {e}"));
                (Vec::new(), error::NO_CONTEXT, 0)
            }
        }
    }

    /// The GL thread's queue, started on first use. Batches run in the order
    /// submitted, synchronous and asynchronous alike.
    fn queue(&self) -> std::sync::mpsc::Sender<Job> {
        let mut w = self.worker.lock().unwrap();
        if let Some(tx) = w.as_ref() {
            return tx.clone();
        }
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let dev = self.me.upgrade().expect("a GL device in use is alive");
        std::thread::Builder::new()
            .name("gex-gl".into())
            .spawn(move || {
                for job in rx {
                    let r = dev.execute(job.batch.bytes());
                    match job.done {
                        Some(done) => {
                            let _ = done.send(r);
                        }
                        None => {
                            let (m, cv) = &*dev.pending;
                            let mut p = m.lock().unwrap();
                            if r.1 != error::NONE && p.1.is_none() {
                                p.1 = Some(r.1);
                            }
                            p.0 -= 1;
                            cv.notify_all();
                        }
                    }
                }
            })
            .expect("starting the GL thread");
        *w = Some(tx.clone());
        tx
    }

    /// Wait until at most `n` asynchronous batches are still to run; the first
    /// failure among those that ran, if any.
    fn drain_to(&self, n: usize) -> Option<u32> {
        let (m, cv) = &*self.pending;
        let mut p = m.lock().unwrap();
        while p.0 > n {
            p = cv.wait(p).unwrap();
        }
        p.1.take()
    }

    /// The header and command bytes of the ring at `ring` (a copy, for an
    /// asynchronous submit).
    fn take_batch(&self, m: &mut GuestMem<'_>, ring: u64) -> Result<(Header, Vec<u8>), Errno> {
        let (header, batch) = self.ring_batch(m, ring)?;
        Ok((header, batch.to_vec()))
    }

    /// The header, and the command bytes where they are in the ring.
    fn ring_batch<'m>(&self, m: &'m mut GuestMem<'_>, ring: u64) -> Result<(Header, &'m [u8]), Errno> {
        let head = m.read(ring, Header::SIZE)?.to_vec();
        let Some(header) = read_header(&head) else {
            return Err(Errno::EINVAL);
        };
        if header.magic != MAGIC {
            return Err(Errno::EINVAL);
        }
        if header.capacity > MAX_BATCH || header.tail > header.capacity {
            return Err(Errno::EINVAL);
        }
        let batch = m.read(ring + Header::SIZE as u64, header.tail as usize)?;
        self.record(batch, header.reply_capacity);
        Ok((header, batch))
    }

    /// Run one batch, reading it out of guest memory and writing the reply back.
    fn submit(&self, m: &mut GuestMem<'_>, ring: u64) -> SysResult {
        let (header, batch) = self.ring_batch(m, ring)?;
        let (bytes, status, executed) = if self.threaded() {
            // In place when the ring is in the memory the device knows (the
            // process's own; a forked child's is copied as before).
            let start = ring as usize + Header::SIZE;
            let mem = self.memory.lock().unwrap().clone().filter(|g| g.bytes().as_ptr() as usize + start == batch.as_ptr() as usize);
            let batch = match mem {
                Some(g) => Batch::Ring(g, start..start + batch.len()),
                None => Batch::Owned(batch.to_vec()),
            };
            let (tx, rx) = std::sync::mpsc::channel();
            if self.queue().send(Job { batch, done: Some(tx) }).is_err() {
                return Err(Errno::EIO);
            }
            // The GL thread reads the ring until it answers: wait for that
            // whatever happens.
            rx.recv().map_err(|_| Errno::EIO)?
        } else {
            self.execute(batch)
        };
        // Behind it in the queue, so everything handed over before has run.
        if let Some(failed) = self.drain_to(0) {
            write_status(m, ring, failed, 0, 0)?;
            return Ok(0);
        }
        let room = header.reply_capacity as usize;
        let n = bytes.len().min(room);
        if n > 0 {
            let at = ring + Header::SIZE as u64 + header.capacity as u64;
            m.write(at, &bytes[..n])?;
        }
        write_status(m, ring, status, executed, n as u32)?;
        Ok(0)
    }

    /// Hand one batch to the GL thread and return: a frame's GL runs while the
    /// guest builds the next. At most one is in flight (the next waits for
    /// it, as a swap chain of two would). A failure of the one before is
    /// this call's.
    fn submit_async(&self, m: &mut GuestMem<'_>, ring: u64) -> SysResult {
        let (_, batch) = self.take_batch(m, ring)?;
        let failed = self.drain_to(0);
        self.pending.0.lock().unwrap().0 += 1;
        if self.queue().send(Job { batch: Batch::Owned(batch), done: None }).is_err() {
            return Err(Errno::EIO);
        }
        write_status(m, ring, error::NONE, 0, 0)?;
        match failed {
            Some(_) => Err(Errno::EIO),
            None => Ok(0),
        }
    }
}

/// Update the header after a batch: commands consumed, reply length recorded.
fn write_status(
    m: &mut GuestMem<'_>,
    ring: u64,
    status: u32,
    failed_at: u32,
    reply_len: u32,
) -> Result<(), Errno> {
    m.write_u32(ring + 8, 0)?; // tail: consumed
    m.write_u32(ring + 16, reply_len)?;
    m.write_u32(ring + 20, status)?;
    m.write_u32(ring + 24, failed_at)?;
    Ok(())
}

impl FileHandle for GlDevice {
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> SysResult {
        Err(Errno::EINVAL)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> SysResult {
        Err(Errno::EINVAL)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: FileType::CharDevice,
            size: 0,
            mode: 0o666,
            ino: 0,
            mtime: 0,
            nlink: 1,
            rdev: 0,
        })
    }

    fn ioctl(&self, m: &mut GuestMem<'_>, request: u64, arg: u64) -> Option<SysResult> {
        // `ioctl`'s request is a C `int`, so every `_IOWR` arrives sign-extended.
        match request & 0xffff_ffff {
            GEX_GL_SUBMIT => Some(self.submit(m, arg)),
            GEX_GL_SUBMIT_ASYNC if self.threaded() => Some(self.submit_async(m, arg)),
            GEX_GL_LIMITS => Some(
                m.write_u32(arg, MAX_BATCH)
                    .map(|()| 0)
                    .map_err(|e| e),
            ),
            _ => None,
        }
    }
}
