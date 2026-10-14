//! The gex WebGPU wire's server (guest/gpu/wire-server.c) on the host's
//! Dawn: the native host's `/dev/gex-gpu` hands each message a guest sends
//! to a [`Server`], and the guest the reply.
//!
//! Dawn keeps some state per thread (error scopes), so every server runs on
//! one thread of this crate's, whatever thread the guest calls from.

#[cfg(feature = "dawn")]
mod imp {
    use std::ffi::c_void;
    use std::sync::mpsc::{channel, Sender};
    use std::sync::{Mutex, OnceLock};

    #[repr(C)]
    struct RawServer(c_void);

    extern "C" {
        fn gexgpu_server_create_dawn() -> *mut RawServer;
        fn gexgpu_server_destroy(server: *mut RawServer);
        fn gexgpu_server_handle(
            server: *mut RawServer,
            data: *const c_void,
            size: usize,
            reply: *mut *const c_void,
            reply_size: *mut usize,
        ) -> bool;
    }

    /// A server pointer, used only on the Dawn thread.
    struct Ptr(*mut RawServer);
    unsafe impl Send for Ptr {}

    type Job = Box<dyn FnOnce() + Send>;

    /// The Dawn thread's queue.
    fn thread() -> &'static Mutex<Sender<Job>> {
        static T: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();
        T.get_or_init(|| {
            let (tx, rx) = channel::<Job>();
            std::thread::Builder::new()
                .name("gex-webgpu".into())
                .spawn(move || {
                    for job in rx {
                        job();
                    }
                })
                .expect("gex-webgpu: no thread");
            Mutex::new(tx)
        })
    }

    fn run<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
        let (tx, rx) = channel();
        let job: Job = Box::new(move || {
            let _ = tx.send(f());
        });
        thread().lock().unwrap().send(job).expect("gex-webgpu: thread gone");
        rx.recv().expect("gex-webgpu: thread gone")
    }

    /// One client's server: its objects are the client's ids.
    pub struct Server(Mutex<Option<Ptr>>);

    impl Server {
        pub fn new() -> Option<Server> {
            let p = run(|| Ptr(unsafe { gexgpu_server_create_dawn() }));
            (!p.0.is_null()).then(|| Server(Mutex::new(Some(p))))
        }

        /// Runs a message; the reply, and whether the message parsed.
        pub fn handle(&self, message: Vec<u8>) -> (Vec<u8>, bool) {
            // Held across the call, so one client's messages run in order.
            let guard = self.0.lock().unwrap();
            let Some(p) = guard.as_ref() else {
                return (Vec::new(), false);
            };
            let p = Ptr(p.0);
            run(move || {
                let p = p;
                let mut reply: *const c_void = std::ptr::null();
                let mut size = 0usize;
                let ok = unsafe {
                    gexgpu_server_handle(p.0, message.as_ptr().cast(), message.len(), &mut reply, &mut size)
                };
                let bytes = if reply.is_null() || size == 0 {
                    Vec::new()
                } else {
                    unsafe { std::slice::from_raw_parts(reply.cast::<u8>(), size) }.to_vec()
                };
                (bytes, ok)
            })
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            if let Some(p) = self.0.get_mut().unwrap().take() {
                run(move || {
                    let p = p;
                    unsafe { gexgpu_server_destroy(p.0) }
                });
            }
        }
    }
}

#[cfg(not(feature = "dawn"))]
mod imp {
    /// No Dawn in this build: no server can be made.
    pub struct Server(());

    impl Server {
        pub fn new() -> Option<Server> {
            None
        }

        pub fn handle(&self, _message: Vec<u8>) -> (Vec<u8>, bool) {
            (Vec::new(), false)
        }
    }
}

pub use imp::Server;

/// Whether this build can serve WebGPU at all.
pub const AVAILABLE: bool = cfg!(feature = "dawn");
