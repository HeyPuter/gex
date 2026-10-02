//! The NT personality's native host side (docs/nt-layer.md §2.3, §10).
//!
//! A Windows program runs through the same machinery as a Linux one
//! ([`super::run_program_with`] with an empty image and the runtime
//! translator on), with three differences, all here:
//! - [`boot`] replaces `place_image`: `gex_kernel::nt::boot` maps the program
//!   and Wine's ntdll and sets the main thread up to enter
//!   `LdrInitializeThunk`;
//! - [`syscall_import`] replaces the body of the `syscall` import: every
//!   `syscall` of an NT process is an NT call, dispatched by
//!   `gex_kernel::nt::syscall` outside the kernel lock;
//! - a ticker keeps `KUSER_SHARED_DATA`'s clocks moving.

use super::*;
use gex_kernel::nt;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// What `gexc run` decided about a Windows program.
#[derive(Clone, Debug)]
pub struct NtSetup {
    pub abi: nt::Abi,
    /// The program's guest path.
    pub exe_path: String,
    /// Arguments after argv[0].
    pub args: Vec<String>,
    pub cfg: nt::Config,
    /// `--wayland-out`: where the display writes composited frames.
    pub display_out: Option<String>,
}

/// `GEX_NT_TIME_SCALE`: how much faster than real time the guest's
/// monotonic clocks run (the counterpart of PEWASM_TIME_SCALE). Every
/// monotonic source (QueryPerformanceCounter, the tick count and interrupt
/// time in KUSER_SHARED_DATA, wait timeouts) reads `mono_ns`, so they agree;
/// the wall-clock date and time are not scaled.
fn time_scale() -> f64 {
    static SCALE: OnceLock<f64> = OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("GEX_NT_TIME_SCALE").ok().and_then(|v| v.parse().ok()).filter(|v: &f64| *v > 0.0).unwrap_or(1.0)
    })
}

fn mono_ns() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let real = START.get_or_init(Instant::now).elapsed().as_nanos() as u64;
    let scale = time_scale();
    if scale == 1.0 {
        real
    } else {
        (real as f64 * scale) as u64
    }
}

fn wall() -> (u64, u32) {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs(), d.subsec_nanos())
}

pub fn clocks() -> nt::Clocks {
    nt::Clocks { mono_ns, wall }
}

/// Run a Windows program to completion.
pub fn run_nt_program(opts: &RunOptions, xlate: &Options, setup: &NtSetup) -> Result<(Exit, Stats)> {
    // Nothing is translated ahead of time: Wine's loader maps every image
    // itself, so all code arrives through the runtime translator.
    let mut exe = gex_ld::elf::headers_only(0, 0, 0, 0);
    // A PE32 program's code is decoded in i386 compatibility mode (S4).
    exe.class32 = setup.abi == nt::Abi::P32;
    let program = Program { exe, interp: None };
    let mut opts = opts.clone();
    opts.jit = true;
    super::nt_cgen::enable()?;
    let mut setup = setup.clone();
    if setup.display_out.is_none() {
        setup.display_out = opts.wayland_out.clone();
    }
    run_program_with(&program, &opts, xlate, None, Some(&setup))
}

/// Build the NT process in place of `place_image`. Returns the pc the main
/// thread starts at (its registers are already in context slot 0).
pub(crate) fn boot(store: &mut Store<HostState>, memory: gex_mem::GuestMemory, setup: &NtSetup) -> Result<u64> {
    let (futexes, pid) = store.data_mut().with_kernel(|k| (Arc::clone(&k.futexes), k.pid));
    let p = Arc::new(nt::Process::new(setup.abi, nt::thread::nt_pid(pid), setup.cfg.clone(), clocks(), futexes));
    let req = nt::boot::BootRequest { exe_path: &setup.exe_path, args: &setup.args };
    let pc = store
        .data_mut()
        .with_kernel(|k| nt::boot::boot(&p, k, memory.bytes(), &req))
        .map_err(|e| anyhow!("starting {}: {e}", setup.exe_path))?;
    if let Some(out) = &setup.display_out {
        *p.display_host.lock().unwrap() = Some(Arc::new(PpmDisplay::new(out)));
    }
    store.data_mut().nt = Some(Arc::clone(&p));
    let sampler = store.data().sampler.clone();
    start_ticker(memory, Arc::downgrade(&p), sampler);
    Ok(pc)
}

/// How long without a frame before the hang dump: `GEX_NT_HANG_SECS`, default
/// 300 (Noita's world load holds one frame for over a minute).
fn hang_after() -> Duration {
    Duration::from_secs(std::env::var("GEX_NT_HANG_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(300))
}

/// GEX_NT_STATS, once, when no frame has been presented for a while: every
/// guest thread's pc, stack pointer and state, and the image addresses on the
/// top of its stack (return addresses, mostly), so a hang says where each
/// thread is.
fn hang_dump(p: &nt::Process, memory: &gex_mem::GuestMemory) {
    let m = GuestMem::new(memory.bytes());
    let threads: Vec<Arc<nt::Thread>> = p.threads.read().unwrap().by_tid.values().cloned().collect();
    eprintln!("gex-nt: hang: no frame for {} s; {} threads", hang_after().as_secs(), threads.len());
    let p32 = p.abi == nt::Abi::P32;
    for t in threads {
        let pc = m.read_u64(t.ctx_addr + gex_abi::ctx::PC as u64).unwrap_or(0);
        let sp = m.read_u64(t.ctx_addr + gex_abi::ctx::x(4) as u64).unwrap_or(0);
        let sp = if p32 { sp & 0xffff_ffff } else { sp };
        let mut chain = Vec::new();
        let w = if p32 { 4 } else { 8 };
        for i in 0..256u64 {
            let a = sp + i * w;
            let v = if p32 { m.read_u32(a).map(|v| v as u64) } else { m.read_u64(a) };
            let Ok(v) = v else { break };
            let d = p.describe_addr(v);
            if d.contains('(') {
                chain.push(d);
                if chain.len() >= 12 {
                    break;
                }
            }
        }
        eprintln!(
            "gex-nt: hang: tid {} pc {} sp {sp:#x} suspend {} callback_depth {} alerted {} exited {}\n    stack: {}",
            t.tid,
            p.describe_addr(pc),
            t.suspend.load(std::sync::atomic::Ordering::Relaxed),
            t.callback_depth.load(std::sync::atomic::Ordering::Relaxed),
            t.alerted.load(std::sync::atomic::Ordering::Relaxed),
            t.exit_code.lock().unwrap().is_some(),
            chain.join(" <- ")
        );
    }
}

fn layout_misses() -> u64 {
    nt::section::LAYOUT_MISSES.load(std::sync::atomic::Ordering::Relaxed)
}

/// The native display (§6a.1): every published frame composites the
/// visible top-level windows at their screen positions, cropped to their
/// bounding box, into `PREFIX-NNNNN.ppm`. A file is what a test compares and
/// what works with no screen. There is no native input source: input is
/// `GEX_NT_INPUT`'s.
struct PpmDisplay {
    prefix: String,
    state: Mutex<PpmState>,
}

#[derive(Default)]
struct PpmState {
    /// Window id -> (x, y, width, height, BGRX rows).
    windows: std::collections::BTreeMap<u32, (i32, i32, i32, i32, Vec<u8>)>,
    written: u64,
}

impl PpmDisplay {
    fn new(prefix: &str) -> PpmDisplay {
        PpmDisplay { prefix: prefix.to_string(), state: Mutex::new(PpmState::default()) }
    }
}

impl nt::display::NtDisplay for PpmDisplay {
    fn publish(&self, mem: &GuestMem<'_>, s: &nt::display::NtSurface) {
        let (w, h) = (s.width.max(0), s.height.max(0));
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as u64 {
            match mem.read(s.addr + y * s.stride as u64, w as usize * 4) {
                Ok(row) => px.extend_from_slice(row),
                Err(_) => return,
            }
        }
        let mut st = self.state.lock().unwrap();
        st.windows.insert(s.id, (s.x, s.y, w, h, px));
        // The bounding box of what is shown, composited in id order.
        let (mut l, mut t, mut r, mut b) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for (x, y, w, h, _) in st.windows.values() {
            l = l.min(*x);
            t = t.min(*y);
            r = r.max(x + w);
            b = b.max(y + h);
        }
        let (cw, ch) = ((r - l).max(1), (b - t).max(1));
        let mut rgb = vec![0u8; (cw * ch * 3) as usize];
        for (x, y, w, h, px) in st.windows.values() {
            for row in 0..*h {
                for col in 0..*w {
                    let (cx, cy) = (x - l + col, y - t + row);
                    let i = ((row * w + col) * 4) as usize;
                    let o = ((cy * cw + cx) * 3) as usize;
                    rgb[o..o + 3].copy_from_slice(&[px[i + 2], px[i + 1], px[i]]);
                }
            }
        }
        let path = format!("{}-{:05}.ppm", self.prefix, st.written);
        st.written += 1;
        drop(st);
        let mut out = format!("P6\n{cw} {ch}\n255\n").into_bytes();
        out.extend_from_slice(&rgb);
        let tmp = format!("{path}.tmp");
        if std::fs::write(&tmp, out).and_then(|()| std::fs::rename(&tmp, &path)).is_err() {
            eprintln!("gex: could not write {path}");
        }
    }

    fn remove(&self, id: u32) {
        self.state.lock().unwrap().windows.remove(&id);
    }

    fn poll_input(&self) -> Vec<nt::display::NtDisplayEvent> {
        Vec::new()
    }
}

/// Advance `KUSER_SHARED_DATA`'s clocks every millisecond for as long as the
/// process exists, so a guest polling `GetTickCount` without syscalls sees
/// time pass (docs/nt-layer.md §4).
fn start_ticker(memory: gex_mem::GuestMemory, p: std::sync::Weak<nt::Process>, sampler: Option<Arc<Sampler>>) {
    let stats = std::env::var_os("GEX_NT_STATS").is_some();
    let peek = std::env::var("GEX_NT_PEEK").ok().and_then(|v| {
        let (addr, deref) = match v.strip_suffix('*') {
            Some(a) => (a, true),
            None => (v.as_str(), false),
        };
        let addr = u64::from_str_radix(addr.trim_start_matches("0x"), 16).ok()?;
        Some((addr, deref))
    });
    // GEX_NT_HANG_DUMP=S: every S seconds, each thread's last safepoint pc,
    // last indirect branch and context registers (`dump_threads`), for a
    // console program that hangs (the frame-based dump below needs frames).
    let dump_every = std::env::var("GEX_NT_HANG_DUMP").ok().and_then(|v| v.parse::<u64>().ok()).map(Duration::from_secs);
    let mut next_dump = dump_every;
    let start = Instant::now();
    let mut next_stats = Duration::from_secs(1);
    // The hang dump: presents frozen this long after the first one.
    let (mut last_presents, mut frozen_since, mut dumped) = (0u64, Instant::now(), false);
    let _ = std::thread::Builder::new().name("gex-nt-usd".into()).spawn(move || loop {
        match p.upgrade() {
            Some(p) if !p.terminating.load(std::sync::atomic::Ordering::Relaxed) => {
                nt::sysinfo::usd_tick(memory.bytes(), &p.clocks);
                if let (Some(every), Some(at)) = (dump_every, next_dump) {
                    if start.elapsed() >= at {
                        next_dump = Some(at + every);
                        dump_threads(&p, &mut GuestMem::new(memory.bytes()));
                    }
                }
                // GEX_NT_STATS: once a (real) second, for benchmark harnesses.
                if stats && start.elapsed() >= next_stats {
                    next_stats += Duration::from_secs(1);
                    let presents = p.presents.load(std::sync::atomic::Ordering::Relaxed);
                    let value = peek.and_then(|(addr, deref)| {
                        let m = GuestMem::new(memory.bytes());
                        let mut a = addr;
                        if deref {
                            a = m.read_u32(a).ok()? as u64;
                            if a == 0 {
                                return None; // not set up yet
                            }
                        }
                        m.read_u32(a).ok()
                    });
                    let nest_exits = GuestMem::new(memory.bytes()).read_u64(gex_abi::NEST_EXITS).unwrap_or(0);
                    let co = crate::CALLOUTS.load(std::sync::atomic::Ordering::Relaxed);
                    let coa = crate::CALLOUT_ABORTS.load(std::sync::atomic::Ordering::Relaxed);
                    let cg = format!("cgen nest_exits={nest_exits} callouts={co} callout_aborts={coa} {}", crate::ftab::stats::line());
                    match value {
                        Some(v) => eprintln!("gex-nt: stats t={} presents={presents} peek={v} layout_miss={} {cg} {}", start.elapsed().as_millis(), layout_misses(), crate::jit_stats::line()),
                        None => eprintln!("gex-nt: stats t={} presents={presents} layout_miss={} {cg} {}", start.elapsed().as_millis(), layout_misses(), crate::jit_stats::line()),
                    }
                    if next_stats.as_secs() % 60 == 0 {
                        let ex: Vec<String> = crate::take_exit_sites(15)
                            .into_iter()
                            .map(|(f, t, c)| format!("{} -> {} x{c}", p.describe_addr(f), p.describe_addr(t)))
                            .collect();
                        eprintln!("gex-nt: nest exits (1 in 4096) {}", ex.join(" | "));
                        let fb: Vec<String> = crate::ftab::stats::take_fallback_sites(15)
                            .into_iter()
                            .map(|(t, c)| format!("{} x{c}", p.describe_addr(t)))
                            .collect();
                        eprintln!("gex-nt: fallback targets (1 in 1024) {}", fb.join(" | "));
                        let mut by: std::collections::BTreeMap<String, u64> = Default::default();
                        for (page, n) in crate::tier1_pages() {
                            let d = p.describe_addr(page * gex_abi::DISPATCH_PAGE);
                            let name = d.split('(').nth(1).and_then(|x| x.split('+').next()).unwrap_or("(no image)").to_string();
                            *by.entry(name).or_default() += n;
                        }
                        let mut v: Vec<_> = by.into_iter().collect();
                        v.sort_by(|a, b| b.1.cmp(&a.1));
                        let v: Vec<String> = v.iter().take(12).map(|(m, n)| format!("{m} {n}")).collect();
                        eprintln!("gex-nt: tier-1 blocks by image: {}", v.join(", "));
                    }
                    let presents = presents as u64;
                    if presents != last_presents {
                        (last_presents, frozen_since) = (presents, Instant::now());
                    } else if presents > 0 && !dumped && frozen_since.elapsed() >= hang_after() {
                        dumped = true;
                        hang_dump(&p, &memory);
                    }
                    // With --sample, the hottest guest back-edges of the last
                    // 30 s, by module, so a run that never exits can still be
                    // profiled.
                    if let Some(s) = sampler.as_ref().filter(|_| next_stats.as_secs() % 30 == 0) {
                        let counts = std::mem::take(&mut *s.counts.lock().unwrap_or_else(|e| e.into_inner()));
                        s.append_out(&counts);
                        let mut v: Vec<(u64, u64)> = counts.into_iter().collect();
                        v.sort_by(|a, b| b.1.cmp(&a.1));
                        for (pc, n) in v.into_iter().take(12) {
                            eprintln!("gex-nt: hot {n} {}", p.describe_addr(pc));
                        }
                    }
                }
            }
            _ => return,
        }
        std::thread::sleep(Duration::from_millis(1));
    });
}

/// GEX_NT_HANG_DUMP's report: every thread's last safepoint pc, last
/// indirect branch, registers and the top of its stack.
fn dump_threads(p: &nt::Process, gm: &mut GuestMem<'_>) {
    let threads: Vec<Arc<nt::Thread>> = p.threads.read().unwrap().by_tid.values().cloned().collect();
    let p32 = p.abi == nt::Abi::P32;
    for t in threads {
        let c = t.ctx_addr;
        let r = |o: usize| gm.read_u64(c + o as u64).unwrap_or(0);
        let mask = if p32 { 0xffff_ffff } else { !0 };
        let regs: Vec<String> = (0..if p32 { 8 } else { 16 }).map(|i| format!("{:#x}", r(ctx::x(i)) & mask)).collect();
        let sp = r(ctx::x(4)) & mask;
        let stack: Vec<String> = (0..8u64)
            .map(|i| {
                let v = if p32 { gm.read_u32(sp + 4 * i).unwrap_or(0) as u64 } else { gm.read_u64(sp + 8 * i).unwrap_or(0) };
                p.describe_addr(v)
            })
            .collect();
        eprintln!(
            "gex-nt: thread {:x}: pc {} last target {} from {}; safepoint counter {}; alerted {} word {:#x}; regs [{}] stack [{}]",
            t.tid,
            p.describe_addr(r(ctx::PC)),
            p.describe_addr(r(ctx::LAST_TARGET)),
            p.describe_addr(r(ctx::LAST_SOURCE)),
            gm.read_u32(c + ctx::SAFEPOINT_COUNTER as u64).unwrap_or(0) as i32,
            t.alerted.load(std::sync::atomic::Ordering::Relaxed),
            gm.read_u32(t.word).unwrap_or(0),
            regs.join(" "),
            stack.join(" ")
        );
        // The next back-edge then stops at the host, which stores its pc:
        // a thread looping shows where by the next report.
        let _ = gm.write_u32(c + ctx::SAFEPOINT_COUNTER as u64, 0);
    }
}

/// The `syscall` import for an NT process.
pub(crate) fn syscall_import(caller: &mut Caller<'_, HostState>, pc: u64) -> Result<()> {
    catch_up_in_caller(caller)?;
    let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let state = caller.data_mut();
    if state.spawn.as_ref().is_some_and(|s| s.stopping.load(std::sync::atomic::Ordering::Acquire)) {
        state.exit = Some(Exit::Status(0));
        return Err(StopExecution.into());
    }
    let p = Arc::clone(state.nt.as_ref().expect("an NT syscall in a Linux process"));
    let kernel = Arc::clone(&state.kernel);
    let ctx_addr = state.thread.ctx_addr;
    let outcome = {
        let mut host = CallerHost { caller: &mut *caller, p: &p };
        nt::syscall(&p, &*kernel, memory.bytes(), ctx_addr, pc, &mut host)
    };
    // A callback inside that syscall may have ended the process or failed.
    if caller.data().exit.is_some() {
        return Err(StopExecution.into());
    }
    // Code that went away (an unmapped image, a protection change) loses its
    // translations. NT images are never statically recompiled, so newly
    // mapped code needs nothing here.
    let (retired, _mapped) = caller
        .data_mut()
        .with_kernel(|k| (std::mem::take(&mut k.retired_code), std::mem::take(&mut k.new_code)));
    if !retired.is_empty() {
        retire_translations(caller, &retired)?;
    }
    // An image just mapped that has a hot set gets its function-tier unit.
    super::nt_cgen::drain_in_caller(caller)?;
    let state = caller.data_mut();
    let resume_pc = || read_u64(memory.bytes(), (ctx_addr + ctx::PC as u64) as usize);
    match outcome {
        nt::Outcome::Continue => Ok(()),
        nt::Outcome::Resume(target) => {
            state.resume_at = Some(target);
            Err(StopExecution.into())
        }
        nt::Outcome::Exit(code) => {
            state.exit = Some(Exit::Status(code as i32));
            Err(StopExecution.into())
        }
        nt::Outcome::ThreadExit => {
            // The main thread's host thread is the process's: when it ends
            // first (ExitThread, or a kill from another thread's
            // ExitProcess), it waits for the process to end and reports the
            // process's exit code, as Windows does.
            if ctx_addr == gex_abi::ctx_addr(0) {
                while !p.terminating.load(std::sync::atomic::Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                state.exit = Some(Exit::Status(p.exit_code.load(std::sync::atomic::Ordering::Acquire) as i32));
            } else {
                state.exit = Some(Exit::Status(0));
            }
            Err(StopExecution.into())
        }
        nt::Outcome::SpawnThread { ctx_addr: child, gex_tid } => {
            // Started by the run loop between calls, as a Linux clone is.
            state.pending_spawn = Some(ThreadState { ctx_addr: child, tid: gex_tid, clear_child_tid: 0 });
            state.resume_at = Some(resume_pc());
            Err(StopExecution.into())
        }
        // The result is with the thread; unwinding here ends the innermost
        // nested loop (`CallerHost::run_callback`).
        nt::Outcome::CallbackReturn => Err(StopExecution.into()),
    }
}

pub(crate) use gex_kernel::nt::Fault;

/// Deliver `fault` at `pc` to the guest as an NT exception, as Wine's signal
/// handlers do. Returns the StopExecution that redirects the thread into
/// `KiUserExceptionDispatcher`.
pub(crate) fn fault(caller: &mut Caller<'_, HostState>, fault: Fault, pc: u64) -> Result<()> {
    let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let state = caller.data_mut();
    let p = Arc::clone(state.nt.as_ref().expect("an NT fault in a Linux process"));
    if let Fault::X87 = fault {
        eprintln!("gex-nt: an unmasked x87 exception at {pc:#x}: gex does not deliver x87 exceptions; ending the process");
        state.exit = Some(Exit::Signal {
            signal: 8,
            pc,
            detail: "unmasked x87 exception (not delivered by gex)".into(),
            internal: true,
        });
        return Err(StopExecution.into());
    }
    let (code, rip, info) = fault.exception(pc).expect("every fault but X87 is an exception");
    let outcome = nt::hardware_exception(&p, &*state.kernel, memory.bytes(), state.thread.ctx_addr, code, rip, &info);
    match outcome {
        nt::Outcome::Resume(target) => state.resume_at = Some(target),
        nt::Outcome::Exit(code) => state.exit = Some(Exit::Status(code as i32)),
        other => bail!("gex-nt: unexpected outcome {other:?} from exception dispatch"),
    }
    Err(StopExecution.into())
}

/// Runs user-mode callbacks for the NT layer: a nested guest loop on the
/// calling thread, inside the `syscall` import (docs/nt-layer.md §5).
struct CallerHost<'a, 'b> {
    caller: &'a mut Caller<'b, HostState>,
    p: &'a Arc<nt::Process>,
}

impl nt::NtHost for CallerHost<'_, '_> {
    fn run_callback(&mut self) -> std::result::Result<(), ()> {
        match nested_loop(self.caller, self.p) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Recorded as a failure of the process, which the outer
                // syscall import notices on return.
                let state = self.caller.data_mut();
                if state.exit.is_none() {
                    eprintln!("gex-nt: a user-mode callback failed: {e:#}");
                    state.exit = Some(Exit::Signal {
                        signal: signal::SIGSEGV,
                        pc: 0,
                        detail: format!("user-mode callback: {e:#}"),
                        internal: true,
                    });
                }
                Err(())
            }
        }
    }
}

/// The same loop as `run_thread_loop`, one level down: re-enter translated
/// code at the context's pc, follow redirects, and stop at the thread's
/// `NtCallbackReturn`.
fn nested_loop(caller: &mut Caller<'_, HostState>, p: &Arc<nt::Process>) -> Result<()> {
    // The calling module's `enter`, or -- when the call came from a
    // function-tier unit, which has none -- any tier-1 module's in this
    // store: they all reach every slot through the same tables.
    let enter = match caller.get_export(exports::ENTER).and_then(Extern::into_func) {
        Some(f) => f,
        None => {
            let instances = caller.data().jit.as_ref().map(|j| j.instances.clone()).unwrap_or_default();
            instances
                .iter()
                .find_map(|i| i.get_func(&mut *caller, exports::ENTER))
                .ok_or_else(|| anyhow!("no module in this store exports `enter`"))?
        }
    };
    let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let ctx_addr = caller.data().thread.ctx_addr;
    let thread = p.thread_by_ctx(ctx_addr).ok_or_else(|| anyhow!("a callback on a thread the NT layer does not know"))?;
    let mut pc = read_u64(memory.bytes(), (ctx_addr + ctx::PC as u64) as usize);
    let mut next: Option<wasmtime::Func> = None;
    loop {
        let result = match next.take() {
            Some(f) => f.call(&mut *caller, &[Val::I32(ctx_addr as i32)], &mut []),
            None => enter.call(&mut *caller, &[Val::I32(ctx_addr as i32), Val::I64(pc as i64)], &mut []),
        };
        if let Some(child) = caller.data_mut().pending_spawn.take() {
            let cx = caller.data().spawn.clone().ok_or_else(|| anyhow!("thread creation with no spawn context"))?;
            start_guest_thread(cx, child)?;
        }
        let e = match result {
            Ok(()) => bail!("guest code returned out of a user-mode callback"),
            Err(e) => e,
        };
        // A function-tier call-out further out watches a dispatch slot this
        // callback's code reached: something else that uses the slot runs
        // what it held; its own return (which a deeper frame cannot make)
        // goes back to the call-out's loop.
        if let Some(sig) = caller.data_mut().callout_signal.take() {
            match sig {
                crate::CalloutSignal::Bypass(f) => {
                    next = Some(f);
                    continue;
                }
                other => {
                    caller.data_mut().callout_signal = Some(other);
                    return Err(e);
                }
            }
        }
        if thread.callback_result.lock().unwrap().is_some() {
            return Ok(());
        }
        if let Some(next) = caller.data_mut().resume_at.take() {
            crate::ftab::stats::RESUMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            pc = next;
            continue;
        }
        if caller.data().exit.is_some() {
            bail!("the thread ended inside a user-mode callback");
        }
        if is_stale_table_trap(&e) {
            catch_up_in_caller(caller)?;
            pc = read_u64(memory.bytes(), (ctx_addr + ctx::PC as u64) as usize);
            continue;
        }
        return Err(e);
    }
}
