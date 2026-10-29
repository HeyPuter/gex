//! What the crate calls in its host: the libc the host was linked with, and the JS library (`js/gex_embed.js`).

#[cfg(target_arch = "wasm32")]
mod imp {
    #[repr(C)]
    pub struct Timespec {
        pub tv_sec: i64,
        pub tv_nsec: i32,
    }

    extern "C" {
        pub fn write(fd: i32, p: *const u8, n: usize) -> isize;
        pub fn abort() -> !;
        pub fn exit(code: i32) -> !;
        pub fn clock_gettime(clk: i32, ts: *mut Timespec) -> i32;
        pub fn pthread_self() -> usize;
        pub fn fma(a: f64, b: f64, c: f64) -> f64;
        pub fn fmaf(a: f32, b: f32, c: f32) -> f32;
    }

    /// A thread id for the guest: the pthread's, which is unique among live threads.
    pub unsafe fn gettid() -> u32 {
        pthread_self() as u32
    }

    /// One line on the host's stderr. Rust's own stderr is a no-op on `wasm32-unknown-unknown`, so without this a
    /// panic message goes nowhere.
    pub fn log(s: &str) {
        let mut b = s.as_bytes();
        while !b.is_empty() {
            let n = unsafe { write(2, b.as_ptr(), b.len()) };
            if n <= 0 {
                return;
            }
            b = &b[n as usize..];
        }
    }

    /// Panics end the host process: `panic=abort`, and a panic means gex's state is no longer trustworthy. The hook
    /// runs before the abort and says why.
    pub fn install_panic_hook() {
        std::panic::set_hook(Box::new(|info| {
            log(&format!("gex-embed: {info}\n"));
            unsafe { abort() }
        }));
    }

    /// The monotonic clock, in nanoseconds.
    pub fn now_ns() -> u64 {
        let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { clock_gettime(1, &mut ts) };
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }

    /// The JS library (`js/gex_embed.js`). All three take the descriptor (`crate::desc`).
    pub mod js {
        extern "C" {
            /// Install every module in the log this Worker has not: 1 if there were any.
            pub fn gexjs_sync(desc: u32) -> u32;
            /// Run translated code from `pc` with context `ctx` until it leaves (a return pad's miss).
            pub fn gexjs_enter(desc: u32, ctx: u32, pc: u64);
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use imp::*;

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    pub fn log(s: &str) {
        eprint!("{s}");
    }
    pub fn install_panic_hook() {}
    pub fn now_ns() -> u64 {
        0
    }
    pub unsafe fn gettid() -> u32 {
        0
    }
    pub mod js {
        pub unsafe fn gexjs_sync(_: u32) -> u32 {
            unimplemented!("gex-embed runs translated code only as wasm")
        }
        pub unsafe fn gexjs_enter(_: u32, _: u32, _: u64) {
            unimplemented!("gex-embed runs translated code only as wasm")
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use imp::*;

/// Report and end the process: something the guest did that gex can't go on from.
pub fn fatal(msg: &str) -> ! {
    log(&format!("gex-embed: {msg}\n"));
    #[cfg(target_arch = "wasm32")]
    unsafe {
        abort()
    }
    #[cfg(not(target_arch = "wasm32"))]
    panic!("{msg}")
}
