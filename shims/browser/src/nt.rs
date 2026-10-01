//! The NT personality's browser host side (docs/nt-layer.md §10.2).
//!
//! The counterpart of `shims/native/src/nt.rs`, for a Windows program in a
//! page: the same `gex_kernel::nt` layer, entered from this shim's imports.
//!
//! - [`boot`] replaces the ELF placement in `gex_load`;
//! - [`syscall`] is the whole `syscall` import for an NT process, entered
//!   before `with_host` is ever taken;
//! - [`fault`] turns a breakpoint, an illegal instruction or a branch to
//!   memory that is not executable into an NT exception;
//! - [`tick`] keeps `KUSER_SHARED_DATA`'s clocks moving (there is no thread
//!   to spare for it, so it runs from the syscall and safepoint imports);
//! - a user-mode callback is a nested guest loop in JS (`gex_env.nt_callback`),
//!   entered from inside the `syscall` import on the calling Worker.
//!
//! One kernel: the process's `Kernel` is the one in [`Host`], and the NT
//! layer reaches it through [`HostKernel`], which takes the same kernel lock
//! (`KLock`) every other path through the shim takes. So signals, the
//! dispatch-miss path, the devices and the NT layer can never hold two views
//! of it.
//!
//! Only the shared-memory build runs one: a Windows program blocks inside
//! its syscalls (an NT wait parks the calling thread on a condition
//! variable), which takes a Worker per thread.

use gex_kernel::nt;

#[cfg(feature = "shared-memory")]
pub(crate) use imp::*;
#[cfg(not(feature = "shared-memory"))]
pub(crate) use stub::*;

/// The NT layer's PE ABI for an executable's bytes, if this shim runs it.
pub(crate) fn exe_abi(bytes: &[u8]) -> Option<nt::Abi> {
    if !gex_abi::X86 {
        return None;
    }
    nt::boot::exe_abi(bytes)
}

/// What `gex_load` places for a Windows program: nothing. Wine's loader maps
/// every image itself, so all code arrives through the runtime translator.
pub(crate) fn empty_program(abi: nt::Abi) -> gex_ld::Program {
    let mut exe = gex_ld::elf::headers_only(0, 0, 0, 0);
    // A PE32 program's code is decoded in i386 compatibility mode.
    exe.class32 = abi == nt::Abi::P32;
    gex_ld::Program { exe, interp: None }
}

#[cfg(not(feature = "shared-memory"))]
mod stub {
    use gex_kernel::nt;
    use gex_kernel::Kernel;

    pub(crate) fn active() -> bool {
        false
    }

    pub(crate) fn process() -> Option<&'static std::sync::Arc<nt::Process>> {
        None
    }

    pub(crate) fn boot(_k: &mut Kernel, _abi: nt::Abi, _args: &[String], _env: &[String]) -> Result<u64, String> {
        Err("a Windows program needs the shared-memory build of the shim (a thread is a Worker)".into())
    }

    pub(crate) fn syscall(_p: &std::sync::Arc<nt::Process>, _ctx: i32, _pc: i64) {
        unreachable!()
    }

    pub(crate) fn fault(_ctx: i32, _f: nt::Fault, _pc: u64) -> ! {
        unreachable!()
    }

    pub(crate) fn tick() {}

    pub(crate) fn arena_ceiling() -> u64 {
        gex_abi::MMAP_TOP
    }

    /// The glue's nested callback loop asks this; with no Windows process
    /// that loop never runs, but the export has to exist for the glue check.
    #[no_mangle]
    pub extern "C" fn gex_nt_callback_done(_ctx: i32) -> i32 {
        0
    }
}

#[cfg(feature = "shared-memory")]
mod imp {
    use crate::{
        backfill_registry, cur, exit_process, grow_memory_to, guest_bytes, set_resume, set_resume_ctx,
        set_resume_none, stage_thread, trap, warn_line, with_host, KLock, Thread, Waiting, ARENA_NEXT, HOST,
    };
    use gex_abi::ctx;
    use gex_kernel::nt;
    use gex_kernel::{GuestMem, Kernel, ThreadState};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, OnceLock};

    #[link(wasm_import_module = "gex_env")]
    extern "C" {
        /// Run the calling thread's guest code from `pc` in a nested loop
        /// until its `NtCallbackReturn` (docs/nt-layer.md §5). One when the
        /// callback returned, zero when the run failed or the process ended.
        fn nt_callback(ctx: i32, pc: i64) -> i32;
        /// Monotonic nanoseconds (`performance.now()`): the page's `now` is
        /// `Date.now()`, a millisecond clock that can step.
        fn mono_ns() -> i64;
        /// The compositor's scene for this process (see `crate::wl`).
        fn wl_publish(ptr: u32, count: u32);
        /// This process's compositor input ring (see `crate::wl`).
        fn wl_poll_input(ptr: u32, max: u32) -> u32;
    }

    /// The process, once booted. In the shim's statics, which every Worker
    /// of the process shares (the memory is a `SharedArrayBuffer`).
    static PROCESS: OnceLock<Arc<nt::Process>> = OnceLock::new();

    pub(crate) fn process() -> Option<&'static Arc<nt::Process>> {
        PROCESS.get()
    }

    pub(crate) fn active() -> bool {
        PROCESS.get().is_some()
    }

    /// Where the shim's heap may climb to. A Linux process's arena ends at
    /// `MMAP_TOP` (the dispatch directory's reach); an NT process owns
    /// everything below 2 GiB, so its heap lives above it.
    static ARENA_CEILING: AtomicU64 = AtomicU64::new(gex_abi::MMAP_TOP);

    pub(crate) fn arena_ceiling() -> u64 {
        ARENA_CEILING.load(Ordering::Relaxed)
    }

    /// The bottom of the shim's heap in an NT process: the top of the
    /// Windows user address space and above (see `nt::vm::ALLOC_HIGH`).
    const NT_HEAP_BASE: u64 = 0x8000_0000;

    /// The process's kernel, as the NT layer takes it: the one in `Host`,
    /// under `KLock`.
    struct HostKernel;

    struct Held {
        _lock: KLock,
        k: &'static mut Kernel,
    }

    impl std::ops::Deref for Held {
        type Target = Kernel;
        fn deref(&self) -> &Kernel {
            self.k
        }
    }

    impl std::ops::DerefMut for Held {
        fn deref_mut(&mut self) -> &mut Kernel {
            self.k
        }
    }

    impl nt::KernelCell for HostKernel {
        fn lock_kernel(&self) -> nt::KernelGuard<'_> {
            let lock = KLock::take();
            // SAFETY: as `with_host`: `KLock` makes this Worker the only one
            // inside the host until the guard drops, and the NT layer never
            // holds the kernel across a call that takes it again (it would
            // deadlock natively, where the lock is a plain `Mutex`).
            let k = unsafe { &mut (*core::ptr::addr_of_mut!(HOST)).as_mut().expect("gex: not initialised").kernel };
            nt::KernelGuard::new(Box::new(Held { _lock: lock, k }))
        }
    }

    /// `GEX_NT_TIME_SCALE`, as natively.
    static TIME_SCALE: AtomicU64 = AtomicU64::new(0);
    static MONO_START: AtomicU64 = AtomicU64::new(0);

    fn mono() -> u64 {
        let now = unsafe { mono_ns() } as u64;
        let start = match MONO_START.load(Ordering::Relaxed) {
            0 => {
                let _ = MONO_START.compare_exchange(0, now.max(1), Ordering::Relaxed, Ordering::Relaxed);
                MONO_START.load(Ordering::Relaxed)
            }
            s => s,
        };
        let real = now.saturating_sub(start);
        let bits = TIME_SCALE.load(Ordering::Relaxed);
        if bits == 0 {
            real
        } else {
            (real as f64 * f64::from_bits(bits)) as u64
        }
    }

    fn clocks() -> nt::Clocks {
        nt::Clocks { mono_ns: mono, wall: crate::browser_clock }
    }

    fn log(s: &str) {
        warn_line(&format!("{s}\n"));
    }

    /// Build the NT process in place of the ELF placement: the address space
    /// split with the shim, the process, and everything up to the first
    /// instruction of `LdrInitializeThunk`. Returns the main thread's pc (its
    /// registers are already in context slot 0).
    ///
    /// `env` is the page's environment list: `GEX_WINE_PREFIX` (default
    /// `/wine`), `GEX_NT_TRACE`, `GEX_NT_STRICT` and `GEX_NT_TIME_SCALE` are
    /// for this layer, and everything else overrides the Windows environment.
    pub(crate) fn boot(k: &mut Kernel, abi: nt::Abi, args: &[String], env: &[String]) -> Result<u64, String> {
        nt::set_log(log);
        // The address space. Windows' is everything below 2 GiB, and the
        // fixed parts of it (KUSER_SHARED_DATA at 0x7ffe0000, the PEB and
        // TEBs top-down) reach its top at once, so the memory is grown to all
        // of it now rather than allocation by allocation.
        if !grow_memory_to(NT_HEAP_BASE) {
            return Err("the engine will not grow this memory to 2 GiB, which a Windows process needs".into());
        }
        // The shim's own stack and data sit at fixed addresses inside it, and
        // so does whatever its heap has taken so far: those are kept out of
        // the NT allocator for good, and from here on the heap climbs above
        // 2 GiB, where no Windows address is.
        ARENA_CEILING.store(gex_abi::GUEST_MEM_BYTES, Ordering::Relaxed);
        let taken = ARENA_NEXT.fetch_max(NT_HEAP_BASE, Ordering::AcqRel);
        let host_lo = gex_abi::STACK_TOP - gex_abi::STACK_SIZE;
        let host_hi = taken.max(gex_abi::ARENA_BOTTOM).min(NT_HEAP_BASE);

        let var = |name: &str| {
            env.iter().find_map(|kv| kv.strip_prefix(name).and_then(|r| r.strip_prefix('=')).map(str::to_string))
        };
        let flag = |name: &str| var(name).is_some_and(|v| !v.is_empty() && v != "0");
        if let Some(s) = var("GEX_NT_TIME_SCALE").and_then(|v| v.parse::<f64>().ok()).filter(|v| *v > 0.0) {
            TIME_SCALE.store(s.to_bits(), Ordering::Relaxed);
        }
        let prefix = var("GEX_WINE_PREFIX").unwrap_or_else(|| "/wine".into());
        let mut wenv = nt::boot::default_environment();
        for kv in env {
            let Some((name, v)) = kv.split_once('=') else { continue };
            if name.starts_with("GEX_") {
                continue;
            }
            match wenv.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
                Some(e) => e.1 = v.to_string(),
                None => wenv.push((name.to_string(), v.to_string())),
            }
        }
        let exe_path = k.exe_path.clone();
        // The page's working directory if it named one, else the program's
        // own, which is where a Windows program usually expects to start.
        let cwd = match k.vfs.cwd.as_str() {
            "/" | "" => exe_path.rsplit_once('/').map(|(d, _)| if d.is_empty() { "/" } else { d }).unwrap_or("/").to_string(),
            c => c.to_string(),
        };
        let cfg = nt::Config {
            prefix: prefix.clone(),
            image_path: nt::boot::dos_path(&prefix, &exe_path),
            cmdline: String::new(),
            env: wenv,
            cwd: nt::boot::dos_path(&prefix, &cwd),
            trace: flag("GEX_NT_TRACE"),
            strict: flag("GEX_NT_STRICT"),
        };
        let p = Arc::new(nt::Process::new(abi, nt::thread::nt_pid(k.pid), cfg, clocks(), Arc::clone(&k.futexes)));
        p.reserve_host(host_lo, host_hi, "[gex-shim]");
        let req = nt::boot::BootRequest { exe_path: &exe_path, args: args.get(1..).unwrap_or(&[]) };
        let pc = nt::boot::boot(&p, k, guest_bytes(), &req).map_err(|e| format!("starting {exe_path}: {e}"))?;
        // The page's compositor, when it brought one: windows become its
        // toplevels and its input comes back as hardware input.
        if crate::wl::have_wl() {
            *p.display_host.lock().unwrap() = Some(Arc::new(WlDisplay::default()));
        }
        if PROCESS.set(p).is_err() {
            return Err("this Worker has already booted a Windows process".into());
        }
        Ok(pc)
    }

    /// The browser's display (docs/nt-layer.md §6a.1): every top-level
    /// window is a toplevel in the page's compositor, published as a scene
    /// record whose pixels the compositor reads in place out of the shared
    /// memory (the window surface is a 32 bpp BGRX bitmap, `wl_shm`'s
    /// XRGB8888). The compositor places and decorates the windows itself;
    /// input arrives in surface coordinates, which is what the NT side
    /// wants.
    #[derive(Default)]
    struct WlDisplay {
        windows: std::sync::Mutex<std::collections::BTreeMap<u32, (nt::display::NtSurface, u32)>>,
    }

    /// Words in a scene record and an input event: `REC` and `EV` in
    /// `crate::wl` and `wl-bridge.mjs`.
    const REC: usize = 28;
    const EV: usize = 8;
    /// The client number NT windows go by in this process's block. A Windows
    /// process has no Wayland clients of its own, so there is nobody to
    /// share it with.
    const NT_CLIENT: u32 = 1;
    /// `wl_shm`'s XRGB8888: B, G, R, X in memory order.
    const XRGB8888: i32 = 1;

    impl WlDisplay {
        fn flush(&self, windows: &std::collections::BTreeMap<u32, (nt::display::NtSurface, u32)>) {
            let mut scene: Vec<i32> = Vec::with_capacity(windows.len() * REC);
            for (s, serial) in windows.values().take(64) {
                let base = scene.len();
                scene.resize(base + REC, 0);
                let r = &mut scene[base..];
                r[0] = NT_CLIENT as i32;
                r[1] = s.id as i32;
                r[2] = 1; // toplevel
                r[4] = s.x;
                r[5] = s.y;
                r[6] = s.width;
                r[7] = s.height;
                r[8] = s.addr as u32 as i32;
                r[9] = s.stride as i32;
                r[10] = XRGB8888;
                r[11] = *serial as i32;
                let (l, t, rt, b) = s.damage;
                r[12] = l;
                r[13] = t;
                r[14] = rt - l;
                r[15] = b - t;
                let mut n = s.title.len().min(32);
                while n > 0 && !s.title.is_char_boundary(n) {
                    n -= 1;
                }
                for (i, chunk) in s.title.as_bytes()[..n].chunks(4).enumerate() {
                    let mut w = [0u8; 4];
                    w[..chunk.len()].copy_from_slice(chunk);
                    r[18 + i] = i32::from_le_bytes(w);
                }
            }
            // SAFETY: the import reads `count` records at the pointer and keeps
            // none of them.
            unsafe { wl_publish(scene.as_ptr() as u32, (scene.len() / REC) as u32) };
        }
    }

    impl nt::display::NtDisplay for WlDisplay {
        fn publish(&self, _mem: &GuestMem<'_>, surface: &nt::display::NtSurface) {
            let mut w = self.windows.lock().unwrap();
            let serial = w.get(&surface.id).map_or(1, |(_, n)| n.wrapping_add(1));
            w.insert(surface.id, (surface.clone(), serial));
            self.flush(&w);
        }

        fn remove(&self, id: u32) {
            let mut w = self.windows.lock().unwrap();
            if w.remove(&id).is_some() {
                self.flush(&w);
            }
        }

        fn poll_input(&self) -> Vec<nt::display::NtDisplayEvent> {
            use nt::display::NtDisplayEvent as E;
            let mut raw = [0i32; 64 * EV];
            // SAFETY: the import writes at most 64 records into the buffer.
            let n = (unsafe { wl_poll_input(raw.as_mut_ptr() as u32, 64) } as usize).min(64);
            let mut out = Vec::new();
            for q in raw[..n * EV].chunks(EV) {
                let (client, window) = (q[5] as u32, q[6] as u32);
                if client != NT_CLIENT {
                    continue;
                }
                out.push(match q[0] {
                    0 => E::Motion { window, x: q[1], y: q[2] },
                    1 => E::Button { window, button: q[1] as u32, pressed: q[2] != 0 },
                    // Wayland's ten units a wheel notch, positive downwards;
                    // Windows' 120 a notch, positive away from the user.
                    2 => E::Axis { window, axis: q[1] as u32, value: -q[2] * 12 },
                    3 => E::Key { window, code: q[1] as u32, pressed: q[2] != 0 },
                    6 if window != 0 => E::Focus { window },
                    // Only a real resize: the compositor configures on every
                    // focus change too, with the size the window already has.
                    7 => {
                        let same = self.windows.lock().unwrap().get(&window).is_some_and(|(s, _)| {
                            s.width == q[1] && s.height == q[2]
                        });
                        if same {
                            continue;
                        }
                        E::Configure { window, width: q[1], height: q[2] }
                    }
                    8 => E::Close { window },
                    _ => continue,
                });
            }
            out
        }
    }

    /// Bring `KUSER_SHARED_DATA`'s clocks up to date, at most once a
    /// millisecond. Natively a thread does this; here every NT syscall and
    /// every safepoint does, which is as often as anything can look.
    pub(crate) fn tick() {
        static LAST: AtomicU64 = AtomicU64::new(0);
        let Some(p) = PROCESS.get() else { return };
        let now = mono();
        let last = LAST.load(Ordering::Relaxed);
        if now.wrapping_sub(last) < 1_000_000 && last != 0 {
            return;
        }
        if LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            nt::sysinfo::usd_tick(guest_bytes(), &p.clocks);
        }
    }

    /// Runs user-mode callbacks: the JS side's nested loop on this Worker.
    struct Nested {
        ctx_addr: u64,
    }

    impl nt::NtHost for Nested {
        fn run_callback(&mut self) -> Result<(), ()> {
            let pc = GuestMem::new(guest_bytes()).read_u64(self.ctx_addr + ctx::PC as u64).unwrap_or(0);
            // SAFETY: a host import taking scalars. Nothing here holds the
            // kernel: the NT layer lets go before it runs a callback.
            match unsafe { nt_callback(self.ctx_addr as u32 as i32, pc as i64) } {
                1 => Ok(()),
                _ => Err(()),
            }
        }
    }

    /// Whether the thread at `ctx` has issued its `NtCallbackReturn`: what
    /// the nested loop asks after each trap.
    #[no_mangle]
    pub extern "C" fn gex_nt_callback_done(ctx: i32) -> i32 {
        let Some(p) = PROCESS.get() else { return 0 };
        p.thread_by_ctx(ctx as u32 as u64).is_some_and(|t| t.callback_result.lock().unwrap().is_some()) as i32
    }

    /// Whether this process is a Windows one, for the glue.
    #[no_mangle]
    pub extern "C" fn gex_nt_active() -> i32 {
        active() as i32
    }

    /// The `syscall` import for an NT process. Returns when the guest carries
    /// straight on; anything else traps back to JS (the outer run loop, or a
    /// nested callback loop) with the resume address set.
    pub(crate) fn syscall(p: &Arc<nt::Process>, ctx: i32, pc: i64) {
        let ctx_addr = ctx as u32 as u64;
        tick();
        let outcome = {
            let mut host = Nested { ctx_addr };
            nt::syscall(p, &HostKernel, guest_bytes(), ctx_addr, pc as u64, &mut host)
        };
        // And after: a wait that slept leaves the clocks behind, and the
        // program reads them next without a syscall (GetTickCount).
        tick();
        // Code that went away (an unmapped image, a protection change: LuaJIT
        // rewrites its machine code in place) loses its translations, as
        // natively. NT images are never statically recompiled, so newly mapped
        // code needs nothing.
        with_host(|h| {
            let retired = std::mem::take(&mut h.kernel.retired_code);
            h.kernel.new_code.clear();
            if !retired.is_empty() {
                retire(h, &retired);
            }
        });
        let resume_pc = || GuestMem::new(guest_bytes()).read_u64(ctx_addr + ctx::PC as u64).unwrap_or(0);
        match outcome {
            nt::Outcome::Continue => {}
            nt::Outcome::Resume(target) => {
                set_resume(target);
                set_resume_ctx(ctx_addr);
                trap();
            }
            nt::Outcome::Exit(code) => end_process(code),
            nt::Outcome::ThreadExit => {
                // The main thread's Worker is the process's: when it ends
                // first it waits for the process to end and reports the
                // process's exit code, as Windows does.
                if ctx_addr == gex_abi::ctx_addr(0) {
                    while !p.terminating.load(Ordering::Acquire) {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    end_process(p.exit_code.load(Ordering::Acquire));
                }
                with_host(|h| h.threads[cur()].waiting = Waiting::Exited);
                set_resume_none();
                set_resume_ctx_none_and_trap();
            }
            nt::Outcome::SpawnThread { ctx_addr: child, gex_tid } => {
                let start = GuestMem::new(guest_bytes()).read_u64(child + ctx::PC as u64).unwrap_or(0);
                with_host(|h| {
                    // A Worker of its own, as a Linux clone gets: the entry
                    // is this process's, the thread runs elsewhere.
                    h.threads.push(Thread {
                        kstate: ThreadState { ctx_addr: child, tid: gex_tid, clear_child_tid: 0 },
                        pc: start,
                        waiting: Waiting::Elsewhere,
                        blocked_on: None,
                    });
                    if !h.jit.ever_threaded {
                        backfill_registry(h);
                        h.jit.ever_threaded = true;
                    }
                    stage_thread(gex_tid, child, start);
                });
                set_resume(resume_pc());
                set_resume_ctx(ctx_addr);
                trap();
            }
            // The result is with the thread; trapping here ends the innermost
            // nested loop, which asks `gex_nt_callback_done`.
            nt::Outcome::CallbackReturn => trap(),
        }
    }

    /// Forget every translation in `ranges`, the browser's
    /// `retire_translations`: the pages leave the shared page table and their
    /// directory entries go back to the miss stub, so the next branch there
    /// (from any Worker) lifts what is there now into fresh slots. The old
    /// slots are left behind, unreachable.
    fn retire(h: &mut crate::Host, ranges: &[(u64, u64)]) {
        use std::sync::atomic::AtomicU32;
        for &(start, end) in ranges {
            if end <= start {
                continue;
            }
            h.jit.translated.retain(|a| *a < start || *a >= end);
            for page in start / gex_abi::DISPATCH_PAGE..=(end - 1) / gex_abi::DISPATCH_PAGE {
                if page >= gex_abi::DISPATCH_MAX_PAGES {
                    break;
                }
                h.jit.page_table.forget(page);
                let at = gex_abi::DISPATCH_DIR + page * 4;
                // SAFETY: the directory is in the shared memory, four-byte
                // aligned, and every Worker's dispatch reads it as a word.
                unsafe { (*(at as usize as *const AtomicU32)).store(0, Ordering::Release) };
            }
        }
    }

    fn set_resume_ctx_none_and_trap() -> ! {
        crate::set_resume_ctx_none();
        trap()
    }

    /// End the process with NT exit code `code`. The low byte is what a
    /// parent sees, as natively.
    fn end_process(code: u32) -> ! {
        with_host(|h| {
            exit_process(h, (code & 0xff) as i32);
        });
        trap()
    }

    /// Deliver a processor exception at `pc` to the guest, as Wine's signal
    /// handlers do: resume in `KiUserExceptionDispatcher`.
    pub(crate) fn fault(ctx: i32, fault: nt::Fault, pc: u64) -> ! {
        let p = PROCESS.get().expect("an NT fault in a Linux process");
        let Some((code, rip, info)) = fault.exception(pc) else {
            warn_line(&format!(
                "gex-nt: an unmasked x87 exception at {pc:#x}: gex does not deliver x87 exceptions; ending the process\n"
            ));
            with_host(|h| {
                exit_process(h, 128 + 8);
            });
            trap();
        };
        let ctx_addr = ctx as u32 as u64;
        match nt::hardware_exception(p, &HostKernel, guest_bytes(), ctx_addr, code, rip, &info) {
            nt::Outcome::Resume(target) => {
                set_resume(target);
                set_resume_ctx(ctx_addr);
                trap()
            }
            nt::Outcome::Exit(code) => end_process(code),
            other => {
                warn_line(&format!("gex-nt: unexpected outcome {other:?} from exception dispatch\n"));
                end_process(st_internal())
            }
        }
    }

    /// STATUS_INTERNAL_ERROR.
    fn st_internal() -> u32 {
        0xc000_00e5
    }
}
