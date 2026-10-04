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

    pub(crate) fn end_process(_code: u32) -> ! {
        unreachable!()
    }

    pub(crate) fn tick() {}
    pub(crate) fn serve_now() {}

    pub(crate) fn arena_ceiling() -> u64 {
        gex_abi::MMAP_TOP
    }

    pub(crate) fn slot_log() -> bool {
        false
    }

    pub(crate) fn describe(addr: u64) -> String {
        format!("{addr:#x}")
    }

    pub(crate) fn exc_escape(_ctx: i32, _target: u64) -> bool {
        false
    }

    /// The glue's nested callback loop asks this; with no Windows process
    /// that loop never runs, but the export has to exist for the glue check.
    #[no_mangle]
    pub extern "C" fn gex_nt_callback_done(_ctx: i32) -> i32 {
        0
    }

    /// No nested exception dispatch without a Windows process: never an
    /// escape. The glue calls it, so the export has to exist.
    #[no_mangle]
    pub extern "C" fn gex_exc_escaped(_clear: i32) -> i32 {
        -1
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
        /// Dispatch an exception nested (`raise_nested`): run the guest
        /// from `target` (KiUserExceptionDispatcher) in a loop on this
        /// Worker until control goes back above the raise (`exc_escape`
        /// for depth `level`): 1, or 0 when it could not.
        fn exc_dispatch(ctx: i32, target: i64, level: i32) -> i32;
        /// An AOT-only run (an ejected project's): serve what a syscall
        /// left for JS -- the units of images just mapped, a thread just
        /// created -- inside the call, so the guest's frames survive it
        /// (leaving would resume at continuations no unit has entries for).
        fn serve_in_place();
        /// Monotonic nanoseconds (`performance.now()`): the page's `now` is
        /// `Date.now()`, a millisecond clock that can step. An f64 (exact
        /// for 104 days): an i64 is a BigInt in JS, per call.
        fn mono_ns() -> f64;
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

    /// `performance.now()` in nanoseconds, unscaled: for timing the host
    /// (the glue's `now` is a BigInt, and only milliseconds in a Worker).
    fn mono_raw() -> u64 {
        unsafe { mono_ns() as u64 }
    }

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

    /// `GEX_NT_FIXED_TIME=<unix seconds>`, as natively (54cb7124): the
    /// guest's wall clock starts there and advances with real time, so a
    /// program seeding from the date (Noita's world) sees the same value
    /// every run. 0 for the page's own clock.
    static FIXED_TIME: AtomicU64 = AtomicU64::new(0);
    static FIXED_ORIGIN: AtomicU64 = AtomicU64::new(0);

    fn wall() -> (u64, u32) {
        let fixed = FIXED_TIME.load(Ordering::Relaxed);
        if fixed == 0 {
            return crate::browser_clock();
        }
        let now = unsafe { mono_ns() } as u64;
        let ns = fixed * 1_000_000_000 + now.saturating_sub(FIXED_ORIGIN.load(Ordering::Relaxed));
        (ns / 1_000_000_000, (ns % 1_000_000_000) as u32)
    }

    fn clocks() -> nt::Clocks {
        nt::Clocks { mono_ns: mono, wall }
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
        crate::memmap::claim(crate::memmap::Kind::Guest, 0, host_hi, NT_HEAP_BASE, "Windows address space");

        let var = |name: &str| {
            env.iter().find_map(|kv| kv.strip_prefix(name).and_then(|r| r.strip_prefix('=')).map(str::to_string))
        };
        let flag = |name: &str| var(name).is_some_and(|v| !v.is_empty() && v != "0");
        STATS.store(flag("GEX_NT_STATS"), Ordering::Relaxed);
        // GEX_NT_PEEK=<hex>[*]: a guest word reported on the stats line, read
        // through a pointer with `*` (as natively).
        if let Some(v) = var("GEX_NT_PEEK") {
            let (a, deref) = match v.strip_suffix('*') {
                Some(a) => (a, true),
                None => (v.as_str(), false),
            };
            if let Ok(a) = u64::from_str_radix(a.trim_start_matches("0x"), 16) {
                PEEK.store(a << 1 | deref as u64, Ordering::Relaxed);
            }
        }
        if flag("GEX_NT_MAP") || flag("GEX_NT_TRACE") {
            crate::memmap::print();
        }
        TRACE_FOREIGN.store(flag("GEX_NT_TRACE") || flag("GEX_NT_FOREIGN_TRACE"), Ordering::Relaxed);
        if flag("GEX_NT_ARENA_SELFTEST") {
            warn_line(&format!("gex-nt: arena self-check: {}\n", crate::arena_selftest()));
        }
        NO_RETIRE.store(flag("GEX_NT_NO_RETIRE"), Ordering::Relaxed);
        GL_READBACK.store(flag("GEX_NT_GL_READBACK"), Ordering::Relaxed);
        GL_STATS.store(flag("GEX_NT_GL_STATS"), Ordering::Relaxed);
        nt::opengl::STATS.store(flag("GEX_NT_GL_STATS"), Ordering::Relaxed);
        TRAP_PRINT.store(flag("GEX_NT_TRAP_PRINT"), Ordering::Relaxed);
        if let Some(n) = var("GEX_NT_BIG_ALLOC").and_then(|v| v.parse::<u64>().ok()) {
            crate::BIG_ALLOC.store(n, Ordering::Relaxed);
        }
        if let Some(a) = var("GEX_NT_WATCH_ALLOC").and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok()) {
            crate::WATCH_ALLOC.store(a, Ordering::Relaxed);
        }
        // A Windows process owns the low 2 GiB and the shim's heap is above
        // it, in the same memory: a guest access above is made a fault, as an
        // unallocated address is on Windows, rather than a write into the
        // runtime (natively it would land in nothing). GEX_NT_GUARD_2G=0 to
        // compare.
        if var("GEX_NT_GUARD_2G").as_deref() != Some("0") {
            gex_xlate::set_guard_limit(NT_HEAP_BASE);
        }
        SLOT_LOG.store(flag("GEX_NT_SLOT_LOG"), Ordering::Relaxed);
        VERIFY_REGISTRY.store(flag("GEX_NT_VERIFY_REGISTRY"), Ordering::Relaxed);
        RETIRE_LOG.store(flag("GEX_NT_RETIRE_LOG"), Ordering::Relaxed);
        // GEX_NT_TRAP_PCS=<hex>,<hex>: block entries that note the thread's
        // registers in its ring, for an execute fault's report.
        if let Some(v) = var("GEX_NT_TRAP_PCS") {
            for a in v.split(['+', ' ', ';']) {
                if let Ok(a) = u64::from_str_radix(a.trim().trim_start_matches("0x"), 16) {
                    gex_xlate::add_trap_pc(a);
                }
            }
        }
        if let Some(s) = var("GEX_NT_TIME_SCALE").and_then(|v| v.parse::<f64>().ok()).filter(|v| *v > 0.0) {
            TIME_SCALE.store(s.to_bits(), Ordering::Relaxed);
        }
        if var("GEX_NT_EXC_TRACE").is_some() {
            crate::gex_set_exc_trace(1);
        }
        if var("GEX_NT_EXC_NEST").as_deref() == Some("0") {
            NO_EXC_NEST.store(true, Ordering::Relaxed);
        }
        if let Some(t) = var("GEX_NT_FIXED_TIME").and_then(|v| v.parse::<u64>().ok()).filter(|v| *v > 0) {
            FIXED_ORIGIN.store(unsafe { mono_ns() } as u64, Ordering::Relaxed);
            FIXED_TIME.store(t, Ordering::Relaxed);
        }
        // An ejected project's stateful providers (docs/eject-pe.md §5.1).
        crate::cgen::ENTRY_LOG.store(flag("GEX_NT_ENTRY_LOG"), Ordering::Relaxed);
        crate::RESUME_LOG.store(flag("GEX_NT_RESUME_LOG"), Ordering::Relaxed);
        if let Some(v) = var("GEX_NT_STATEFUL") {
            crate::cgen::set_stateful(&v);
        }
        let prefix = var("GEX_WINE_PREFIX").unwrap_or_else(|| "/wine".into());
        let mut wenv = nt::boot::default_environment();
        let mut settings = Vec::new();
        for kv in env {
            let Some((name, v)) = kv.split_once('=') else { continue };
            if name.starts_with("GEX_") {
                settings.push((name.to_string(), v.to_string()));
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
            settings,
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
        /// Windows whose GL frames the compositor draws itself, at this
        /// rectangle of the window's surface (`gl_target`).
        gl: std::sync::Mutex<std::collections::BTreeMap<u32, (i32, i32, i32, i32)>>,
    }

    /// GEX_NT_GL_READBACK: read each GL frame back into the window's surface
    /// instead of handing it to the compositor (the old path, for comparing).
    static GL_READBACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// GEX_NT_GL_STATS: every ten seconds, the program's GL by function
    /// (`nt::opengl::take_stats`).
    static GL_STATS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
            let gl = self.gl.lock().unwrap().clone();
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
                // The host's GL frame, drawn by the compositor over this
                // part of the surface (`F_EXTERNAL`, `R_GL_RECT` in
                // wl-bridge.mjs).
                if let Some(&(gx, gy, gw, gh)) = gl.get(&s.id) {
                    r[16] = 2;
                    r[26] = (gx & 0xffff) | (gy << 16);
                    r[27] = (gw & 0xffff) | (gh << 16);
                }
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
            self.gl.lock().unwrap().remove(&id);
            if w.remove(&id).is_some() {
                self.flush(&w);
            }
        }

        fn gl_target(&self, toplevel: u32, rect: (i32, i32, i32, i32)) -> Option<u32> {
            if GL_READBACK.load(Ordering::Relaxed) {
                return None;
            }
            let changed = self.gl.lock().unwrap().insert(toplevel, rect) != Some(rect);
            if changed {
                let w = self.windows.lock().unwrap();
                self.flush(&w);
            }
            Some(crate::my_pid().max(0) as u32)
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

    /// `GEX_NT_STATS`: once a (real) second, the native shim's
    /// `gex-nt: stats t=<ms> presents=<n>` line, on the console.
    static STATS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    /// `GEX_NT_PEEK`'s address shifted up one, with the dereference flag in
    /// bit 0; zero for none.
    static PEEK: AtomicU64 = AtomicU64::new(0);

    fn peek() -> Option<u32> {
        let v = PEEK.load(Ordering::Relaxed);
        if v == 0 {
            return None;
        }
        let m = GuestMem::new(guest_bytes());
        let mut a = v >> 1;
        if v & 1 != 0 {
            a = m.read_u32(a).ok()? as u64;
            if a == 0 {
                return None;
            }
        }
        m.read_u32(a).ok()
    }

    fn report_stats(p: &nt::Process) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let real = unsafe { mono_ns() } as u64 / 1_000_000;
        let start = MONO_START.load(Ordering::Relaxed) / 1_000_000;
        let t = real.saturating_sub(start);
        let next = NEXT.load(Ordering::Relaxed);
        if t < next || NEXT.compare_exchange(next, t / 1000 * 1000 + 1000, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return;
        }
        let presents = p.presents.load(Ordering::Relaxed);
        if t / 1000 % 10 == 0 {
            report_times();
            report_syscalls();
            // What every Worker's tables hold: `blocks` (dense) and the
            // slots each dispatch table is numbered to.
            let slots: Vec<String> = (0..gex_abi::DISPATCH_TABLES as u32).map(|k| crate::gex_claimed_slots(k).to_string()).collect();
            let (callouts, left) = crate::cgen::callouts();
            warn_line(&format!(
                "gex-nt: tables blocks={} slots={} dense={} callouts={callouts} callouts_left={left} in_place={} exc_nested={} exc_escaped={} fjit={:?}\n",
                crate::dense::gex_dense_size(),
                slots.join("/"),
                crate::dense::on() as u8,
                crate::IN_PLACE_N.load(Ordering::Relaxed),
                EXC_NESTED.load(Ordering::Relaxed),
                EXC_ESCAPED.load(Ordering::Relaxed),
                crate::fjit::counts()
            ));
            if GL_STATS.load(Ordering::Relaxed) {
                warn_line(&format!("gex-nt: gl {}\n", nt::opengl::take_stats(p, 14)));
            }
        }
        if VERIFY_REGISTRY.load(Ordering::Relaxed) {
            if let Some(what) = with_host(crate::verify_registry) {
                warn_line(&format!("gex-nt: a translation was overwritten after it was kept: {what}\n"));
            }
        }
        if VERIFY_REGISTRY.load(Ordering::Relaxed) {
            warn_line(&format!("gex-nt: stats worker stack least headroom {} bytes\n", crate::stack_least()));
        }
        let foreign = crate::FOREIGN_BYTES.load(Ordering::Relaxed);
        let fx = if foreign != 0 { format!(" foreign={foreign}") } else { String::new() };
        match peek() {
            Some(v) => warn_line(&format!("gex-nt: stats t={t} presents={presents} peek={v}{fx}\n")),
            None => warn_line(&format!("gex-nt: stats t={t} presents={presents}{fx}\n")),
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
            if STATS.load(Ordering::Relaxed) {
                report_stats(p);
            }
        }
    }

    /// Exceptions dispatched nested and escaped back (the stats line).
    pub(crate) static EXC_NESTED: AtomicU64 = AtomicU64::new(0);
    pub(crate) static EXC_ESCAPED: AtomicU64 = AtomicU64::new(0);

    /// How deep exceptions nest before raising traps out as before.
    const EXC_MAX_DEPTH: usize = 8;

    thread_local! {
        /// This Worker's nested exception dispatches: the stack each was
        /// raised on, and the NT callbacks in progress then.
        static EXC: std::cell::RefCell<Vec<(u64, u32)>> = const { std::cell::RefCell::new(Vec::new()) };
        /// The dispatch whose escape was found (`exc_escape`), or -1.
        static ESCAPE: std::cell::Cell<i32> = const { std::cell::Cell::new(-1) };
    }

    /// Whether an exception the syscall returning to `next_pc` raises is
    /// dispatched nested, as natively: with function bodies about (units on),
    /// when the syscall returns through a `ret`. `GEX_NT_EXC_NEST=0` (a page
    /// setting) turns it off.
    fn nest_exceptions(next_pc: u64) -> bool {
        if !crate::cgen::on() || NO_EXC_NEST.load(Ordering::Relaxed) {
            return false;
        }
        matches!(guest_bytes().get(next_pc as usize), Some(0xc3) | Some(0xc2))
    }

    static NO_EXC_NEST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// Has control left the innermost nested exception dispatch on this
    /// Worker for `target`? It has when the stack is above the raise (a
    /// handler's longjmp, a catch continuation, an NtContinue) and no NT
    /// callback begun inside the dispatch is still in progress. Then
    /// `ctx::PC` = `target` and the caller traps: the dispatch's loop ends.
    /// Called where the context is complete: the fallback, a dispatch miss,
    /// a safepoint, a syscall's resume.
    pub(crate) fn exc_escape(ctx: i32, target: u64) -> bool {
        let Some((raise_sp, cb, level)) = EXC.with(|e| e.borrow().last().map(|&(s, c)| (s, c, e.borrow().len() - 1))) else { return false };
        if cb != crate::cgen::callback_depth() {
            return false;
        }
        let c = ctx as u32 as u64;
        let mut gm = GuestMem::new(guest_bytes());
        let p32 = PROCESS.get().is_some_and(|p| p.abi == nt::Abi::P32);
        let sp = gm.read_u64(c + ctx::SP as u64).unwrap_or(0);
        let sp = if p32 { sp & 0xffff_ffff } else { sp };
        // At or above the syscall's own stack: control is back with the
        // frames the dispatch was entered from (an APC's NtContinue comes
        // back to exactly it), as natively (fb7dfd52).
        if sp < raise_sp {
            return false;
        }
        let _ = gm.write_u64(c + ctx::PC as u64, target);
        ESCAPE.with(|e| e.set(level as i32));
        true
    }

    /// The nested dispatch an escape was found for (its depth), or -1;
    /// reading it clears it (`clear` = 0 only looks).
    #[no_mangle]
    pub extern "C" fn gex_exc_escaped(clear: i32) -> i32 {
        ESCAPE.with(|e| if clear != 0 { e.replace(-1) } else { e.get() })
    }

    /// A nested dispatch at depth `level` (and deeper) was left by an
    /// exception through JS's loop: its entries go.
    #[no_mangle]
    pub extern "C" fn gex_exc_unwind(level: u32) {
        let gone = EXC.with(|e| {
            let mut e = e.borrow_mut();
            let n = e.len().saturating_sub(level as usize);
            e.truncate(level as usize);
            n
        });
        for _ in 0..gone {
            crate::cgen::callback(false);
        }
    }

    /// Dispatch an exception without unwinding the thread's wasm frames
    /// (`NtRaiseException`, first chance), as natively (ffa2cec9): run
    /// `KiUserExceptionDispatcher` (`target`) in a nested loop on top of them
    /// until control goes back above the raise; then the syscall returns,
    /// its `ret` taking the guest to where the dispatch ended (the return
    /// address it pops is written just below the new stack). False when it
    /// cannot (too deep, the loop failed): trap out as before.
    fn raise_nested(p: &Arc<nt::Process>, ctx_addr: u64, target: u64, raise_sp: u64, next_pc: u64) -> bool {
        let p32 = p.abi == nt::Abi::P32;
        let mask = if p32 { 0xffff_ffff } else { !0u64 };
        let level = EXC.with(|e| e.borrow().len());
        if level >= EXC_MAX_DEPTH {
            return false;
        }
        EXC_NESTED.fetch_add(1, Ordering::Relaxed);
        // A barrier for call-outs, as a callback is: one begun outside cannot
        // end inside (its caller's frame is below this loop's).
        crate::cgen::callback(true);
        let cb = crate::cgen::callback_depth();
        EXC.with(|e| e.borrow_mut().push((raise_sp & mask, cb)));
        // SAFETY: a host import taking scalars; nothing here holds a lock.
        let r = unsafe { exc_dispatch(ctx_addr as u32 as i32, target as i64, level as i32) };
        // JS's loop unwinds its level on the way out; again here, harmlessly,
        // for a host that did not.
        gex_exc_unwind(level as u32);
        if r != 1 {
            return false;
        }
        EXC_ESCAPED.fetch_add(1, Ordering::Relaxed);
        let mut gm = GuestMem::new(guest_bytes());
        let to_pc = gm.read_u64(ctx_addr + ctx::PC as u64).unwrap_or(0);
        let to_sp = gm.read_u64(ctx_addr + ctx::SP as u64).unwrap_or(0) & mask;
        // Back at the syscall's own return (an APC's NtContinue, an
        // exception continued at the raise): just return, with the
        // context's registers.
        if to_pc == next_pc && to_sp == raise_sp & mask {
            let _ = gm.write_u64(ctx_addr + ctx::PC as u64, next_pc);
            return true;
        }
        let pops = match guest_bytes().get(next_pc as usize) {
            Some(0xc2) => gm.read_u32(next_pc + 1).unwrap_or(0) as u64 & 0xffff,
            _ => 0,
        };
        let psize = if p32 { 4 } else { 8 };
        let sp = to_sp.wrapping_sub(psize + pops) & mask;
        let wrote = if p32 { gm.write_u32(sp, to_pc as u32) } else { gm.write_u64(sp, to_pc) };
        if wrote.is_err() {
            // No stack there: resume the old way.
            set_resume(to_pc);
            set_resume_ctx(ctx_addr);
            trap();
        }
        let _ = gm.write_u64(ctx_addr + ctx::SP as u64, sp);
        let _ = gm.write_u64(ctx_addr + ctx::PC as u64, next_pc);
        true
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
            crate::cgen::callback(true);
            let r = unsafe { nt_callback(self.ctx_addr as u32 as i32, pc as i64) };
            crate::cgen::callback(false);
            match r {
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

    /// Which NT regions overlap `[addr, addr+len)`, on the console: for a
    /// translation the engine refused, whose bytes something overwrote.
    #[no_mangle]
    pub extern "C" fn gex_nt_where(addr: u32, len: u32) {
        let Some(p) = PROCESS.get() else { return };
        let (a, e) = (addr as u64, addr as u64 + len as u64);
        let st = p.st.lock().unwrap();
        let hits: Vec<String> = st
            .vm
            .regions
            .range(..e)
            .filter(|(_, r)| r.end() > a)
            .map(|(_, r)| format!("{:#x}+{:#x} {} prot {:#x}", r.base, r.size, r.name, r.alloc_prot))
            .collect();
        drop(st);
        warn_line(&format!(
            "gex-nt: {a:#x}..{e:#x} is {}, its end {}\n",
            crate::memmap::owner_of(a),
            crate::memmap::owner_of(e.saturating_sub(1))
        ));
        warn_line(&format!(
            "gex-nt: {a:#x}..{e:#x} (arena next {:#x}) overlaps NT regions: {}\n",
            ARENA_NEXT.load(Ordering::Relaxed),
            if hits.is_empty() { "none".into() } else { hits.join(", ") }
        ));
    }

    /// `addr` as the NT layer names it (`image+offset`), for a report.
    pub(crate) fn describe(addr: u64) -> String {
        match PROCESS.get() {
            Some(p) => p.describe_addr(addr),
            None => format!("{addr:#x}"),
        }
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
        // A clock read (`performance.now()`) costs about a microsecond in a
        // Worker, as much as a cheap syscall: the clocks are brought up to
        // date every 16th call and around waits, and the time split is
        // sampled likewise (Noita's main thread makes ~10^5 NtYieldExecution
        // a second).
        let n = SYSCALL_N.with(|c| {
            let n = c.get().wrapping_add(1);
            c.set(n);
            n
        });
        let sampled = n % SAMPLE == 0;
        if sampled {
            tick();
        }
        if let Some((lo, bad, least)) = crate::stack_guard_check() {
            warn_line(&format!(
                "gex-nt: BUG: this Worker's shim stack overran its bottom at {lo:#x}: {bad} guard bytes overwritten (least headroom seen at an NT call: {least} bytes; ctx {ctx_addr:#x})\n"
            ));
        }
        let nr = GuestMem::new(guest_bytes()).read_u64(ctx_addr + ctx::x(0) as u64).unwrap_or(0) as u32;
        if TRACE_FOREIGN.load(Ordering::Relaxed) {
            crate::foreign_now("before", &nt::syscall_name(nr));
        }
        let gm = GuestMem::new(guest_bytes());
        let sp0 = gm.read_u64(ctx_addr + ctx::SP as u64).unwrap_or(0) & 0xffff_ffff;
        let r0 = gm.read_u32(sp0).unwrap_or(0) as u64;
        let r1 = gm.read_u32(sp0 + 4).unwrap_or(0) as u64;
        ring_note(ctx_addr, (nr as u64) << 32 | sp0, r0 << 32 | r1);
        // Where the syscall returns to and the stack it was made on: what a
        // nested exception dispatch needs to come back here.
        let next_pc = gm.read_u64(ctx_addr + ctx::PC as u64).unwrap_or(0);
        let raise_sp = gm.read_u64(ctx_addr + ctx::SP as u64).unwrap_or(0);
        // Waits other than a yield are timed every time (they are few and
        // long), everything else every SAMPLE-th call, counted SAMPLE times.
        let wait = is_wait(nr);
        let timed = sampled || (wait && nr != nt::sysno::ntdll::NtYieldExecution);
        let scale = if timed && !sampled { 1 } else { SAMPLE };
        let (t_in, gl_in, glw_in) = (
            if timed { mono_raw() } else { 0 },
            crate::gl::GL_NS.with(|g| g.get()),
            crate::gl::GL_WAIT_NS.with(|g| g.get()),
        );
        let outcome = {
            let mut host = Nested { ctx_addr };
            nt::syscall(p, &HostKernel, guest_bytes(), ctx_addr, pc as u64, &mut host)
        };
        if TRACE_FOREIGN.load(Ordering::Relaxed) {
            crate::foreign_now("during", &nt::syscall_name(nr));
        }
        count_syscall(nr);
        {
            // GL time is the GL host's own clock, every call.
            let gl = crate::gl::GL_NS.with(|g| g.get()) - gl_in;
            let glw = (crate::gl::GL_WAIT_NS.with(|g| g.get()) - glw_in).min(gl);
            if timed {
                let all = mono_raw().saturating_sub(t_in);
                time_note(ctx_addr, if wait { 1 } else { 0 }, all.saturating_sub(gl) * scale);
            }
            time_note(ctx_addr, 2, gl - glw);
            time_note(ctx_addr, 4, glw);
        }
        // And after: a wait that slept leaves the clocks behind, and the
        // program reads them next without a syscall (GetTickCount).
        if timed {
            tick();
        }
        // Code that went away (an unmapped image, a protection change: LuaJIT
        // rewrites its machine code in place) loses its translations, as
        // natively. NT images are never statically recompiled, so newly mapped
        // code needs nothing.
        with_host(|h| {
            let retired = std::mem::take(&mut h.kernel.retired_code);
            h.kernel.new_code.clear();
            if !retired.is_empty() {
                note_retire(&retired);
                crate::cgen::on_retire(&retired);
                if !NO_RETIRE.load(Ordering::Relaxed) {
                    retire(h, &retired);
                }
            }
        });
        let resume_pc = || GuestMem::new(guest_bytes()).read_u64(ctx_addr + ctx::PC as u64).unwrap_or(0);
        match outcome {
            nt::Outcome::Continue => {
                // A call that returns where it was made leaves the stack
                // where it was: say so loudly when not.
                let sp1 = GuestMem::new(guest_bytes()).read_u64(ctx_addr + ctx::SP as u64).unwrap_or(0) & 0xffff_ffff;
                if sp1 != sp0 {
                    warn_line(&format!(
                        "gex-nt: {} moved the stack: sp {sp0:#x} -> {sp1:#x} (ctx {ctx_addr:#x})\n",
                        nt::syscall_name(nr)
                    ));
                }
                // An image mapped with a function-tier unit to fetch: JS
                // loads and installs it before the guest goes on, as the
                // native host does after the same syscall.
                if crate::cgen::drain() {
                    if crate::aot() {
                        // SAFETY: a host import without arguments.
                        unsafe { serve_in_place() };
                        return;
                    }
                    set_resume(resume_pc());
                    set_resume_ctx(ctx_addr);
                    trap();
                }
            }
            nt::Outcome::Resume(target) => {
                // NtContinue out of a nested dispatch: back where it was raised.
                if exc_escape(ctx, target) {
                    set_resume_none();
                    trap();
                }
                if nr == nt::sysno::ntdll::NtRaiseException && crate::tracing_exc() {
                    warn_line(&format!("[nt] NtRaiseException returning to {next_pc:#x} (byte {:#x}): nested {}\n", guest_bytes().get(next_pc as usize).copied().unwrap_or(0), nest_exceptions(next_pc)));
                }
                // An exception raised, or a user APC delivered by an
                // alertable wait (not NtContinue's own delivery): both run a
                // dispatcher that ends in NtContinue, nested (fb7dfd52's
                // browser half).
                let apc = target == p.ntdll.lock().unwrap().ki_user_apc_dispatcher
                    && nr != nt::sysno::ntdll::NtContinue
                    && nr != nt::sysno::ntdll::NtContinueEx;
                if (nr == nt::sysno::ntdll::NtRaiseException || apc) && nest_exceptions(next_pc) && raise_nested(p, ctx_addr, target, raise_sp, next_pc) {
                    return;
                }
                set_resume(target);
                set_resume_ctx(ctx_addr);
                trap();
            }
            nt::Outcome::Exit(code) => {
                static ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                if code == 0xc000_0002 && !ONCE.swap(true, Ordering::Relaxed) {
                    // GEX_NT_STRICT's end: what the thread that hit it was
                    // doing (the first to end; the others follow it).
                    warn_line(&format!("gex-nt: this thread's last calls: {}\n", ring_report(ctx_addr)));
                    warn_line(&format!("gex-nt: this thread's last trap pcs: {}\n", trap_report(ctx_addr)));
                }
                end_process(code)
            }
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
                if crate::aot() {
                    crate::cgen::drain();
                    // SAFETY: a host import without arguments.
                    unsafe { serve_in_place() };
                    return;
                }
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
    /// GEX_NT_NO_RETIRE: keep translations of code that went away (a
    /// diagnostic: stale code runs).
    static NO_RETIRE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// GEX_NT_TRACE: look for memory grown outside the arena around every NT
    /// call, so the first foreign grow is caught near whoever did it.
    static TRACE_FOREIGN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// GEX_NT_VERIFY_REGISTRY: once a second, check every kept translation
    /// against the hashes taken when it was kept (a stray write into the
    /// shim's heap).
    static VERIFY_REGISTRY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// GEX_NT_SLOT_LOG: name the first few hundred slot conflicts (a block
    /// entered for a neighbour sharing its dispatch slot).
    static SLOT_LOG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    pub(crate) fn slot_log() -> bool {
        SLOT_LOG.load(Ordering::Relaxed)
    }

    /// GEX_NT_RETIRE_LOG: name each retired range, the first few hundred.
    static RETIRE_LOG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn note_retire(ranges: &[(u64, u64)]) {
        static N: AtomicU64 = AtomicU64::new(0);
        if !RETIRE_LOG.load(Ordering::Relaxed) || N.fetch_add(1, Ordering::Relaxed) > 400 {
            return;
        }
        let Some(p) = PROCESS.get() else { return };
        let s: Vec<String> = ranges.iter().map(|&(a, b)| format!("{}..{b:#x}", p.describe_addr(a))).collect();
        warn_line(&format!("gex-nt: retiring {}\n", s.join(" ")));
    }

    fn retire(h: &mut crate::Host, ranges: &[(u64, u64)]) {
        crate::retire_code(h, ranges);
    }

    /// Each thread's last NT calls and where they were made, for an execute
    /// fault's report: a fixed table keyed by context address, with no
    /// allocation (a thread-local `VecDeque` here was followed by translations
    /// arriving overwritten -- not understood; see MERGE-STATUS S12).
    const RING: usize = 32;
    struct Ring {
        ctx: AtomicU64,
        next: AtomicU64,
        calls: [[AtomicU64; 2]; RING],
    }
    static RINGS: [Ring; 64] = [const {
        Ring { ctx: AtomicU64::new(0), next: AtomicU64::new(0), calls: [const { [const { AtomicU64::new(0) }; 2] }; RING] }
    }; 64];

    fn ring_of(ctx: u64) -> &'static Ring {
        &RINGS[(ctx as usize / 0x80) % RINGS.len()]
    }

    /// `a` is the call number and the stack pointer, `b` the two words on
    /// top of the stack (the trampoline's return address and the stub's).
    fn ring_note(ctx: u64, a: u64, b: u64) {
        let r = ring_of(ctx);
        if r.ctx.swap(ctx, Ordering::Relaxed) != ctx {
            r.next.store(0, Ordering::Relaxed);
        }
        let i = r.next.fetch_add(1, Ordering::Relaxed);
        let e = &r.calls[(i % RING as u64) as usize];
        e[0].store(a, Ordering::Relaxed);
        e[1].store(b, Ordering::Relaxed);
    }

    /// GEX_NT_STATS' per-thread time split: nanoseconds in NT calls (0),
    /// in waits (1), waiting on the page's GL host (2), in the glue's run
    /// loop between traps (3), and waiting for the previous unwaited GL batch
    /// (4, `GEX_GL_SUBMIT_ASYNC`: the frame before's Mesa time the guest did
    /// not hide); and, per thread, the totals as of the last report
    /// (KINDS..2*KINDS) and when that was (2*KINDS). Keyed by context, like
    /// the rings.
    const KINDS: usize = 5;
    struct Times {
        ctx: AtomicU64,
        v: [AtomicU64; 2 * KINDS + 1],
    }
    static TIMES: [Times; 64] = [const { Times { ctx: AtomicU64::new(0), v: [const { AtomicU64::new(0) }; 2 * KINDS + 1] } }; 64];

    fn times_of(ctx: u64) -> &'static Times {
        let t = &TIMES[(ctx as usize / 0x80) % TIMES.len()];
        if t.ctx.swap(ctx, Ordering::Relaxed) != ctx {
            for v in &t.v {
                v.store(0, Ordering::Relaxed);
            }
            t.v[2 * KINDS].store(crate::now_ns(), Ordering::Relaxed);
        }
        t
    }

    /// Every how many NT syscalls a thread reads the clock (see `syscall`).
    const SAMPLE: u64 = 16;

    thread_local! {
        static SYSCALL_N: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    /// Whether syscall `nr` waits (its time is a wait, not NT work): by
    /// name, worked out once per number rather than per call.
    fn is_wait(nr: u32) -> bool {
        static WAITS: OnceLock<Vec<bool>> = OnceLock::new();
        let w = WAITS.get_or_init(|| {
            (0..0x2000u32)
                .map(|n| {
                    let name = nt::syscall_name(n);
                    name.contains("Wait") || name.contains("Delay") || name.contains("Remove") || name.contains("Yield")
                })
                .collect()
        });
        w.get(nr as usize).copied().unwrap_or(false)
    }

    /// NT syscalls by number since the last report (GEX_NT_STATS' `calls`
    /// line): which calls a frame makes.
    static SYSCALLS: [AtomicU64; 0x2000] = [const { AtomicU64::new(0) }; 0x2000];

    fn count_syscall(nr: u32) {
        if let Some(c) = SYSCALLS.get(nr as usize) {
            c.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The busiest syscalls since the last call, as `name n` pairs.
    fn report_syscalls() {
        let mut v: Vec<(u64, u32)> =
            (0..SYSCALLS.len()).map(|i| (SYSCALLS[i].swap(0, Ordering::Relaxed), i as u32)).filter(|x| x.0 > 0).collect();
        if v.is_empty() {
            return;
        }
        let total: u64 = v.iter().map(|x| x.0).sum();
        v.sort_unstable_by(|a, b| b.cmp(a));
        let top: Vec<String> = v.iter().take(10).map(|(n, nr)| format!("{} {n}", nt::syscall_name(*nr))).collect();
        warn_line(&format!("gex-nt: calls {total}: {}\n", top.join(", ")));
    }

    fn time_note(ctx: u64, what: usize, ns: u64) {
        times_of(ctx).v[what].fetch_add(ns, Ordering::Relaxed);
    }

    /// The glue's time between a trap and re-entering the guest, per thread.
    #[no_mangle]
    pub extern "C" fn gex_nt_note_js_ns(ctx: i32, ns: f64) {
        if ns > 0.0 {
            time_note(ctx as u32 as u64, 3, ns as u64);
        }
    }

    /// Every thread's split since its last report: one line each.
    fn report_times() {
        let now = crate::now_ns();
        for t in &TIMES {
            let ctx = t.ctx.load(Ordering::Relaxed);
            if ctx == 0 {
                continue;
            }
            let cur: Vec<u64> = (0..KINDS).map(|i| t.v[i].load(Ordering::Relaxed)).collect();
            let d: Vec<u64> =
                (0..KINDS).map(|i| cur[i].saturating_sub(t.v[KINDS + i].swap(cur[i], Ordering::Relaxed))).collect();
            let wall = now.saturating_sub(t.v[2 * KINDS].swap(now, Ordering::Relaxed));
            if wall == 0 || d.iter().sum::<u64>() == 0 {
                continue;
            }
            let ms = |v: u64| v / 1_000_000;
            let rest = wall.saturating_sub(d.iter().sum::<u64>());
            warn_line(&format!(
                "gex-nt: time ctx {ctx:#x} over {} ms: guest {} ms, NT calls {} ms, waits {} ms, GL host {} ms, GL previous frame {} ms, glue {} ms\n",
                ms(wall), ms(rest), ms(d[0]), ms(d[1]), ms(d[2]), ms(d[4]), ms(d[3])
            ));
        }
    }

    /// GEX_NT_TRAP_PCS' notes: per thread, the last few block entries with
    /// esp, ebp, eax, ecx, esi, edi and [esp].
    const TRING: usize = 16;
    struct TRing {
        ctx: AtomicU64,
        next: AtomicU64,
        e: [[AtomicU64; 4]; TRING],
    }
    static TRINGS: [TRing; 64] = [const {
        TRing { ctx: AtomicU64::new(0), next: AtomicU64::new(0), e: [const { [const { AtomicU64::new(0) }; 4] }; TRING] }
    }; 64];

    pub(crate) fn trap_note(ctx: u64, pc: u64) {
        let gm = GuestMem::new(guest_bytes());
        let r = |i: u8| gm.read_u64(ctx + ctx::x(i) as u64).unwrap_or(0) & 0xffff_ffff;
        let m = |a: u64| gm.read_u32(a).unwrap_or(0xdead_dead) as u64;
        let (sp, ecx) = (r(4), r(1));
        let vt = m(ecx);
        let t = &TRINGS[(ctx as usize / 0x80) % TRINGS.len()];
        if t.ctx.swap(ctx, Ordering::Relaxed) != ctx {
            t.next.store(0, Ordering::Relaxed);
        }
        let i = t.next.fetch_add(1, Ordering::Relaxed);
        let e = &t.e[(i % TRING as u64) as usize];
        e[0].store(pc << 32 | sp, Ordering::Relaxed);
        e[1].store(ecx << 32 | r(6), Ordering::Relaxed);
        let _ = vt;
        e[2].store(r(5) << 32 | r(0), Ordering::Relaxed);
        e[3].store(m(sp) << 32 | r(7), Ordering::Relaxed);
        static PRINTED: AtomicU64 = AtomicU64::new(0);
        if TRAP_PRINT.load(Ordering::Relaxed) && PRINTED.fetch_add(1, Ordering::Relaxed) < 200 {
            let q = |i: u8| gm.read_u64(ctx + ctx::x(i) as u64).unwrap_or(0);
            warn_line(&format!(
                "gex-nt: trap {} ctx {ctx:#x}: rax {:#x} rcx {:#x} rdx {:#x} rbx {:#x} rsp {:#x} rsi {:#x} rdi {:#x} r8 {:#x} [rsp] {:#x}\n",
                describe(pc), q(0), q(1), q(2), q(3), q(4), q(6), q(7), q(8), gm.read_u64(q(4)).unwrap_or(0)
            ));
        }
    }
    static TRAP_PRINT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn trap_report(ctx: u64) -> String {
        let t = &TRINGS[(ctx as usize / 0x80) % TRINGS.len()];
        if t.ctx.load(Ordering::Relaxed) != ctx {
            return "(none)".into();
        }
        let n = t.next.load(Ordering::Relaxed);
        let h = |v: u64| (v >> 32, v & 0xffff_ffff);
        (n.saturating_sub(TRING as u64)..n)
            .map(|i| {
                let e = &t.e[(i % TRING as u64) as usize];
                let (pc, sp) = h(e[0].load(Ordering::Relaxed));
                let (ecx, esi) = h(e[1].load(Ordering::Relaxed));
                let (ebp, eax) = h(e[2].load(Ordering::Relaxed));
                let (top, edi) = h(e[3].load(Ordering::Relaxed));
                format!("[{pc:#x} esp {sp:#x} ebp {ebp:#x} [esp] {top:#x} eax {eax:#x} ecx {ecx:#x} esi {esi:#x} edi {edi:#x}]")
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn ring_report(ctx: u64) -> String {
        let r = ring_of(ctx);
        if r.ctx.load(Ordering::Relaxed) != ctx {
            return "(none recorded)".into();
        }
        let n = r.next.load(Ordering::Relaxed);
        (n.saturating_sub(RING as u64)..n)
            .map(|i| {
                let e = &r.calls[(i % RING as u64) as usize];
                let (a, b) = (e[0].load(Ordering::Relaxed), e[1].load(Ordering::Relaxed));
                format!("{}(sp {:#x}: {:#x} {:#x})", nt::syscall_name((a >> 32) as u32), a & 0xffff_ffff, b >> 32, b & 0xffff_ffff)
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn set_resume_ctx_none_and_trap() -> ! {
        crate::set_resume_ctx_none();
        trap()
    }

    /// End the process with NT exit code `code`. The low byte is what a
    /// parent sees, as natively.
    /// An AOT-only run: JS serves units, catches this Worker up on the
    /// function table and posts new threads, inside this call.
    pub(crate) fn serve_now() {
        // SAFETY: a host import without arguments.
        unsafe { serve_in_place() };
    }

    pub(crate) fn end_process(code: u32) -> ! {
        if crate::tracing_exc() {
            let (n, l) = crate::cgen::callouts();
            crate::stderr_line(&format!(
                "[nt] at exit: exc_nested={} exc_escaped={} callouts={n} callouts_left={l} in_place={} fjit={:?}\n",
                EXC_NESTED.load(Ordering::Relaxed),
                EXC_ESCAPED.load(Ordering::Relaxed),
                crate::IN_PLACE_N.load(Ordering::Relaxed),
                crate::fjit::counts()
            ));
        }
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
        if let nt::Fault::NotExecutable = fault {
            // What the two maps say about the address, since the NT layer and
            // the translator disagreeing is the interesting case.
            let nt_side = {
                let st = p.st.lock().unwrap();
                match st.vm.find(pc) {
                    Some(r) => {
                        let page = &r.pages[((pc - r.base) / nt::vm::PAGE) as usize];
                        // Runs of pages by protection, which says whether the
                        // program protected part of the region and missed this.
                        let mut runs = String::new();
                        let mut i = 0;
                        while i < r.pages.len() {
                            let p0 = r.pages[i];
                            let mut j = i;
                            while j < r.pages.len() && r.pages[j] == p0 {
                                j += 1;
                            }
                            runs.push_str(&format!(" [{:#x}..{:#x} {}{:#x}]", r.base + i as u64 * nt::vm::PAGE,
                                r.base + j as u64 * nt::vm::PAGE, if p0.committed { "" } else { "res " }, p0.prot));
                            i = j;
                        }
                        format!(
                            "NT region {:#x}+{:#x} {} alloc_prot {:#x}, page committed={} prot {:#x}; pages{runs}",
                            r.base, r.size, r.name, r.alloc_prot, page.committed, page.prot
                        )
                    }
                    None => "no NT region".into(),
                }
            };
            let linux = with_host(|h| {
                h.kernel
                    .mem
                    .vmas()
                    .iter()
                    .find(|v| pc >= v.start && pc < v.end)
                    .map(|v| format!("{:#x}-{:#x} prot {:#x} {}", v.start, v.end, v.prot, v.name))
                    .unwrap_or_else(|| "no Linux mapping".into())
            });
            // Who branched there: the block the last indirect branch left,
            // and the stack (i386: the return address of a call is on top).
            let gm = GuestMem::new(guest_bytes());
            let c = ctx as u32 as u64;
            let r = |o: usize| gm.read_u64(c + o as u64).unwrap_or(0);
            let sp = r(ctx::SP) & 0xffff_ffff;
            let stack: Vec<String> = (0..12u64)
                .map(|i| p.describe_addr(gm.read_u32(sp + 4 * i).unwrap_or(0) as u64))
                .collect();
            let regs: Vec<String> = (0..8u8).map(|i| format!("{:#x}", r(ctx::x(i)) & 0xffff_ffff)).collect();
            let recent = ring_report(c);
            warn_line(&format!("gex-nt: this thread's last calls: {recent}\n"));
            warn_line(&format!("gex-nt: this thread's last trap pcs: {}\n", trap_report(c)));
            warn_line(&format!(
                "gex-nt: execute fault at {}: {nt_side}; {linux}; from block {} (last target {}), regs [{}], sp {sp:#x} stack [{}]\n",
                p.describe_addr(pc),
                p.describe_addr(r(ctx::LAST_SOURCE)),
                p.describe_addr(r(ctx::LAST_TARGET)),
                regs.join(" "),
                stack.join(" ")
            ));
        }
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
