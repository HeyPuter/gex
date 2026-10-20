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

/// The guest's wall clock. `GEX_NT_FIXED_TIME=<unix seconds>` (the
/// counterpart of PEWASM_FIXED_TIME) starts it at that instant and advances
/// it with real time, so a program that reads the date sees the same day
/// every run. A program that seeds its own RNG from elsewhere still differs
/// run to run.
fn wall() -> (u64, u32) {
    static FIXED: OnceLock<Option<(u64, Instant)>> = OnceLock::new();
    let fixed = FIXED.get_or_init(|| {
        std::env::var("GEX_NT_FIXED_TIME").ok().and_then(|v| v.parse::<u64>().ok()).map(|s| (s, Instant::now()))
    });
    let d = match fixed {
        Some((start, origin)) => std::time::Duration::from_secs(*start) + origin.elapsed(),
        None => SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default(),
    };
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
    super::nt_cgen::enable(opts.record.is_some())?;
    let mut setup = setup.clone();
    if setup.display_out.is_none() {
        setup.display_out = opts.wayland_out.clone();
    }
    run_program_with(&program, &opts, xlate, None, Some(&setup))
}

/// Build the NT process in place of `place_image`. Returns the pc the main
/// thread starts at (its registers are already in context slot 0).
pub(crate) fn boot(store: &mut Store<HostState>, memory: gex_mem::GuestMemory, setup: &NtSetup) -> Result<u64> {
    crate::profile::note_nt(setup, crate::gl::DEVICE.get().is_some());
    let (futexes, pid) = store.data_mut().with_kernel(|k| (Arc::clone(&k.futexes), k.pid));
    let p = Arc::new(nt::Process::new(setup.abi, nt::thread::nt_pid(pid), setup.cfg.clone(), clocks(), futexes));
    let req = nt::boot::BootRequest { exe_path: &setup.exe_path, args: &setup.args };
    let pc = store
        .data_mut()
        .with_kernel(|k| nt::boot::boot(&p, k, memory.bytes(), &req))
        .map_err(|e| anyhow!("starting {}: {e}", setup.exe_path))?;
    if let Some(out) = &setup.display_out {
        // `--wayland-out null`: frames are presented and dropped (the
        // throughput recipe, docs/noita-perf.md: pewasm's null GL shows
        // nothing either).
        let host: Arc<dyn nt::display::NtDisplay> =
            if out == "null" { Arc::new(NullDisplay) } else { Arc::new(PpmDisplay::new(out)) };
        // Frames nobody sees: a frame's GL may run while the next is built.
        if out == "null" {
            if let Some(dev) = crate::gl::DEVICE.get() {
                dev.set_threaded();
            }
        }
        *p.display_host.lock().unwrap() = Some(host);
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
struct NullDisplay;

impl nt::display::NtDisplay for NullDisplay {
    fn publish(&self, _mem: &GuestMem<'_>, _s: &nt::display::NtSurface) {}
    fn remove(&self, _id: u32) {}
    fn poll_input(&self) -> Vec<nt::display::NtDisplayEvent> {
        Vec::new()
    }
    fn shows_frames(&self) -> bool {
        false
    }
}

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
        if st.written % crate::ppm_every() as u64 != 0 {
            st.written += 1;
            return;
        }
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
    // GEX_NT_GL_STATS=1: the program's GL by function, with the 60 s report.
    let gl_stats = std::env::var("GEX_NT_GL_STATS").as_deref() == Ok("1");
    nt::opengl::STATS.store(gl_stats, std::sync::atomic::Ordering::Relaxed);
    nt::opengl::FAST.store(std::env::var("GEX_NT_GL_FAST").map_or(true, |v| v != "0"), std::sync::atomic::Ordering::Relaxed);
    nt::opengl::FAST_CLIENT_STATE.store(std::env::var("GEX_NT_GL_FAST").map_or(true, |v| v != "nocs"), std::sync::atomic::Ordering::Relaxed);
    nt::opengl::CLIENT_ARRAY_OP.store(std::env::var("GEX_NT_GL_CLIENT_ARRAYS").map_or(true, |v| v != "buffer"), std::sync::atomic::Ordering::Relaxed);
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
                    let cg = format!(
                        "cgen nest_exits={nest_exits} callouts={co} callout_aborts={coa} provider_try={} provider_throw={} {}",
                        crate::libprov::TRIES.load(std::sync::atomic::Ordering::Relaxed),
                        crate::libprov::THROWS.load(std::sync::atomic::Ordering::Relaxed),
                        crate::ftab::stats::line()
                    );
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
                        if let Some(l) = crate::aot_share::line() {
                            eprintln!("gex-nt: {l}");
                        }
                        let (lt, lv) = crate::take_leave_sites(15);
                        let lv: Vec<String> = lv
                            .into_iter()
                            .map(|(f, t, c)| format!("{} -> {}{} x{c}", p.describe_addr(f), p.describe_addr(t & !(1 << 63)), if t >> 63 != 0 { " [patched]" } else { "" }))
                            .collect();
                        eprintln!("gex-nt: leaves among them x{lt}: {}", lv.join(" | "));
                        let fb: Vec<String> = crate::ftab::stats::take_fallback_sites(15)
                            .into_iter()
                            .map(|(t, c)| format!("{} x{c}", p.describe_addr(t)))
                            .collect();
                        eprintln!("gex-nt: fallback targets (1 in 1024) {}", fb.join(" | "));
                        let (tot, top) = crate::ftab::stats::take_resume_causes(12);
                        let tot: Vec<String> = tot.iter().map(|(c, k)| format!("{c} x{k}")).collect();
                        let top: Vec<String> = top.iter().map(|((c, t), k)| format!("{c} -> {} x{k}", p.describe_addr(*t))).collect();
                        eprintln!("gex-nt: resumes by cause {} || {}", tot.join(" | "), top.join(" | "));
                        // Syscalls since the last report, most frequent first.
                        let mut sc: Vec<(u32, u64)> = SYSCALLS
                            .iter()
                            .enumerate()
                            .map(|(nr, n)| (nr as u32, n.swap(0, std::sync::atomic::Ordering::Relaxed)))
                            .filter(|&(_, n)| n > 0)
                            .collect();
                        sc.sort_by(|a, b| b.1.cmp(&a.1));
                        let total: u64 = sc.iter().map(|x| x.1).sum();
                        let sc: Vec<String> = sc.iter().take(12).map(|&(nr, n)| {
                                let nr = if nr as usize == UNIX_CALL_SLOT { nt::gex_nr::UNIX_CALL } else { nr };
                                format!("{} {n}", nt::syscall_name(nr))
                            }).collect();
                        eprintln!("gex-nt: syscalls {total}: {}", sc.join(" | "));
                        if let Some(l) = super::nt_cgen::no_unit_line() {
                            eprintln!("{l}");
                        }
                        if gl_stats {
                            eprintln!("gex-nt: gl {}", nt::opengl::take_stats(&p, 16));
                        }
                        let mut ss: Vec<((u32, u32, u64), u64)> =
                            std::mem::take(&mut *SYSCALL_SITES.lock().unwrap_or_else(|e| e.into_inner())).into_iter().collect();
                        ss.sort_by(|a, b| b.1.cmp(&a.1));
                        let ss: Vec<String> = ss
                            .iter()
                            .take(16)
                            .map(|&((nr, tid, site), n)| {
                                let nr = if nr as usize == UNIX_CALL_SLOT { nt::gex_nr::UNIX_CALL } else { nr };
                                format!("{} t{tid:x} from {} x{n}", nt::syscall_name(nr), p.describe_addr(site))
                            })
                            .collect();
                        if !ss.is_empty() {
                            eprintln!("gex-nt: syscall sites (1 in 64) {}", ss.join(" | "));
                        }
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

/// A system call made by native Wine on wasm32 (gexrt's `__gexw_syscall`,
/// docs/wine-native.md §3.15) carries this bit in its `pc`: a call that sends
/// the thread elsewhere (an APC, an NtContinue) writes that state into the
/// context and returns normally, as the program's native frames cannot be
/// left and resumed; the caller runs it or unwinds itself.
pub(crate) const NATIVE_SYSCALL: u64 = 1 << 61;

/// The `syscall` import for an NT process.
pub(crate) fn syscall_import(caller: &mut Caller<'_, HostState>, pc: u64) -> Result<()> {
    let native = pc & NATIVE_SYSCALL != 0;
    let pc = pc & !NATIVE_SYSCALL;
    catch_up_in_caller(caller)?;
    let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let state = caller.data_mut();
    if state.spawn.as_ref().is_some_and(|s| s.stopping.load(std::sync::atomic::Ordering::Acquire)) {
        state.exit = Some(Exit::Status(0));
        return Err(StopExecution.into());
    }
    let p = Arc::clone(state.nt.as_ref().expect("an NT syscall in a Linux process"));
    let ctx_addr = state.thread.ctx_addr;
    let nr = read_u64(memory.bytes(), (ctx_addr + ctx::x(0) as u64) as usize) as u32;
    if native && nr == nt::gex_nr::NATIVE_LEFT {
        // gexrt saw the guest enter native code at or above the innermost
        // native entry: it left that entry's frames earlier and runs nested
        // inside them. The innermost call-out, if a native caller's, leaves
        // (0); that caller drops its frames (gexrt's `run_until`), and the
        // guest makes the call again from `jmp eax` (EAX: the function,
        // which gexrt left in ctx::BALLAST), one level flatter. A stale mark
        // (no native call-out innermost) just goes on.
        let target = read_u64(memory.bytes(), (ctx_addr + ctx::BALLAST as u64) as usize);
        let sp = read_u64(memory.bytes(), (ctx_addr + ctx::x(4) as u64) as usize) & 0xffff_ffff;
        let left = crate::innermost_native_unwound(caller, sp);
        if std::env::var_os("GEX_WASM_TRACE").is_some() {
            let w: Vec<String> = caller.data().callouts.iter().map(|c| format!("{:#x}@{:#x}", c.ret, c.sp_want)).collect();
            eprintln!("gex: native left at {} (sp {sp:#x}): call-outs [{}], leaving {left}", p.describe_addr(target), w.join(" "));
        }
        if left {
            let _ = GuestMem::new(memory.bytes()).write_u64(ctx_addr + ctx::x(0) as u64, target);
            caller.data_mut().callout_signal = Some(crate::CalloutSignal::Abort);
            return Err(StopExecution.into());
        }
        return Ok(());
    }
    if crate::ftab::stats_on() {
        let i = if nr == nt::gex_nr::UNIX_CALL { UNIX_CALL_SLOT } else { nr as usize };
        if let Some(n) = SYSCALLS.get(i) {
            // 1 in 64: who made it, the first return address outside
            // Wine's core DLLs on the guest's frame-pointer chain.
            // GEX_NT_SYSCALL_SITES=1 only: the walk and its lock cost the
            // measured run itself.
            if n.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & 63 == 0 && p.abi == nt::Abi::P32 && sites_on() {
                let site = syscall_site(&p, &memory, ctx_addr);
                let tid = p.thread_by_ctx(ctx_addr).map_or(0, |t| t.tid);
                *SYSCALL_SITES.lock().unwrap_or_else(|e| e.into_inner()).entry((i as u32, tid, site)).or_default() += 1;
            }
        }
    }
    // Where the syscall returns to, and the stack it was made on: what a
    // nested exception dispatch needs to come back here.
    let next_pc = read_u64(memory.bytes(), (ctx_addr + ctx::PC as u64) as usize);
    let raise_sp = read_u64(memory.bytes(), (ctx_addr + ctx::x(4) as u64) as usize);
    // The thread, once per store: its context address never changes.
    let t = match caller.data().nt_thread.as_ref().filter(|t| t.ctx_addr == ctx_addr) {
        Some(t) => Some(Arc::clone(t)),
        None => {
            let t = p.thread_by_ctx(ctx_addr);
            caller.data_mut().nt_thread = t.clone();
            t
        }
    };
    let outcome = {
        let mut host = CallerHost { caller: &mut *caller, p: &p };
        nt::syscall_on(&p, t, memory.bytes(), ctx_addr, pc, &mut host)
    };
    // A callback inside that syscall may have ended the process or failed.
    if caller.data().exit.is_some() {
        return Err(StopExecution.into());
    }
    // Code that went away (an unmapped image, a protection change) loses its
    // translations. NT images are never statically recompiled, so newly
    // mapped code needs nothing here.
    // The kernel lock only when something was recorded since the last look.
    // Raised before the swap, so a thread that finds the count already zero
    // (this one took its events) waits for this drain (`RETIRING`).
    use std::sync::atomic::Ordering::SeqCst;
    if gex_kernel::CODE_EVENTS.load(SeqCst) != 0 {
        super::RETIRING.fetch_add(1, SeqCst);
        let drained = (|| -> Result<()> {
            if gex_kernel::CODE_EVENTS.swap(0, SeqCst) != 0 {
                let (retired, _mapped) = caller.data_mut().with_kernel(|k| {
                    let mut mm = k.mem.lock();
                    (std::mem::take(&mut mm.log.retired_code), std::mem::take(&mut mm.log.new_code))
                });
                if !retired.is_empty() {
                    retire_translations(caller, &retired)?;
                }
                recheck_units(caller);
            }
            Ok(())
        })();
        super::RETIRING.fetch_sub(1, SeqCst);
        drained?;
    }
    super::await_retires();
    // An image just mapped that has a hot set gets its function-tier unit.
    super::nt_cgen::drain_in_caller(caller)?;
    let state = caller.data_mut();
    let resume_pc = || read_u64(memory.bytes(), (ctx_addr + ctx::PC as u64) as usize);
    match outcome {
        nt::Outcome::Continue => Ok(()),
        nt::Outcome::Resume(target) if native => {
            let _ = GuestMem::new(memory.bytes()).write_u64(ctx_addr + ctx::PC as u64, target);
            Ok(())
        }
        nt::Outcome::Resume(target) => {
            // NtContinue out of a nested dispatch (EXCEPTION_CONTINUE_EXECUTION,
            // a handler resuming its frame): back to where it was raised.
            if exc_escape(caller, target) {
                return Err(StopExecution.into());
            }
            // An exception raised, or a user APC delivered by an alertable
            // wait: both run a dispatcher that ends in NtContinue.
            // (Not NtContinue's own APC delivery: what it returns to is the
            // context it loaded, not its caller.)
            let apc = target == p.ntdll.lock().unwrap().ki_user_apc_dispatcher
                && nr != nt::sysno::ntdll::NtContinue
                && nr != nt::sysno::ntdll::NtContinueEx;
            if (nr == nt::sysno::ntdll::NtRaiseException || apc) && nest_exceptions(memory.bytes(), next_pc) {
                return raise_nested(caller, &p, target, raise_sp, Some(next_pc));
            }
            let state = caller.data_mut();
            if crate::ftab::stats_on() {
                state.resume_cause = Some(nt::syscall_name(nr));
            }
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
                // Only this thread ends (ExitThread, or killed by another's
                // NtTerminateProcess(0)), not the process.
                state.exit = Some(Exit::Status(0));
                state.thread_only = true;
            }
            Err(StopExecution.into())
        }
        nt::Outcome::SpawnThread { ctx_addr: child, gex_tid } => {
            // Started here, and the caller carries on: leaving for the run
            // loop would unwind every wasm frame, which under the function
            // table means every caller resumes at an interior address.
            let cx = state.spawn.clone().ok_or_else(|| anyhow!("thread creation with no spawn context"))?;
            super::start_guest_thread(cx, ThreadState { ctx_addr: child, tid: gex_tid })?;
            Ok(())
        }
        // The result is with the thread; unwinding here ends the innermost
        // nested loop (`CallerHost::run_callback`).
        nt::Outcome::CallbackReturn => Err(StopExecution.into()),
    }
}

/// NT syscalls by number (GEX_NT_STATS): ntdll's table and win32u's
/// (0x1000 up).
const UNIX_CALL_SLOT: usize = 0x1fff;
fn sites_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("GEX_NT_SYSCALL_SITES").as_deref() == Ok("1"))
}

/// Sampled syscalls by (number, thread, guest site) (GEX_NT_STATS with
/// GEX_NT_SYSCALL_SITES=1).
static SYSCALL_SITES: std::sync::Mutex<std::collections::BTreeMap<(u32, u32, u64), u64>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// The first return address outside Wine's core DLLs (ntdll, kernelbase,
/// kernel32, the trampolines) on an i386 thread's EBP chain at a syscall:
/// the program code that asked. 0 when the chain ends first.
fn syscall_site(p: &nt::Process, memory: &gex_mem::GuestMemory, ctx_addr: u64) -> u64 {
    let gm = GuestMem::new(memory.bytes());
    let system = |a: u64| {
        if (nt::TRAMPOLINES..nt::TRAMPOLINES_END).contains(&a) {
            return true;
        }
        let d = p.describe_addr(a);
        ["(ntdll.dll+", "(kernelbase.dll+", "(kernel32.dll+"].iter().any(|m| d.contains(m))
    };
    let mut bp = gm.read_u64(ctx_addr + ctx::x(5) as u64).unwrap_or(0) & 0xffff_ffff;
    for _ in 0..16 {
        let (Ok(next), Ok(ret)) = (gm.read_u32(bp), gm.read_u32(bp + 4)) else { return 0 };
        if !system(ret as u64) {
            return ret as u64;
        }
        if (next as u64) <= bp {
            return 0;
        }
        bp = next as u64;
    }
    0
}
static SYSCALLS: [std::sync::atomic::AtomicU64; 0x2000] = [const { std::sync::atomic::AtomicU64::new(0) }; 0x2000];

/// Exceptions dispatched nested and escaped back (GEX_NT_STATS).
pub(crate) static EXC_NESTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) static EXC_ESCAPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How deep exceptions nest before raising traps out as before.
const EXC_MAX_DEPTH: usize = 8;

/// Whether an exception raised by the syscall returning to `next_pc` is
/// dispatched nested (`raise_nested`) rather than by unwinding every wasm
/// frame of the thread. Only with function bodies about (tier-1 alone has no
/// frames to keep), and only when the syscall returns through a `ret`, which
/// is how the dispatch gets back (`GEX_NT_EXC_NEST=0` turns it off).
fn nest_exceptions(mem: &[u8], next_pc: u64) -> bool {
    nest_on() && matches!(mem.get(next_pc as usize), Some(0xc3) | Some(0xc2))
}

/// Nested dispatch at all: with function bodies about, unless
/// `GEX_NT_EXC_NEST=0`.
fn nest_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("GEX_NT_EXC_NEST").as_deref() != Ok("0")) && super::nt_cgen::mode() != super::nt_cgen::Mode::Off
}

/// Has control left the innermost nested exception dispatch on this thread
/// for `target`? It has when the stack is above the `NtRaiseException`
/// that started it: a handler's longjmp, a `catch` continuation or an
/// NtContinue went back to the frames the exception was raised under.
/// Then the state is the context's with PC = `target`, and the caller
/// unwinds to `raise_nested`'s loop (`CalloutSignal::Escape`). Called where
/// the context is complete: the fallback, a dispatch miss, a safepoint, a
/// syscall's resume.
pub(crate) fn exc_escape(caller: &mut Caller<'_, HostState>, target: u64) -> bool {
    let Some(&raise_sp) = caller.data().exc_levels.last() else { return false };
    let Some(memory) = caller.data().memory.clone() else { return false };
    let ctx_addr = caller.data().thread.ctx_addr;
    let mut gm = GuestMem::new(memory.bytes());
    let sp = gm.read_u64(ctx_addr + ctx::x(4) as u64).unwrap_or(0);
    let sp = if caller.data().nt.as_ref().is_some_and(|p| p.abi == nt::Abi::P32) { sp & 0xffff_ffff } else { sp };
    // At or above the syscall's own stack: nothing the dispatch runs is
    // there (its frame is below), so control is back with the frames it
    // was entered from (an APC's NtContinue comes back to exactly it).
    if sp < raise_sp {
        return false;
    }
    let _ = gm.write_u64(ctx_addr + ctx::PC as u64, target);
    let level = caller.data().exc_levels.len() - 1;
    caller.data_mut().callout_signal = Some(CalloutSignal::Escape(level));
    true
}

/// Dispatch an exception or a user APC without unwinding the thread's wasm
/// frames (`NtRaiseException`, first chance; an alertable wait that delivers
/// an APC): run `KiUserExceptionDispatcher` or `KiUserApcDispatcher`
/// (`target`) in a nested loop on top of them, as a user-mode callback
/// runs, until control goes back above the raise (`exc_escape`). Then the
/// syscall returns, its `ret` taking the guest to where the dispatch
/// ended: the frames below see an ordinary leave, and the one that owns the
/// address (a `setjmp`'s caller, the function a `catch` resumes in, the
/// raiser itself after EXCEPTION_CONTINUE_EXECUTION) carries on in its own
/// code (`gex_resume`). A trap-out instead drops every frame, and the
/// thread's outer functions (a game's main loop) run on the fallback from
/// then on.
fn raise_nested(caller: &mut Caller<'_, HostState>, p: &Arc<nt::Process>, target: u64, raise_sp: u64, next_pc: Option<u64>) -> Result<()> {
    let enter = super::any_enter(caller).ok_or_else(|| anyhow!("no module in this store exports `enter`"))?;
    let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let ctx_addr = caller.data().thread.ctx_addr;
    let p32 = p.abi == nt::Abi::P32;
    let mask = if p32 { 0xffff_ffff } else { !0u64 };
    if caller.data().exc_levels.len() >= EXC_MAX_DEPTH {
        let state = caller.data_mut();
        if crate::ftab::stats_on() {
            state.resume_cause = Some("NtRaiseException (nested too deep)".into());
        }
        state.resume_at = Some(target);
        return Err(StopExecution.into());
    }
    EXC_NESTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    caller.data_mut().exc_levels.push(raise_sp & mask);
    let me = caller.data().exc_levels.len() - 1;
    let mut pc = target;
    let mut next: Option<wasmtime::Func> = None;
    let result = loop {
        let r = match next.take() {
            Some(f) => f.call(&mut *caller, &[Val::I32(ctx_addr as i32)], &mut []),
            None => enter.call(&mut *caller, &[Val::I32(ctx_addr as i32), Val::I64(pc as i64)], &mut []),
        };
        if let Some(child) = caller.data_mut().pending_spawn.take() {
            let cx = caller.data().spawn.clone().ok_or_else(|| anyhow!("thread creation with no spawn context"))?;
            start_guest_thread(cx, child)?;
        }
        let e = match r {
            Ok(()) => break Err(anyhow!("guest code returned out of exception dispatch")),
            Err(e) => anyhow::Error::from(e),
        };
        if let Some(sig) = caller.data_mut().callout_signal.take() {
            match sig {
                CalloutSignal::Escape(i) if i == me => break Ok(()),
                CalloutSignal::Bypass(f) => {
                    next = Some(f);
                    continue;
                }
                // An outer dispatch's escape, or a return a call-out further
                // out waits for: theirs, with these frames.
                other => {
                    caller.data_mut().callout_signal = Some(other);
                    break Err(anyhow::Error::from(e));
                }
            }
        }
        if caller.data().exit.is_some() {
            break Err(anyhow::Error::from(e));
        }
        if let Some(at) = caller.data_mut().resume_at.take() {
            crate::ftab::stats::RESUMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            note_resume(caller.data_mut(), at, "exception dispatch");
            pc = at;
            continue;
        }
        if is_stale_table_trap(&e) {
            catch_up_in_caller(caller)?;
            pc = read_u64(memory.bytes(), (ctx_addr + ctx::PC as u64) as usize);
            continue;
        }
        // Anything else (an NtCallbackReturn ending a callback further out,
        // a failure) is for the loops below.
        break Err(anyhow::Error::from(e));
    };
    caller.data_mut().exc_levels.truncate(me);
    result?;
    EXC_ESCAPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // The guest goes on at the context's PC and SP. The syscall's `ret`
    // (or `ret n`) takes it there: the return address it pops is that PC,
    // just below the new stack (dead stack on x86: nothing lives below SP).
    let mut gm = GuestMem::new(memory.bytes());
    let to_pc = gm.read_u64(ctx_addr + ctx::PC as u64).unwrap_or(0);
    let to_sp = gm.read_u64(ctx_addr + ctx::x(4) as u64).unwrap_or(0) & mask;
    if p.cfg.trace {
        gex_kernel::nt_log!(
            "[nt:{:x}] {2} dispatched nested, back at {1} sp {to_sp:#x} (raised at sp {raise_sp:#x})",
            p.thread_by_ctx(ctx_addr).map_or(0, |t| t.tid),
            p.describe_addr(to_pc),
            if target == p.ntdll.lock().unwrap().ki_user_apc_dispatcher { "APC" } else { "exception" },
        );
    }
    // A fault: the faulting code goes on at the context's PC itself.
    let Some(next_pc) = next_pc else {
        return Ok(());
    };
    // Back at the syscall's own return (an APC's NtContinue, an exception
    // continued at the raise): just return, with the context's registers.
    if to_pc == next_pc && to_sp == raise_sp & mask {
        let _ = gm.write_u64(ctx_addr + ctx::PC as u64, next_pc);
        return Ok(());
    }
    let pops = match memory.bytes().get(next_pc as usize) {
        Some(0xc2) => gm.read_u32(next_pc + 1).unwrap_or(0) as u64 & 0xffff,
        _ => 0,
    };
    let psize = if p32 { 4 } else { 8 };
    let sp = to_sp.wrapping_sub(psize + pops) & mask;
    let wrote = if p32 { gm.write_u32(sp, to_pc as u32) } else { gm.write_u64(sp, to_pc) };
    if wrote.is_err() {
        // No stack there: resume the old way.
        caller.data_mut().resume_at = Some(to_pc);
        return Err(StopExecution.into());
    }
    let _ = gm.write_u64(ctx_addr + ctx::x(4) as u64, sp);
    let _ = gm.write_u64(ctx_addr + ctx::PC as u64, next_pc);
    Ok(())
}

pub(crate) use gex_kernel::nt::Fault;

/// [`fault`] from code that goes on after the import returns
/// (`gex_cgen::RESUMABLE_PC`): the exception is dispatched nested
/// (`raise_nested`), and the import returns with the state the handlers
/// left (the caller continues at `ctx::PC` through its `gex_resume`), so
/// the thread's wasm frames survive the fault as they do a raise.
pub(crate) fn fault_at(caller: &mut Caller<'_, HostState>, f: Fault, pc: u64, resumable: bool) -> Result<()> {
    if resumable && !matches!(f, Fault::X87) && nest_on() {
        let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
        let ctx_addr = caller.data().thread.ctx_addr;
        let sp = read_u64(memory.bytes(), (ctx_addr + ctx::x(4) as u64) as usize);
        let p = Arc::clone(caller.data().nt.as_ref().expect("an NT fault in a Linux process"));
        let (code, rip, info) = f.exception(pc).expect("every fault but X87 is an exception");
        let outcome = nt::hardware_exception(&p, memory.bytes(), ctx_addr, code, rip, &info);
        return match outcome {
            nt::Outcome::Resume(target) => {
                let r = raise_nested(caller, &p, target, sp, None);
                super::check_i386_high(&*caller, "a fault dispatched nested");
                r
            }
            nt::Outcome::Exit(code) => {
                caller.data_mut().exit = Some(Exit::Status(code as i32));
                Err(StopExecution.into())
            }
            other => bail!("gex-nt: unexpected outcome {other:?} from exception dispatch"),
        };
    }
    fault(caller, f, pc)
}

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
    let outcome = nt::hardware_exception(&p, memory.bytes(), state.thread.ctx_addr, code, rip, &info);
    match outcome {
        nt::Outcome::Resume(target) => {
            if crate::ftab::stats_on() {
                state.resume_cause = Some(format!("exception {code:#x} at {pc:#x}"));
            }
            state.resume_at = Some(target)
        }
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
    // The yield park on the wasm memory's own wait queue, so the guest's
    // `memory.atomic.notify` (the ConcRT provider's kicks) ends it too.
    fn park(&self, addr: u64, expected: u32, timeout: std::time::Duration) -> bool {
        match self.caller.data().memory.as_ref() {
            Some(m) => m.shared().atomic_wait32(addr, expected, Some(timeout)).is_ok(),
            None => false,
        }
    }

    fn notify(&self, addr: u64) {
        if let Some(m) = self.caller.data().memory.as_ref() {
            let _ = m.shared().atomic_notify(addr, u32::MAX);
        }
    }

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
    let enter = crate::any_enter(caller).ok_or_else(|| anyhow!("no module in this store exports `enter`"))?;
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
            Err(e) => anyhow::Error::from(e),
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
                    return Err(anyhow::Error::from(e));
                }
            }
        }
        if thread.callback_result.lock().unwrap().is_some() {
            return Ok(());
        }
        if let Some(next) = caller.data_mut().resume_at.take() {
            crate::ftab::stats::RESUMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            crate::note_resume(caller.data_mut(), next, "callback loop");
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
        return Err(anyhow::Error::from(e));
    }
}
