//! The native host: wasmtime plus the kernel bindings. A debug-only adapter;
//! the browser is the real target.

#![forbid(unsafe_code)]

mod lockstep;
use anyhow::{anyhow, bail, Context, Result};
use gex_abi::{ctx, exports, imports, GUEST_MEM_PAGES, IMPORT_MODULE};
use gex_kernel::mem::prot;
use gex_kernel::vfs::{DevFs, HostFs, ProcFs, TmpFs};
use gex_kernel::{BlockOp, GuestMem, Kernel, KernelProcSource, ProcState, SyscallOutcome, ThreadState};
use gex_ld::{build_initial_stack, Identity, Image, Program};
use gex_xlate::{Options, Stats};
use std::sync::{Arc, Mutex};
pub mod aot;
pub mod gl;
pub mod profile;
pub mod nt;
pub mod virgl;
pub mod wayland;

use wasmtime::{Caller, Config, Engine, Extern, Linker, Module, Ref, Store, Table, Val};

/// How a guest run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// `exit_group(status)`.
    Status(i32),
    /// A fatal signal.
    Signal {
        signal: i32,
        pc: u64,
        detail: String,
        /// True when the signal reflects a gex limitation, not the guest's own
        /// behaviour — those get a stderr message, an ordinary fatal signal
        /// does not.
        internal: bool,
    },
}

impl Exit {
    /// The status a shell would see.
    pub fn code(&self) -> i32 {
        match self {
            Exit::Status(c) => *c,
            Exit::Signal { signal, .. } => 128 + signal,
        }
    }

    /// The status `wait4` reports, in the kernel's encoding: signal in the low
    /// seven bits, exit code in bits 8..15. Not [`Exit::code`]'s `128 + signal`,
    /// which would make a killed child look like a normal exit to `WIFSIGNALED`.
    pub fn wait_status(&self) -> i32 {
        match self {
            Exit::Status(c) => (c & 0xff) << 8,
            // Whether a core would have been written is not modelled, and
            // `WIFSIGNALED` does not depend on it.
            Exit::Signal { signal, .. } => signal & 0x7f,
        }
    }
}

/// Most entry points a profile will remember. Large enough for a program the
/// size of SQLite to converge, small enough that the file stays trivial.
const PROFILE_LIMIT: usize = 1 << 18;

/// Back-edges between signal polls. Small enough that delivery is prompt,
/// large enough that the check is noise.
const SAFEPOINT_INTERVAL: i32 = 1024;

/// Signal numbers the runtime raises.
pub mod signal {
    pub const SIGILL: i32 = 4;
    pub const SIGTRAP: i32 = 5;
    pub const SIGFPE: i32 = 8;
    pub const SIGSEGV: i32 = 11;
    pub const SIGSYS: i32 = 31;
}

/// Options for a native run.
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub args: Vec<String>,
    pub env: Vec<String>,
    /// Print every syscall to stderr.
    pub trace: bool,
    /// Print translation statistics to stderr.
    pub verbose: bool,
    /// Host directory presented as the guest's root. Without one the guest
    /// sees the host filesystem directly.
    pub sysroot: Option<std::path::PathBuf>,
    /// Who the guest thinks it is. `None` reports the host's own ids, which a
    /// passthrough filesystem needs for consistent stat/getuid answers.
    pub identity: Option<(u32, u32)>,
    /// Sample the guest's back-edges and report the hottest at exit.
    pub sample: bool,
    /// Where each object of the program's closure must be placed, by guest
    /// path. See `Kernel::pins`: an AOT module is only correct if its object
    /// lands where the build assumed.
    pub pins: std::collections::HashMap<String, u64>,
    /// Report at exit where each pinned object actually landed.
    pub report_pins: bool,
    /// Run a Wayland compositor and bind it at this path in the guest, so a
    /// guest client can put a window on the screen. See `wayland`.
    pub wayland: Option<String>,
    /// Serve a virtio-gpu renderer on this socket name, so the guest's own
    /// Mesa can render on the host's GPU. See `virgl`.
    pub virgl: Option<String>,
    /// What size to tell a toplevel it is.
    pub wayland_size: (i32, i32),
    /// Where to write each committed frame, as a PPM. A window would be
    /// nicer; a file is what a test can compare.
    pub wayland_out: Option<String>,
    /// Composite on the GPU. Falls back to the CPU with a note if there is
    /// no adapter.
    pub wayland_gpu: bool,
    /// Give the guest a graphics device at `/dev/gex-gl`, so its GL calls run
    /// on the host's GL. See the `gl` module.
    pub gl: bool,
    /// Statically recompile each shared library as it is mapped, instead of
    /// letting the runtime translator find it a dispatch miss at a time. Off
    /// by default: measured slower (32s vs 0.60s warm on `dpkg --version`)
    /// because scanning seeds far more blocks than the program touches.
    pub recompile_objects: bool,
    /// Feed the previous run's discovered entry points into this one's
    /// translation. Off by default; see the note in `run_program_with`.
    pub profile: bool,
    /// Translate code on demand when a branch reaches an address with no
    /// translation.
    pub jit: bool,
    /// Reuse compiled modules across runs (PLAN §10).
    pub cache: bool,
    /// Guest directory to start in. Without one the guest inherits the host's
    /// working directory, or `/` when a sysroot is mounted.
    pub cwd: Option<String>,
    /// Record a profile and a session manifest into this directory (see
    /// `profile`). Tier-1 code only.
    pub record: Option<std::path::PathBuf>,
    /// An ejected project's prebuilt modules and library replacements (see
    /// `aot`). With it, an image is never translated: its module comes from
    /// the project, or the run fails.
    pub aot: Option<Arc<aot::Aot>>,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            args: Vec::new(),
            env: Vec::new(),
            trace: false,
            verbose: false,
            sysroot: None,
            identity: None,
            sample: false,
            pins: std::collections::HashMap::new(),
            report_pins: false,
            wayland: None,
            // On unless the build cannot render. Costs nothing until a client
            // connects (the renderer is brought up lazily). On by default
            // because Mesa's vtest winsys hangs forever in `read` when the
            // socket is missing rather than failing.
            virgl: virgl::VirglDevice::available()
                .then(|| virgl::DEFAULT_SOCKET.to_string()),
            wayland_size: (640, 480),
            wayland_out: None,
            wayland_gpu: false,
            gl: false,
            recompile_objects: false,
            profile: false,
            jit: false,
            cwd: None,
            // On by default: half a minute vs half a second on SQLite; a miss
            // costs only the write.
            cache: true,
            record: None,
            aot: None,
        }
    }
}

/// Host-side state threaded through every import.
struct HostState {
    /// Shared with every other guest thread. The fields that describe one
    /// thread rather than the process live in `thread` and are installed
    /// under the lock; see [`ThreadState`].
    kernel: Arc<Mutex<Kernel>>,
    /// This thread's identity.
    thread: ThreadState,
    memory: Option<gex_mem::GuestMemory>,
    /// Snapshot `/proc` reads from, refreshed before each syscall that could
    /// observe it.
    proc_state: Arc<Mutex<ProcState>>,
    /// Set when a host function decided the run must stop.
    exit: Option<Exit>,
    /// Set when a host function wants execution to continue elsewhere.
    resume_at: Option<u64>,
    /// Where the guest is spending its time, when anyone asked.
    sampler: Option<Arc<Sampler>>,
    /// The compositor, when the guest was given one.
    wayland: Option<wayland::Wayland>,
    virgl: Option<virgl::VirglDevice>,
    /// Everything needed to translate and install code at run time.
    jit: Option<Jit>,
    /// Everything a new guest thread needs to build a store of its own.
    spawn: Option<Arc<SpawnContext>>,
    /// Set by a `clone` that a host thread still has to be started for.
    pending_spawn: Option<ThreadState>,
    /// Set by a `clone` without CLONE_THREAD: a child process to build.
    pending_fork: Option<ForkRequest>,
    /// Set by `execve`: a program to replace this process with.
    pending_exec: Option<ExecRequest>,
    /// Raised past wasmtime's defaults. See [`store_limits`].
    store_limits: wasmtime::StoreLimits,
    /// This store's profile counters, when recording. See `profile`.
    prof: Option<profile::StoreProf>,
    /// Set for a Windows process: every `syscall` is an NT call. See `nt`.
    nt: Option<Arc<gex_kernel::nt::Process>>,
    /// This store's instances of an ejected project's replacement modules,
    /// made on the first call. See `aot`.
    repl: Option<aot::StoreRepl>,
}

/// What a process-creating `clone` asked for.
#[derive(Clone, Copy, Debug)]
struct ForkRequest {
    pid: i32,
    child_stack: u64,
    tls: Option<u64>,
}

/// What `execve` asked for.
#[derive(Clone, Debug)]
struct ExecRequest {
    path: String,
    argv: Vec<String>,
    envp: Vec<String>,
}

/// What a guest thread needs to start running.
///
/// A wasmtime store belongs to one host thread, so a new guest thread cannot
/// be handed the parent's: it builds its own and instantiates the same
/// modules into it. Everything here is either immutable or already shared.
struct SpawnContext {
    /// Set when the process is ending, so every other thread stops at its
    /// next safepoint instead of running on into a torn-down world.
    stopping: Arc<std::sync::atomic::AtomicBool>,
    /// How many spawned threads have not yet noticed.
    running: Arc<std::sync::atomic::AtomicUsize>,
    engine: Engine,
    /// The image module, instantiated fresh in each thread's store.
    image_module: Module,
    /// A linker with only the `gex.*` imports registered -- no store-bound
    /// definitions, so it can be cloned into another thread.
    linker: Linker<HostState>,
    memory: gex_mem::GuestMemory,
    /// The options and translation knobs this process started with, so a
    /// child that execs can pass them on.
    opts: RunOptions,
    xlate: Options,
    kernel: Arc<Mutex<Kernel>>,
    proc_state: Arc<Mutex<ProcState>>,
    /// Where compiled runtime translations are kept, passed to every store.
    jit_cache: Option<(std::path::PathBuf, String)>,
    /// Shared across threads and inherited by children, so a profile of a
    /// process tree is one profile.
    sampler: Option<Arc<Sampler>>,
    /// Inherited the same way, so a client that a shell forked reaches the
    /// same compositor its parent could.
    wayland: Option<wayland::Wayland>,
    virgl: Option<virgl::VirglDevice>,
    jit_shared: Option<Arc<Mutex<JitShared>>>,
    limits: gex_xlate::BlockLimits,
    /// Where this process's objects are, for naming profile counts.
    prof_proc: Option<Arc<profile::ProcProf>>,
    /// The NT process, for a Windows program's threads. See `nt`.
    nt: Option<Arc<gex_kernel::nt::Process>>,
}

/// Runtime translation state belonging to one store (PLAN §6.2, M3).
///
/// A wasm table belongs to its store, so each guest thread has its own and
/// fills it from the same modules. `replayed` is how far through the shared
/// registry this store has got.
struct Jit {
    shared: Arc<Mutex<JitShared>>,
    engine: Engine,
    linker: Linker<HostState>,
    /// Every dispatch table, in index order; `tables[0]` holds the miss span
    /// and everything a program this size needs. Several exist only because V8
    /// caps a table at 10M entries and the code `br_table`s between them;
    /// natively tables 1+ stay empty but must exist because the code names them.
    tables: Vec<Table>,
    /// Where compiled runtime translations are kept, and the build they
    /// belong to. `None` when the cache is off or this build cannot name
    /// itself.
    cache: Option<(std::path::PathBuf, String)>,
    /// Instances live here because the table holds funcrefs into them.
    instances: Vec<wasmtime::Instance>,
    /// Registry entries already instantiated into this store.
    replayed: usize,
    limits: gex_xlate::BlockLimits,
}

/// One runtime-translated module, kept so another thread's store can
/// instantiate it rather than translating the same code again.
struct CompiledUnit {
    module: Module,
    exported_blocks: Vec<(u64, String)>,
    dir_entries: Vec<(u64, u32)>,
    table_slots: u64,
    /// Page assignment as of this unit, which is what its blocks' slots were
    /// computed from.
    page_table: gex_xlate::PageTable,
}

/// Runtime translation state shared by every guest thread.
struct JitShared {
    /// Which guest pages own which table slots. Process-wide, because a
    /// page's directory entry is a single value in shared guest memory.
    page_table: gex_xlate::PageTable,
    /// Addresses whose dispatch slot now holds real code, in *some* store.
    translated: std::collections::HashSet<u64>,
    /// Every unit compiled so far, in installation order. Append-only, so a
    /// store catches up by replaying from its own watermark.
    registry: Vec<Arc<CompiledUnit>>,
    /// Which page owns each base slot, so the one thing that must never
    /// happen can be noticed if it ever does. See `publish_unit`.
    slot_owner: std::collections::HashMap<u32, u64>,
    /// Diagnostics, reported under --verbose.
    blocks: usize,
    calls: usize,
}

impl HostState {
    /// Run `f` with the shared kernel locked and describing this thread.
    ///
    /// Every path into the kernel goes through here, which is what makes the
    /// per-thread fields safe to keep in a shared structure: they are only
    /// ever read while the lock that installed them is held.
    fn with_kernel<R>(&mut self, f: impl FnOnce(&mut Kernel) -> R) -> R {
        let shared = Arc::clone(&self.kernel);
        let mut k = shared.lock().unwrap_or_else(|e| e.into_inner());
        k.enter_thread(&self.thread);
        let r = f(&mut k);
        k.leave_thread(&mut self.thread);
        r
    }
}

/// A trap used to unwind out of translated code, for both stopping and
/// redirecting. Redirecting works because the guest's registers live in the
/// context at every host call, so re-entering at a new PC loses nothing.
#[derive(Debug)]
struct StopExecution;

impl std::fmt::Display for StopExecution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "guest execution finished")
    }
}

impl std::error::Error for StopExecution {}

/// The cache key for an image: everything that determines the wasm — the
/// placed segments rather than the file, because that is what the translator
/// reads.
fn cache_key(image: &Image, xlate: &Options, build: &str) -> String {
    let mut fields: Vec<Vec<u8>> = vec![
        image.bias.to_le_bytes().into(),
        image.entry.to_le_bytes().into(),
        image.text_range.0.to_le_bytes().into(),
        image.text_range.1.to_le_bytes().into(),
        format!("{xlate:?}").into_bytes(),
        // The same bytes mean different instructions in 32-bit mode.
        vec![image.class32 as u8],
        // Symbols steer discovery, so two images with the same code but
        // different symbol tables translate differently.
        (image.symbols.len() as u64).to_le_bytes().into(),
    ];
    for seg in &image.segments {
        fields.push(seg.vaddr.to_le_bytes().into());
        fields.push(vec![seg.executable as u8]);
        fields.push(seg.bytes.clone());
    }
    gex_cache::key(build, fields.iter().map(|f| f.as_slice()))
}

/// Check that a module is well-formed wasm, without compiling or running it.
/// Cheap next to translating, so `gexc build` does it every time; catches
/// lifter bugs wasmtime would otherwise only reject deep into a module.
pub fn validate_module(wasm: &[u8]) -> Result<()> {
    let mut config = Config::new();
    config.wasm_tail_call(true);
    config.wasm_threads(true);
    config.wasm_bulk_memory(true);
    // The profile counters are a second memory (`profile`).
    config.wasm_multi_memory(true);
    let engine = Engine::new(&config).context("creating the wasm engine")?;
    Module::validate(&engine, wasm)
}

pub fn run_image(image: &Image, opts: &RunOptions, xlate: &Options) -> Result<(Exit, Stats)> {
    run_program(
        &Program {
            exe: image.clone(),
            interp: None,
        },
        opts,
        xlate,
    )
}

/// Run a program, which may carry a dynamic linker alongside it.
pub fn run_program(program: &Program, opts: &RunOptions, xlate: &Options) -> Result<(Exit, Stats)> {
    run_program_with(program, opts, xlate, None, None)
}

/// What survives an `execve`: the kernel, the process table, and the host-side
/// devices. The devices must be kept because rebinding their socket name would
/// leave a later client talking to a device the old process still services.
/// (`run_program_with` keeps the process — pid, descriptors, cwd — and replaces
/// the address space and program, which is the same work as starting fresh.)
struct Inherited {
    kernel: Kernel,
    proc_state: Arc<Mutex<ProcState>>,
    wayland: Option<wayland::Wayland>,
    virgl: Option<virgl::VirglDevice>,
}

fn run_program_with(
    program: &Program,
    opts: &RunOptions,
    xlate: &Options,
    inherited: Option<Inherited>,
    nt_setup: Option<&nt::NtSetup>,
) -> Result<(Exit, Stats)> {
    let image = &program.exe;
    // Before anything is translated: a 32-bit (i386) program's code is
    // decoded in compatibility mode, and the kernel below speaks its ABI.
    // One setting per host process -- the native runner keeps one translator
    // for every guest process, so a mixed tree is not supported here (the
    // browser has a shim per process).
    gex_xlate::set_compat(program.compat());

    // A profile records where a previous run went, feeding `extra_entries` so
    // discovery reaches entry points static analysis cannot. It changes the
    // cache key, so a run that discovers something new recompiles once.
    //
    // A build that cannot identify itself must not use the cache: reusing
    // another build's compiled code would serve stale translations.
    if let Some(dir) = &opts.record {
        profile::start(dir.clone(), opts)?;
    }
    // Profiled modules differ from plain ones, so they are cached apart.
    let build = gex_cache::build_fingerprint()
        .map(|b| if profile::active() { format!("{b}+profile") } else { b });
    let use_cache = opts.cache && build.is_some();
    let build = build.unwrap_or_default();
    let profile_dir = use_cache.then(gex_cache::profile_dir).flatten();
    let image_key = cache_key(image, &Options::default(), &build);
    let mut xlate = xlate.clone();
    // Off unless asked for: measured, the profile costs a recompile on the
    // second run and buys nothing measurable after it on programs this size.
    // Still written every run, so turning it on costs only the recompile.
    let use_profile = opts.profile;
    if let Some(dir) = profile_dir.as_ref().filter(|_| use_profile) {
        let learned = gex_cache::read_profile(dir, &image_key);
        if !learned.is_empty() && opts.verbose {
            eprintln!("[gex] profile: {} learned entry points", learned.len());
        }
        xlate.extra_entries.extend(learned);
        xlate.extra_entries.sort_unstable();
        xlate.extra_entries.dedup();
    }
    let xlate = &xlate;

    let phase = std::time::Instant::now();
    let mut config = Config::new();
    // Tail calls are load-bearing: the whole block-per-function design in
    // PLAN §2.4 depends on them.
    config.wasm_tail_call(true);
    config.wasm_threads(true);
    config.wasm_bulk_memory(true);
    // The profile counters are a second memory (`profile`).
    config.wasm_multi_memory(true);
    // gex's output is already optimised; Cranelift only adds register
    // allocation and peepholes, whose time dominates on a large program (half
    // a minute to compile SQLite vs under two seconds to translate).
    // `GEX_OPT_LEVEL` picks the trade.
    config.cranelift_opt_level(match std::env::var("GEX_OPT_LEVEL").as_deref() {
        Ok("none") => wasmtime::OptLevel::None,
        Ok("speed_and_size") => wasmtime::OptLevel::SpeedAndSize,
        _ => wasmtime::OptLevel::Speed,
    });
    let engine = Engine::new(&config).context("creating the wasm engine")?;

    // Only the executable is translated ahead of time; the interpreter and its
    // libraries arrive through runtime translation, because at build time we do
    // not know where they land. Translation is inside the cache miss path, so a
    // hit skips it and reports empty stats apart from the sidecar block count.
    let mut stats = Stats::default();
    let translate = || {
        let (m, s) = gex_xlate::translate_image(image, xlate);
        if opts.verbose {
            eprintln!(
                "[gex] translated {} blocks / {} instructions -> {} KiB wasm ({} dispatch \
                 slots, {} entry points from data, {} untranslatable blocks, {} dead flag \
                 statements)",
                s.blocks,
                s.instructions,
                s.wasm_bytes / 1024,
                m.table_slots,
                s.discovered_from_data,
                s.illegal_blocks,
                s.dead_flag_stmts,
            );
        }
        (m, s)
    };

    let (module, meta) = if let Some(aot) = &opts.aot {
        // An ejected project: the image was translated when it was built.
        let dir = use_cache.then(gex_cache::cache_dir).flatten();
        aot::image_module(aot, &engine, dir.as_deref().map(|d| (d, build.as_str())), image)?
    } else {
        match use_cache.then(gex_cache::cache_dir).flatten() {
        Some(dir) => {
            let key = cache_key(image, xlate, &build);
            let mut built: Option<Stats> = None;
            let loaded = gex_cache::load_or_compile(&engine, &dir, &key, || {
                let (m, s) = translate();
                built = Some(s);
                Ok((m.wasm.clone(), m.metadata().encode()))
            })?;
            if let Some(s) = built {
                stats = s;
            }
            let meta = gex_xlate::Metadata::decode(&loaded.metadata)
                .ok_or_else(|| anyhow!("cached module metadata is unreadable"))?;
            if opts.verbose {
                eprintln!(
                    "[gex] compiled module {} ({})",
                    if loaded.hit { "from cache" } else { "cached" },
                    dir.join(format!("{key}.cwasm")).display()
                );
            }
            (loaded.module, meta)
        }
        None => {
            let (m, s) = translate();
            stats = s;
            let meta = m.metadata();
            (
                Module::new(&engine, &m.wasm).context("validating translated wasm")?,
                meta,
            )
        }
        }
    };
    if stats.blocks == 0 {
        stats.blocks = meta.n_blocks;
    }
    if opts.verbose {
        eprintln!(
            "[gex] module ready in {:.2}s",
            phase.elapsed().as_secs_f64()
        );
    }
    let phase = std::time::Instant::now();

    let ctx_addr = gex_abi::ctx_addr(0);
    let sampler = sampler_for(opts);
    RECOMPILE_OBJECTS.store(
        opts.recompile_objects,
        std::sync::atomic::Ordering::Relaxed,
    );
    JIT_ON.store(opts.jit, std::sync::atomic::Ordering::Relaxed);
    DUMP_ABORT.store(
        std::env::var_os("GEX_DUMP_ABORT").is_some(),
        std::sync::atomic::Ordering::Relaxed,
    );
    WATCH_ON.store(
        std::env::var_os("GEX_WATCH").is_some(),
        std::sync::atomic::Ordering::Relaxed,
    );
    // The same directory the ahead-of-time modules use; the keys cannot
    // collide because they hash different things.
    let jit_cache = use_cache
        .then(gex_cache::cache_dir)
        .flatten()
        .map(|d| (d, build.clone()));
    // `/proc/self/exe` is the resolved program, not argv[0]: a shell that
    // re-executes itself relies on the difference.
    let exe_path = opts
        .args
        .first()
        .and_then(|a| std::fs::canonicalize(a).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| opts.args.first().cloned().unwrap_or_default());
    // A program inside the sysroot is named by its guest path: the host path
    // leads nowhere in the guest's namespace, and glibc's `$ORIGIN` handling
    // asserts on what it reads back through /proc/self/exe.
    let exe_path = match opts
        .sysroot
        .as_deref()
        .and_then(|r| std::fs::canonicalize(r).ok())
        .and_then(|r| exe_path.strip_prefix(r.to_string_lossy().as_ref()).map(str::to_string))
    {
        Some(rest) if !rest.is_empty() => {
            if rest.starts_with('/') { rest } else { format!("/{rest}") }
        }
        _ => exe_path,
    };
    let fresh = inherited.is_none();
    // Taken before the kernel is moved out, because the setup below uses them
    // to decide whether to build a device or keep the one already bound.
    let (kept_wayland, kept_virgl) = match &inherited {
        Some(i) => (i.wayland.clone(), i.virgl.clone()),
        None => (None, None),
    };
    let (kernel, proc_state) = match inherited {
        Some(Inherited {
            kernel: mut k,
            proc_state: ps,
            ..
        }) => {
            // The address space is new, so the map of it starts empty. Every
            // other part of the kernel is what exec is defined to keep.
            k.mem = gex_kernel::mem::MemoryMap::new(image.brk);
            k.ctx_addr = ctx_addr;
            // `exec_program` already named the file it loaded, as a guest path;
            // argv[0] (all `exe_path` has here) may be a bare `wc`.
            k.argv = opts.args.clone();
            k.envp = opts.env.clone();
            k.live_threads = 1;
            k.next_ctx_slot = 1;
            (k, ps)
        }
        None => (
            {
                let mut k = Kernel::new(ctx_addr, image.brk, exe_path.clone());
                k.pins = opts.pins.clone();
                k
            },
            // `..Default::default()` for the rest: listing every field means a
            // new `ProcState` field breaks only this native site.
            Arc::new(Mutex::new(ProcState {
                pid: 1,
                ppid: 0,
                pgid: 1,
                sid: 1,
                exe: exe_path,
                cwd: "/".into(),
                ..Default::default()
            })),
        ),
    };
    // Report the host's identity: with hostfs passthrough, a guest that
    // stats a file and then calls getuid must see a consistent answer.
    let (uid, gid) = opts.identity.unwrap_or_else(host_identity);
    let mut kernel = Kernel {
        trace: opts.trace,
        uid,
        gid,
        argv: opts.args.clone(),
        envp: opts.env.clone(),
        ..kernel
    };
    // A 32-bit (i386) program: the kernel speaks its ABI and the translator
    // decodes its code in compatibility mode, for this process and the ones
    // it forks. One setting per host process: the native runner keeps one
    // translator for every guest process, so a mixed tree is not supported
    // here (the browser has a shim per process).
    kernel.compat = program.compat();
    // An ejected project's replacement code has its own region. Recorded for
    // each new address space, an exec's included.
    if let Some(aot) = &opts.aot {
        aot::reserve_region(&mut kernel, aot);
    }
    // Mounts survive an exec, so they are only established for a new process.
    if fresh {
        mount_filesystems(&mut kernel, opts, Arc::clone(&proc_state))?;
    }
    // The compositor binds a name in the guest's unix-socket namespace (shared
    // across the process tree). Bound before anything runs, or a client that
    // connects first gets ECONNREFUSED and gives up.
    let compositor = kept_wayland.or_else(|| opts.wayland.as_ref().map(|path| {
        let w = wayland::Wayland::new(opts.wayland_size.0, opts.wayland_size.1);
        if let Some(out) = &opts.wayland_out {
            w.write_frames_to(out);
        }
        if opts.wayland_gpu {
            match w.enable_gpu(opts.wayland_size.0, opts.wayland_size.1) {
                Ok(adapter) => {
                    if opts.verbose {
                        eprintln!("[gex] compositing on {adapter}");
                    }
                }
                // Not fatal. The guest still gets a display; the host just
                // does the drawing itself.
                Err(e) => eprintln!("gex: no GPU compositor ({e}); using the CPU"),
            }
        }
        kernel
            .socket_names
            .bind_host(path, Arc::new(w.clone()) as Arc<dyn gex_kernel::unix::HostListener>);
        w
    }));
    // The same shape, and bound just as early: Mesa connects while it is
    // still working out which driver it has, long before the first draw.
    let renderer = kept_virgl.or_else(|| opts.virgl.as_ref().map(|path| {
        if !virgl::VirglDevice::available() {
            eprintln!(
                "gex: this build has no libvirglrenderer, so {path} will accept \
                 connections and answer nothing"
            );
        }
        let v = virgl::VirglDevice::new(opts.verbose);
        kernel
            .socket_names
            .bind_host(path, Arc::new(v.clone()) as Arc<dyn gex_kernel::unix::HostListener>);
        v
    }));
    let tid = kernel.pid;
    let prof_proc = profile::new_process(&kernel, program, kernel.pid, &opts.args, &opts.env);

    let mut store = Store::new(
        &engine,
        HostState {
            kernel: Arc::new(Mutex::new(kernel)),
            thread: ThreadState::new(ctx_addr, tid),
            memory: None,
            proc_state,
            exit: None,
            resume_at: None,
            sampler: sampler.clone(),
            wayland: compositor.clone(),
        virgl: renderer.clone(),
            jit: None,
            spawn: None,
            pending_spawn: None,
            pending_fork: None,
            pending_exec: None,
            store_limits: store_limits(),
            prof: None,
            nt: None,
            repl: None,
        },
    );
    // Without this the store keeps wasmtime's default of ten thousand
    // instances, and gex spends one per runtime translation.
    store.limiter(|s| &mut s.store_limits);

    let mut linker = Linker::new(&engine);
    register_imports(&mut linker)?;
    // Kept before any store-bound definition is added, so a thread can clone
    // it and define its own memory and table.
    let base_linker = linker.clone();

    // The guest's memory belongs to the host, not to any module: every guest
    // thread runs in its own store, and a module-defined memory would give
    // each of them a private copy of the address space.
    let shared = wasmtime::SharedMemory::new(
        &engine,
        wasmtime::MemoryType::shared(GUEST_MEM_PAGES as u32, GUEST_MEM_PAGES as u32),
    )
    .context("creating the guest's shared memory")?;
    linker.define(&mut store, IMPORT_MODULE, exports::MEMORY, shared.clone())?;
    profile::link(&mut linker, &mut store, &prof_proc)?;

    let instance = linker
        .instantiate(&mut store, &module)
        .context("instantiating the translated module")?;
    profile::attach(&mut store, &instance)?;
    if opts.verbose {
        eprintln!(
            "[gex] instantiated in {:.2}s",
            phase.elapsed().as_secs_f64()
        );
    }
    let phase = std::time::Instant::now();

    let memory = gex_mem::GuestMemory::new(shared.clone());
    store.data_mut().memory = Some(memory.clone());

    // Runtime translation needs the table to patch and the engine and linker
    // to build modules with; holding them here turns a dispatch miss into a
    // translation.
    let tables = dispatch_tables(&mut store, &instance)?;
    let table = tables[0];
    // An ejected dynamically linked program's interpreter and libraries,
    // built ahead of time: they go in through the same registry runtime
    // translation uses, so it has to exist even with that off.
    let aot_shared = match &opts.aot {
        Some(aot) => {
            let dir = use_cache.then(gex_cache::cache_dir).flatten();
            aot::shared_modules(aot, &engine, dir.as_deref().map(|d| (d, build.as_str())), image)?
        }
        None => Vec::new(),
    };
    // The machinery here installs a translated module into this store. The
    // miss handler is one user; static object recompilation is another.
    // `--no-jit` turns off the miss handler, not this.
    if opts.jit || opts.recompile_objects || !aot_shared.is_empty() {
        store.data_mut().jit = Some(Jit {
            shared: Arc::new(Mutex::new(JitShared {
                page_table: gex_xlate::PageTable::from_entries(&meta.dir_entries, meta.next_slot),
                translated: Default::default(),
                registry: Vec::new(),
                blocks: 0,
                calls: 0,
                slot_owner: Default::default(),
            })),
            engine: engine.clone(),
            linker: linker.clone(),
            tables,
            cache: jit_cache.clone(),
            instances: Vec::new(),
            replayed: 0,
            limits: xlate.limits,
        });
    }
    if !aot_shared.is_empty() {
        aot::install_shared(&mut store, aot_shared)?;
    }

    let entry = match nt_setup {
        Some(setup) => nt::boot(&mut store, memory.clone(), setup)?,
        None => {
            place_image(
                &mut store,
                memory.clone(),
                program,
                opts,
                Identity {
                    uid,
                    gid,
                    interp_base: program.interp_base(),
                    entry_hook: 0,
                },
            )?;
            program.entry()
        }
    };

    // Publish the image's page-directory entries, without which none of its
    // blocks are reachable through the table.
    {
        let data = memory.bytes();
        for (page, base) in &meta.dir_entries {
            let off = (gex_abi::DISPATCH_DIR + page * 4) as usize;
            if off + 4 <= data.len() {
                data[off..off + 4].copy_from_slice(&base.to_le_bytes());
            }
        }
    }

    if opts.verbose {
        eprintln!(
            "[gex] guest state placed in {:.2}s",
            phase.elapsed().as_secs_f64()
        );
    }
    // Everything a new guest thread needs. The linker here is the pristine
    // one -- only the `gex.*` imports -- because a linker carrying store-bound
    // definitions could not be cloned into another thread.
    let spawn = Arc::new(SpawnContext {
        jit_cache: jit_cache.clone(),
        sampler: sampler.clone(),
        wayland: compositor.clone(),
        virgl: renderer.clone(),
        stopping: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        running: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        engine: engine.clone(),
        image_module: module.clone(),
        linker: base_linker,
        memory: memory.clone(),
        opts: opts.clone(),
        xlate: xlate.clone(),
        kernel: Arc::clone(&store.data().kernel),
        proc_state: Arc::clone(&store.data().proc_state),
        jit_shared: store.data().jit.as_ref().map(|j| Arc::clone(&j.shared)),
        limits: xlate.limits,
        prof_proc: prof_proc.clone(),
        nt: store.data().nt.clone(),
    });
    store.data_mut().spawn = Some(Arc::clone(&spawn));

    // The interpreter is placed by gex's own loader, not by a guest `mmap`,
    // so nothing announces it and the mapping hook never sees it. Without
    // this it is the one object that is never statically recompiled -- and it
    // is the one the guest starts in, so with the runtime translator off the
    // program dies on its first instruction.
    if opts.recompile_objects {
        if let Some(interp) = &program.interp {
            for seg in interp.segments.iter().filter(|s| s.executable) {
                let map = gex_kernel::CodeMap {
                    start: seg.vaddr,
                    end: seg.vaddr + seg.bytes.len() as u64,
                    path: program
                        .exe
                        .interp
                        .clone()
                        .unwrap_or_else(|| "[interp]".to_string()),
                };
                // Empty seeds: scan the whole segment. The interpreter's
                // functions are almost all local and exported nowhere, so its
                // symbol table covers a fraction of it -- and it is small
                // enough that scanning is affordable where it is not for libc.
                if let Err(e) = recompile_object(&mut store, &map, &[]) {
                    eprintln!("gex: could not recompile the interpreter: {e:#}");
                }
            }
        }
    }

    let stopped = run_thread_loop(&mut store, &instance, ctx_addr, entry)?;
    profile::harvest(&mut store)?;
    if let Stopped::Exec(req) = stopped {
        stop_other_threads(&spawn, opts.verbose);
        let kernel = take_kernel(&spawn);
        return exec_program(
            req,
            opts,
            xlate,
            kernel,
            Arc::clone(&spawn.proc_state),
            spawn.wayland.clone(),
            spawn.virgl.clone(),
        );
    }
    let Stopped::Exited(exit) = stopped else {
        unreachable!()
    };

    // The process is over. Every other thread must stop before the stores and
    // memory go away: set the stopping flag and wake anyone parked on a futex,
    // then give them a bounded moment to notice.
    stop_other_threads(&spawn, opts.verbose);

    if let Some(s) = &sampler {
        // Through the vma table, so a hot address in libapt says so rather
        // than printing as `?`.
        let vmas = {
            let k = store.data().kernel.lock().unwrap_or_else(|e| e.into_inner());
            k.mem.vmas().to_vec()
        };
        s.report_mapped(&vmas, opts.sysroot.as_deref().and_then(|p| p.to_str()), 20);
    }

    // Where the pinned objects actually landed. A pin that silently does not
    // take means code running at the wrong address, so report each one: the
    // `mmap` hint is only honoured when the range is free.
    if opts.report_pins {
        let landed = {
            let k = store.data().kernel.lock().unwrap_or_else(|e| e.into_inner());
            k.pin_landed.clone()
        };
        let mut wrong = 0;
        for (path, want, got) in &landed {
            if want == got {
                eprintln!("[pin] {path} at {want:#x}");
            } else {
                wrong += 1;
                eprintln!("[pin] {path} WANTED {want:#x} GOT {got:#x}");
            }
        }
        let asked = opts.pins.len();
        if landed.len() < asked {
            eprintln!(
                "[pin] {} of {} objects were never reserved",
                asked - landed.len(),
                asked
            );
        }
        if wrong > 0 {
            eprintln!("[pin] {wrong} landed somewhere else; a build against these is not valid");
        }
    }

    if let Some(jit) = store.data().jit.as_ref() {
        let (blocks, calls, translated) = {
            let sh = jit.shared.lock().unwrap_or_else(|e| e.into_inner());
            (sh.blocks, sh.calls, sh.translated.clone())
        };
        if opts.verbose {
            eprintln!("[gex] runtime translation: {blocks} blocks in {calls} compilations");
        }
        if let Some(dir) = &profile_dir {
            // Only addresses in the executable's own image are worth
            // recording: a shared library lands wherever the interpreter put
            // it, and ahead-of-time translation of the executable cannot
            // reach it anyway.
            let (lo, hi) = image.text_range;
            let found: Vec<u64> = translated
                .iter()
                .copied()
                .filter(|a| *a >= lo && *a < hi)
                .collect();
            gex_cache::write_profile(dir, &image_key, &found, PROFILE_LIMIT);
        }
    }
    Ok((exit, stats))
}

/// Bring every other guest thread to a halt after the process has exited.
fn stop_other_threads(spawn: &SpawnContext, verbose: bool) {
    use std::sync::atomic::Ordering;
    let started = std::time::Instant::now();
    spawn.stopping.store(true, Ordering::Release);
    {
        let k = spawn.kernel.lock().unwrap_or_else(|e| e.into_inner());
        k.futexes.wake_everything();
    }
    let deadline = started + std::time::Duration::from_millis(500);
    while spawn.running.load(Ordering::Acquire) > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    if verbose {
        let left = spawn.running.load(Ordering::Acquire);
        eprintln!(
            "[gex] stopped other threads in {:.0}ms{}",
            started.elapsed().as_secs_f64() * 1000.0,
            if left > 0 {
                format!(", {left} did not stop in time")
            } else {
                String::new()
            }
        );
    }
}

/// What one store may hold. wasmtime defaults to 10,000 instances, but gex
/// adds one per runtime translation, so the default bounds how much guest code
/// a process may execute (and overrunning it surfaces as a guest SIGSEGV).
/// Raised far above anything real; tables and memories with it, since each
/// instance may bring one.
fn store_limits() -> wasmtime::StoreLimits {
    wasmtime::StoreLimitsBuilder::new()
        .instances(1 << 20)
        .tables(1 << 20)
        .memories(1 << 20)
        .build()
}

/// Run a syscall's blocking half outside the kernel lock.
///
/// `service` runs on every turn so host devices keep working while the guest
/// sleeps — without it the compositor deadlocks. `interrupted` reports a
/// pending signal so a blocking syscall gives up with `EINTR` rather than
/// running its full wait.
fn perform_block(
    op: BlockOp,
    data: &mut [u8],
    service: &dyn Fn(&mut [u8]),
    interrupted: &dyn Fn() -> bool,
) -> Result<u64, gex_kernel::errno::Errno> {
    use gex_kernel::errno::Errno;
    /// One turn of a wait loop: let the host do its work, then give up if a
    /// signal has arrived.
    macro_rules! turn {
        () => {
            if interrupted() {
                return Err(Errno::EINTR);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
    }
    match op {
        BlockOp::FutexWait {
            table,
            uaddr,
            val,
            deadline,
            // The native host closes the check/sleep race with the queue lock
            // (below), so it does not use the kernel's pre-snapshotted place.
            parked: _,
        } => {
            // The re-read has to happen under the queue's own lock, which is
            // what closes the race between checking the word and sleeping.
            let mem = GuestMem::new(data);
            table
                .wait(uaddr, val, deadline, &|| mem.read_u32(uaddr).ok())
                .map(|_| 0)
        }
        BlockOp::Sleep(d) => {
            // In slices, so a signal ends it. A flat `sleep(d)` is why
            // `nanosleep` ran its full five seconds against a one-second
            // alarm while QEMU returned `EINTR` after one.
            let start = std::time::Instant::now();
            while start.elapsed() < d {
                if interrupted() {
                    return Err(Errno::EINTR);
                }
                // Saturating: the deadline can pass between the test above
                // and here, and `Duration` subtraction panics on underflow.
                let left = d.saturating_sub(start.elapsed());
                std::thread::sleep(left.min(std::time::Duration::from_millis(1)));
            }
            Ok(0)
        }
        // Both are polls; natively a real thread sleeps here, costing nobody.
        BlockOp::Connect { socket } => loop {
            match socket.status() {
                gex_kernel::net::Status::Connecting => {
                    turn!();
                }
                gex_kernel::net::Status::Open => return Ok(0),
                gex_kernel::net::Status::Refused => return Err(Errno::ECONNREFUSED),
                gex_kernel::net::Status::Closed => return Err(Errno::ECONNRESET),
            }
        },
        BlockOp::Poll {
            fds,
            deadline,
            report,
        } => {
            let start = std::time::Instant::now();
            loop {
                service(data);
                if gex_kernel::syscalls::poll_ready(&fds) {
                    let mut mem = GuestMem::new(data);
                    return gex_kernel::syscalls::poll_finish(&fds, &report, &mut mem);
                }
                if deadline.is_some_and(|d| start.elapsed() >= d) {
                    let mut mem = GuestMem::new(data);
                    return gex_kernel::syscalls::poll_finish(&fds, &report, &mut mem);
                }
                turn!();
            }
        }
        BlockOp::WriteStream { handle, buf, len } => {
            let data = GuestMem::new(data).read(buf, len)?.to_vec();
            let mut done = 0usize;
            while done < data.len() {
                if !handle.poll_writable() {
                    // A partial write already delivered is the caller's
                    // answer; only an untouched write reports the signal.
                    if interrupted() {
                        return if done > 0 { Ok(done as u64) } else { Err(Errno::EINTR) };
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
                match handle.write_at(0, &data[done..]) {
                    Ok(0) => std::thread::sleep(std::time::Duration::from_millis(1)),
                    Ok(n) => done += n as usize,
                    // A partial write already delivered is what the caller
                    // gets; the error belongs to the next call.
                    Err(_) if done > 0 => break,
                    Err(e) => return Err(e),
                }
            }
            Ok(done as u64)
        }
        BlockOp::ReadStream { handle, buf, len } => {
            let mut tmp = vec![0u8; len];
            loop {
                service(data);
                if handle.poll_readable() {
                    let n = handle.read_at(0, &mut tmp)?;
                    let mut mem = GuestMem::new(data);
                    mem.write(buf, &tmp[..n as usize])?;
                    return Ok(n);
                }
                turn!();
            }
        }
        BlockOp::SigSuspend { .. } => loop {
            // Nothing to poll but the signal state, which `interrupted` is.
            service(data);
            if interrupted() {
                return Err(Errno::EINTR);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        },
        BlockOp::AcceptWait { socket } => {
            // Only the wait: the descriptor is made by the kernel afterwards,
            // which is the one place that has a descriptor table.
            loop {
                service(data);
                if socket.poll_readable() {
                    return Ok(0);
                }
                turn!();
            }
        }
        BlockOp::WaitChild {
            children,
            procs,
            want,
            wstatus,
            nohang,
            // The children list passed in is already this process's, so ppid
            // is redundant here.
            ppid: _,
        } => {
            let candidates: Vec<_> = children
                .iter()
                .filter(|c| want <= 0 || c.pid == want)
                .collect();
            if candidates.is_empty() {
                return Err(Errno::ECHILD);
            }
            loop {
                for c in &candidates {
                    let mut st = c.status.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(code) = st.take() {
                        c.reaped.store(true, std::sync::atomic::Ordering::Release);
                        // Collected, so the table can forget it. Without this
                        // an init that respawns for the life of the machine
                        // grows a record per restart.
                        procs.reap(c.pid);
                        if wstatus != 0 {
                            // Already in the kernel's encoding: see
                            // `Exit::wait_status`.
                            let mut mem = GuestMem::new(data);
                            mem.write_u32(wstatus, code as u32)?;
                        }
                        return Ok(c.pid as u64);
                    }
                }
                if nohang {
                    return Ok(0);
                }
                // A halt reaches a parked `wait4` here; `EINTR` sends it back
                // to a syscall where the halt check stops it.
                if procs.halt_code().is_some() {
                    return Err(Errno::EINTR);
                }
                // And an ordinary signal, which is what a shell's `timeout`
                // relies on: it parks in `wait4` and expects `SIGALRM` to
                // bring it back so it can kill the child.
                if interrupted() {
                    return Err(Errno::EINTR);
                }
                // Any child's condvar will do to park on; they are woken
                // together with the status they set.
                let c = candidates[0];
                let guard = c.status.lock().unwrap_or_else(|e| e.into_inner());
                let _ = c
                    .done
                    .wait_timeout(guard, std::time::Duration::from_millis(20));
            }
        }
    }
}

/// Run one guest thread until it stops.
///
/// A loop rather than a single call because a signal, a `sigreturn`, a
/// freshly translated block or a `clone` all redirect execution by unwinding
/// out of wasm and re-entering at a new address. `clone` in particular has to
/// start its host thread from here: doing it inside the syscall import would
/// mean spawning while a wasm frame is live and the store is borrowed.
enum Stopped {
    Exited(Exit),
    /// The guest called `execve`; the caller must build the new program.
    Exec(ExecRequest),
}

fn run_thread_loop(
    store: &mut Store<HostState>,
    instance: &wasmtime::Instance,
    ctx_addr: u64,
    entry: u64,
) -> Result<Stopped> {
    let enter = instance
        .get_func(&mut *store, exports::ENTER)
        .ok_or_else(|| anyhow!("translated module does not export `enter`"))?;
    let mut pc = entry;
    loop {
        let result = enter.call(
            &mut *store,
            &[Val::I32(ctx_addr as i32), Val::I64(pc as i64)],
            &mut [],
        );
        if let Some(req) = store.data_mut().pending_fork.take() {
            fork_process(store, req)?;
        }
        if let Some(child) = store.data_mut().pending_spawn.take() {
            let cx = store
                .data()
                .spawn
                .clone()
                .ok_or_else(|| anyhow!("clone with no spawn context"))?;
            // Detached: a guest thread is joined through its own futex, not
            // through the host's handle, and `exit_group` ends the process
            // whatever its threads are doing.
            cx.running.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let running = Arc::clone(&cx.running);
            std::thread::Builder::new()
                .name(format!("gex-tid-{}", child.tid))
                .spawn(move || {
                    if let Err(e) = run_guest_thread(Arc::clone(&cx), child) {
                        eprintln!("gexc: thread {} failed: {e:#}", child.tid);
                        // A thread that fails takes its process with it, as on
                        // Linux: otherwise the other threads sit in a futex
                        // forever and every descriptor it holds stays open.
                        stop_other_threads(&cx, false);
                        cx.kernel
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .fds
                            .close_all();
                    }
                    running.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                })
                .context("spawning a guest thread")?;
        }
        if let Some(req) = store.data_mut().pending_exec.take() {
            return Ok(Stopped::Exec(req));
        }
        match result {
            // Falling off the end of translated code means the guest returned
            // from its entry point, which Linux treats as an exit.
            Ok(()) => {
                return Ok(Stopped::Exited(
                    store.data().exit.clone().unwrap_or(Exit::Status(0)),
                ))
            }
            Err(e) => {
                if let Some(next) = store.data_mut().resume_at.take() {
                    pc = next;
                    continue;
                }
                if let Some(x) = store.data().exit.clone() {
                    return Ok(Stopped::Exited(x));
                }
                // A dispatch through a slot this thread's table has not reached
                // yet. Every thread shares the dispatch directory (in guest
                // memory) but has its own table, so one thread can publish a
                // slot another's table is still too short for. Recoverable
                // exactly: both indirect transfers write the target into the
                // context before taking it, so no progress was lost.
                if is_stale_table_trap(&e) {
                    if let Some(next) = catch_up_and_resume(store, ctx_addr)? {
                        pc = next;
                        continue;
                    }
                }
                return Err(classify_trap(e, store));
            }
        }
    }
}

/// Build and start a child process.
///
/// A forked child is the same program at the same addresses, so it reuses
/// every module the parent translated. What it cannot share is the memory: it
/// gets a fresh one with the parent's mapped regions copied in — one address
/// space per process, no copy-on-write, since wasm cannot fault on a write.
fn fork_process(store: &mut Store<HostState>, req: ForkRequest) -> Result<()> {
    let pid = req.pid;
    let cx = store
        .data()
        .spawn
        .clone()
        .ok_or_else(|| anyhow!("fork with no spawn context"))?;
    let parent_mem = store
        .data()
        .memory
        .clone()
        .ok_or_else(|| anyhow!("guest memory unavailable"))?;

    // The child's kernel is a copy of the parent's: descriptors are `Arc`
    // handles, so parent and child share the open file descriptions exactly
    // as Linux does, while the VMA tree and the child list are its own.
    let child_proc = Arc::new(Mutex::new(ProcState::default()));
    let (mut child_kernel, regions, ctx_addr) = {
        let k = cx.kernel.lock().unwrap_or_else(|e| e.into_inner());
        let regions: Vec<(u64, u64)> = k.mem.vmas().iter().map(|v| (v.start, v.end)).collect();
        let mut child = k.fork_kernel();
        child.pid = pid;
        child.tid = pid;
        (child, regions, store.data().thread.ctx_addr)
    };

    let shared = wasmtime::SharedMemory::new(
        &cx.engine,
        wasmtime::MemoryType::shared(GUEST_MEM_PAGES as u32, GUEST_MEM_PAGES as u32),
    )
    .context("creating the child's memory")?;
    let child_mem = gex_mem::GuestMemory::new(shared);
    {
        let src = parent_mem.bytes();
        let dst = child_mem.bytes();
        // Everything below the executable: the register contexts, and the
        // dispatch directory that says which table slots a page owns. The
        // directory begins *at* RESERVED_END, not before it -- copying only up
        // to that boundary leaves the child with an all-zero directory, so
        // every page resolves to the miss stub and the first branch after the
        // fork translates the same block forever.
        let low = (gex_abi::DISPATCH_DIR + gex_abi::DISPATCH_MAX_PAGES * 4) as usize;
        dst[..low].copy_from_slice(&src[..low]);
        for (start, end) in &regions {
            let (a, b) = (*start as usize, (*end as usize).min(src.len()));
            if a < b {
                dst[a..b].copy_from_slice(&src[a..b]);
            }
        }
        // `fork` returns zero in the child, and a clone that named a stack
        // starts on it.
        let mut gm = GuestMem::new(dst);
        let _ = gm.write_u64(ctx_addr + ctx::x(0) as u64, 0);
        if req.child_stack != 0 {
            let _ = gm.write_u64(ctx_addr + ctx::SP as u64, req.child_stack);
        }
        if let Some(tls) = req.tls {
            let _ = gm.write_u64(ctx_addr + ctx::TPIDR as u64, tls);
        }
        // A monitor is per-thread; the child's starts clear.
        let _ = gm.write_u64(ctx_addr + ctx::MON_SIZE as u64, 0);
    }

    let entry = {
        let data = child_mem.bytes();
        read_u64(data, (ctx_addr + ctx::PC as u64) as usize)
    };
    // The child's VFS was cloned from the parent's, and that clone included
    // the parent's `/proc` -- so without this the child, and everything it
    // goes on to exec, reads its ancestor's pid, maps and command line from
    // `/proc/self`. Each process gets its own.
    child_kernel.vfs.mount(
        "/proc",
        Arc::new(ProcFs::new(Arc::new(KernelProcSource(Arc::clone(&child_proc))))),
    );
    let child_cx = Arc::new(SpawnContext {
        jit_cache: cx.jit_cache.clone(),
        // One profile for the whole process tree: a package manager spends
        // most of its time in programs it started.
        sampler: cx.sampler.clone(),
        wayland: cx.wayland.clone(),
        virgl: cx.virgl.clone(),
        stopping: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        running: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        engine: cx.engine.clone(),
        image_module: cx.image_module.clone(),
        linker: cx.linker.clone(),
        memory: child_mem,
        opts: cx.opts.clone(),
        xlate: cx.xlate.clone(),
        kernel: Arc::new(Mutex::new(child_kernel)),
        proc_state: child_proc,
        // The registry is shared: the modules are the same code at the same
        // addresses, and instantiating one against a different memory is
        // exactly what the import is for.
        jit_shared: cx.jit_shared.clone(),
        limits: cx.limits,
        prof_proc: profile::fork_process(&cx.prof_proc),
        // Windows has no fork: only a Linux process gets here.
        nt: None,
    });
    let status = {
        let k = cx.kernel.lock().unwrap_or_else(|e| e.into_inner());
        k.procs
            .handle(pid)
            .map(|c| (Arc::clone(&c.status), Arc::clone(&c.done)))
            .ok_or_else(|| anyhow!("forked child {pid} is not registered"))?
    };
    let procs = {
        let k = cx.kernel.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(&k.procs)
    };
    let thread = ThreadState::new(ctx_addr, pid);
    std::thread::Builder::new()
        .name(format!("gex-pid-{pid}"))
        .spawn(move || {
            let cleanup = Arc::clone(&child_cx);
            // Two numbers, deliberately: the process table keeps the one a
            // shell would print, and `wait4` gets the kernel's encoding.
            let (code, wstatus) = match run_guest_process(child_cx, thread, entry) {
                Ok(exit) => (exit.code(), exit.wait_status()),
                Err(e) => {
                    eprintln!("gexc: child {pid} failed: {e:#}");
                    // A process that dies takes its threads and descriptors
                    // with it: otherwise its threads park in a futex nobody
                    // wakes and its pipes never reach EOF for their readers.
                    stop_other_threads(&cleanup, false);
                    cleanup
                        .kernel
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .fds
                        .close_all();
                    (127, 127 << 8)
                }
            };
            if std::env::var_os("GEX_TRACE_EXIT").is_some() {
                eprintln!("[gex:{pid}] process finished: code {code}, wait status {wstatus:#x}");
            }
            // Through the table, so this process's own children -- if any are
            // still running -- become init's rather than being lost. The table
            // publishes into the same cell `wait4` collects from, so it gets
            // the kernel's encoding: handing it `code` and correcting the cell
            // afterwards let a parent parked in `wait4` read `exit 7` as a
            // death by signal 7 ("Bus error") in between.
            procs.exit(pid, wstatus);
            let (st, done) = status;
            *st.lock().unwrap_or_else(|e| e.into_inner()) = Some(wstatus);
            done.notify_all();
        })
        .context("spawning a child process")?;
    Ok(())
}

/// Run a child process to completion in this host thread.
fn run_guest_process(cx: Arc<SpawnContext>, thread: ThreadState, entry: u64) -> Result<Exit> {
    let mut store = Store::new(
        &cx.engine,
        HostState {
            kernel: Arc::clone(&cx.kernel),
            thread,
            memory: Some(cx.memory.clone()),
            proc_state: Arc::clone(&cx.proc_state),
            exit: None,
            resume_at: None,
            sampler: cx.sampler.clone(),
            wayland: cx.wayland.clone(),
            virgl: cx.virgl.clone(),
            jit: None,
            spawn: Some(Arc::clone(&cx)),
            pending_spawn: None,
            pending_fork: None,
            pending_exec: None,
            store_limits: store_limits(),
            prof: None,
            nt: cx.nt.clone(),
            repl: None,
        },
    );
    // Without this the store keeps wasmtime's default of ten thousand
    // instances, and gex spends one per runtime translation.
    store.limiter(|s| &mut s.store_limits);
    let mut linker = cx.linker.clone();
    linker.allow_shadowing(true);
    linker.define(
        &mut store,
        IMPORT_MODULE,
        exports::MEMORY,
        cx.memory.shared().clone(),
    )?;
    profile::link(&mut linker, &mut store, &cx.prof_proc)?;
    let instance = linker
        .instantiate(&mut store, &cx.image_module)
        .context("instantiating the image module for a child process")?;
    profile::attach(&mut store, &instance)?;
    let tables = dispatch_tables(&mut store, &instance)?;
    let table = tables[0];
    if let Some(shared) = &cx.jit_shared {
        store.data_mut().jit = Some(Jit {
            shared: Arc::clone(shared),
            engine: cx.engine.clone(),
            linker: cx.linker.clone(),
            tables,
            cache: cx.jit_cache.clone(),
            instances: Vec::new(),
            replayed: 0,
            limits: cx.limits,
        });
    }
    catch_up_table(&mut store)?;
    let stopped = run_thread_loop(&mut store, &instance, thread.ctx_addr, entry)?;
    profile::harvest(&mut store)?;
    match stopped {
        Stopped::Exited(exit) => {
            stop_other_threads(&cx, false);
            Ok(exit)
        }
        Stopped::Exec(req) => {
            // The child is usually the one that execs: this is the shell's
            // fork-then-exec, and the whole point of the fork.
            stop_other_threads(&cx, false);
            let kernel = take_kernel(&cx);
            let opts = RunOptions {
                args: req.argv.clone(),
                env: req.envp.clone(),
                ..cx.opts.clone()
            };
            exec_program(
                req,
                &opts,
                &cx.xlate,
                kernel,
                Arc::clone(&cx.proc_state),
                cx.wayland.clone(),
                cx.virgl.clone(),
            )
            .map(|(exit, _)| exit)
        }
    }
}

/// Take the kernel out of a process that is about to be replaced.
fn take_kernel(cx: &SpawnContext) -> Kernel {
    let mut guard = cx.kernel.lock().unwrap_or_else(|e| e.into_inner());
    // Every other thread is stopped by now, so nothing else will look at it.
    std::mem::replace(&mut *guard, Kernel::new(0, 0, String::new()))
}

/// Load and run the program `execve` asked for, keeping the process.
#[allow(clippy::too_many_arguments)]
fn exec_program(
    req: ExecRequest,
    opts: &RunOptions,
    xlate: &Options,
    mut kernel: Kernel,
    proc_state: Arc<Mutex<ProcState>>,
    wayland: Option<wayland::Wayland>,
    virgl: Option<virgl::VirglDevice>,
) -> Result<(Exit, Stats)> {
    // The file comes from the guest's namespace, not the host's, so a sysroot
    // is honoured and `/bin/sh` means the guest's.
    let bytes = kernel
        .read_file(&req.path)
        .ok_or_else(|| anyhow!("execve: cannot read {}", req.path))?;
    let program = gex_ld::load_program(
        &bytes,
        gex_abi::PIE_BASE,
        gex_abi::PIE_BASE + 0x1000_0000,
        &|p| kernel.read_file(p),
    )
    .with_context(|| format!("execve: loading {}", req.path))?;
    // Descriptors marked close-on-exec go now; the rest survive, which is how
    // a shell hands a pipe to the command it starts.
    kernel.close_on_exec();
    // And the handlers, which are addresses in the image being replaced.
    kernel.signals.exec();
    // `/proc/self/exe` is the file just loaded, by its guest path -- not
    // argv[0], which for a busybox applet run through `$PATH` is a bare `wc`
    // (and glibc's `$ORIGIN` handling asserts on a relative link).
    kernel.exe_path = if req.path.starts_with('/') {
        req.path.clone()
    } else {
        format!("{}/{}", kernel.vfs.cwd.trim_end_matches('/'), req.path)
    };
    let opts = RunOptions {
        args: req.argv,
        env: req.envp,
        ..opts.clone()
    };
    run_program_with(
        &program,
        &opts,
        xlate,
        Some(Inherited {
            kernel,
            proc_state,
            wayland,
            virgl,
        }),
        None,
    )
}

/// A guest thread: its own store, its own instances, the same memory.
fn run_guest_thread(cx: Arc<SpawnContext>, thread: ThreadState) -> Result<()> {
    let mut store = Store::new(
        &cx.engine,
        HostState {
            kernel: Arc::clone(&cx.kernel),
            thread,
            memory: Some(cx.memory.clone()),
            proc_state: Arc::clone(&cx.proc_state),
            exit: None,
            resume_at: None,
            sampler: cx.sampler.clone(),
            wayland: cx.wayland.clone(),
            virgl: cx.virgl.clone(),
            jit: None,
            spawn: Some(Arc::clone(&cx)),
            pending_spawn: None,
            pending_fork: None,
            pending_exec: None,
            store_limits: store_limits(),
            prof: None,
            nt: cx.nt.clone(),
            repl: None,
        },
    );
    // Without this the store keeps wasmtime's default of ten thousand
    // instances, and gex spends one per runtime translation.
    store.limiter(|s| &mut s.store_limits);
    let mut linker = cx.linker.clone();
    linker.allow_shadowing(true);
    linker.define(
        &mut store,
        IMPORT_MODULE,
        exports::MEMORY,
        cx.memory.shared().clone(),
    )?;
    profile::new_thread();
    profile::link(&mut linker, &mut store, &cx.prof_proc)?;
    let instance = linker
        .instantiate(&mut store, &cx.image_module)
        .context("instantiating the image module for a new thread")?;
    profile::attach(&mut store, &instance)?;
    let tables = dispatch_tables(&mut store, &instance)?;
    let table = tables[0];
    if let Some(shared) = &cx.jit_shared {
        store.data_mut().jit = Some(Jit {
            shared: Arc::clone(shared),
            engine: cx.engine.clone(),
            linker: cx.linker.clone(),
            tables,
            cache: cx.jit_cache.clone(),
            instances: Vec::new(),
            replayed: 0,
            limits: cx.limits,
        });
    }

    // This thread's table starts with only what the image module installed,
    // so everything translated since has to be replayed into it before the
    // first branch reaches for it.
    catch_up_table(&mut store)?;

    let entry = {
        let data = cx.memory.bytes();
        read_u64(data, (thread.ctx_addr + ctx::PC as u64) as usize)
    };
    let stopped = run_thread_loop(&mut store, &instance, thread.ctx_addr, entry)?;
    profile::harvest(&mut store)?;
    if let Stopped::Exec(_) = stopped {
        bail!("execve from a non-main thread is not supported");
    }
    Ok(())
}

/// Instantiate every registry entry into this store, outside a wasm frame.
fn catch_up_table(store: &mut Store<HostState>) -> Result<()> {
    let Some(jit) = store.data().jit.as_ref() else {
        return Ok(());
    };
    let (shared, from) = (Arc::clone(&jit.shared), jit.replayed);
    let pending: Vec<Arc<CompiledUnit>> = {
        let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.registry[from.min(sh.registry.len())..].to_vec()
    };
    for unit in &pending {
        install_unit(&mut *store, unit)?;
        store.data_mut().jit.as_mut().unwrap().replayed += 1;
    }
    Ok(())
}

/// Build the guest's namespace.
///
/// The layout follows what a container presents: a root (the host, or a
/// sysroot when one is given), the synthetic `/proc` and `/dev`, and a
/// writable `/tmp` that never touches the host. Mount order does not matter
/// because the VFS resolves by longest prefix.
fn mount_filesystems(
    kernel: &mut Kernel,
    opts: &RunOptions,
    proc_state: Arc<Mutex<ProcState>>,
) -> Result<()> {
    match &opts.sysroot {
        Some(root) => {
            if !root.is_dir() {
                bail!("sysroot {} is not a directory", root.display());
            }
            kernel.vfs.mount("/", Arc::new(HostFs::new(root)));
        }
        None => kernel.vfs.mount("/", Arc::new(HostFs::new("/"))),
    }
    let mut devfs = DevFs::new();
    if opts.gl {
        // Named for what it is rather than as `/dev/dri/renderD128`: this is
        // not a DRM device and a program that probed it as one would be
        // misled. The guest's GL library looks for this name.
        devfs.add("gex-gl", crate::gl::GlDevice::new(1024, 1024));
    }
    kernel.vfs.mount("/dev", Arc::new(devfs));
    // A native host has real sockets, so the guest gets a real network.
    kernel.net = Some(Arc::new(HostNet));
    kernel.vfs.mount(
        "/proc",
        Arc::new(ProcFs::new(Arc::new(KernelProcSource(proc_state)))),
    );
    if opts.sysroot.is_some() {
        // A sysroot has no host /tmp behind it, so give the guest one.
        kernel.vfs.mount("/tmp", Arc::new(TmpFs::new()));
    }
    // Start where the host process is, so relative paths on the command line
    // mean what the user typed.
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".into());
    let start = match &opts.cwd {
        Some(d) => d.clone(),
        None if opts.sysroot.is_some() => "/".to_string(),
        None => cwd,
    };
    let _ = kernel.vfs.set_cwd(&start);
    Ok(())
}

/// Where the guest spends its time, sampled at back-edge safepoints. Biased
/// towards loops, which is the point; straight-line code is invisible to it.
#[derive(Debug, Default)]
pub struct Sampler {
    counts: Mutex<std::collections::HashMap<u64, u64>>,
}

impl Sampler {
    fn sample(&self, pc: u64) {
        *self
            .counts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(pc)
            .or_insert(0) += 1;
    }

    /// Print the hottest addresses, naming them through the vma table so a hot
    /// address in a library resolves to its object and function rather than
    /// `?`. `root` is the sysroot guest paths are relative to, or `None` for
    /// host paths.
    pub fn report_mapped(
        &self,
        vmas: &[gex_kernel::mem::Vma],
        root: Option<&str>,
        top: usize,
    ) {
        let counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        let total: u64 = counts.values().sum();
        if total == 0 {
            return;
        }
        let mut rows: Vec<(u64, u64)> = counts.iter().map(|(a, n)| (*a, *n)).collect();
        rows.sort_by_key(|(a, n)| (std::cmp::Reverse(*n), *a));
        rows.truncate(top);
        eprintln!("[gex] {total} safepoint samples, hottest back-edges:");

        let vma_of = |addr: u64| vmas.iter().find(|v| addr >= v.start && addr < v.end);

        // Every object the hot rows land in, read once each. Loaded before
        // anything is formatted so the borrow is simple: a table built, then
        // a table used.
        let mut want: Vec<String> = rows
            .iter()
            .filter_map(|(a, _)| vma_of(*a).and_then(|v| v.origin.as_ref()).map(|(p, _)| p.clone()))
            .collect();
        want.sort();
        want.dedup();
        let mut syms: std::collections::HashMap<String, Vec<gex_ld::elf::Symbol>> =
            std::collections::HashMap::new();
        for path in want {
            let full = match root {
                Some(r) => {
                    format!("{}/{}", r.trim_end_matches('/'), path.trim_start_matches('/'))
                }
                None => path.clone(),
            };
            let mut v: Vec<gex_ld::elf::Symbol> = std::fs::read(&full)
                .ok()
                .and_then(|b| gex_ld::elf::load(&b, 0).ok())
                .map(|img| img.symbols.into_iter().filter(|s| s.size > 0).collect())
                .unwrap_or_default();
            v.sort_by_key(|s| s.value);
            syms.insert(path, v);
        }

        for (addr, n) in rows {
            let pct = 100.0 * n as f64 / total as f64;
            let where_ = match vma_of(addr) {
                // Mapped from a file: the load base is the mapping's start
                // less its offset into that file, and the symbol table is in
                // the object's own address space.
                Some(v) => match &v.origin {
                    Some((path, off)) => {
                        let vaddr = addr.wrapping_sub(v.start.wrapping_sub(*off));
                        let short = path.rsplit('/').next().unwrap_or(path);
                        let named = syms.get(path).and_then(|t| {
                            let i = match t.binary_search_by(|s| s.value.cmp(&vaddr)) {
                                Ok(i) => i,
                                Err(0) => return None,
                                Err(i) => i - 1,
                            };
                            let s = &t[i];
                            (vaddr < s.value + s.size)
                                .then(|| format!("{} {}+{:#x}", short, s.name, vaddr - s.value))
                        });
                        named.unwrap_or_else(|| format!("{short}+{vaddr:#x}"))
                    }
                    None => format!("[{}]+{:#x}", v.name, addr - v.start),
                },
                None => "unmapped".to_string(),
            };
            eprintln!("[gex]   {pct:5.1}%  {addr:#012x}  {where_}");
        }
    }

    /// Print the hottest addresses, naming them where the image can.
    pub fn report(&self, symbols: &[gex_ld::elf::Symbol], top: usize) {
        let counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        let total: u64 = counts.values().sum();
        if total == 0 {
            return;
        }
        let mut rows: Vec<(u64, u64)> = counts.iter().map(|(a, n)| (*a, *n)).collect();
        rows.sort_by_key(|(a, n)| (std::cmp::Reverse(*n), *a));
        eprintln!("[gex] {total} safepoint samples, hottest back-edges:");
        // Sorted once so each address is a binary search rather than a scan.
        let mut syms: Vec<&gex_ld::elf::Symbol> =
            symbols.iter().filter(|s| s.size > 0).collect();
        syms.sort_by_key(|s| s.value);
        for (addr, n) in rows.into_iter().take(top) {
            let name = match syms.binary_search_by(|s| s.value.cmp(&addr)) {
                Ok(i) => Some(syms[i]),
                Err(0) => None,
                Err(i) => Some(syms[i - 1]),
            }
            .filter(|s| addr < s.value + s.size)
            .map(|s| format!("{}+{:#x}", s.name, addr - s.value))
            .unwrap_or_else(|| "?".to_string());
            eprintln!(
                "[gex]   {:5.1}%  {addr:#012x}  {name}",
                100.0 * n as f64 / total as f64
            );
        }
    }
}

/// The one sampler for this run.
///
/// Process-wide rather than passed down, because `exec` replaces everything
/// else and a profile that stopped at the first `exec` would miss the part
/// of a package manager's work that matters.
static SAMPLER: std::sync::OnceLock<Option<Arc<Sampler>>> = std::sync::OnceLock::new();

/// Whether to statically recompile each object as it is mapped. Process-wide
/// for the same reason the sampler is: the import surface is registered once
/// and outlives any one program.
static RECOMPILE_OBJECTS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether a dispatch miss may translate. Distinct from whether the
/// installation machinery exists.
static JIT_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Whether to print the guest's registers and stack when it aborts. `abort`
/// unwinds nothing, so the state must be captured at the syscall or it is gone
/// (e.g. which function's canary failed, from the stack's return addresses).
static DUMP_ABORT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A watch on a range of guest memory: `GEX_WATCH=<hex addr>[+<hex len>]`. A
/// software watchpoint: every safepoint and syscall compares the bytes and
/// reports the first pc they differ at. Coarse (the pc is the poll, not the
/// write) but narrows corruption to one loop.
static WATCH: std::sync::Mutex<Option<(u64, usize, Vec<u8>)>> = std::sync::Mutex::new(None);

fn watch_check(gm: &GuestMem<'_>, pc: u64, tid: i32) {
    if !WATCH_ON.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let mut w = WATCH.lock().unwrap_or_else(|e| e.into_inner());
    if w.is_none() {
        watch_arm(gm, &mut w);
    }
    let Some((addr, len, before)) = w.as_mut() else {
        return;
    };
    let (start, end) = (*addr as usize, *addr as usize + *len);
    if end > gm.bytes.len() {
        return;
    }
    let now = &gm.bytes[start..end];
    if now == before.as_slice() {
        return;
    }
    eprintln!(
        "gex: watch {addr:#x} changed at pc {pc:#x} (thread {tid})\n  was {}\n  now {}",
        hex_bytes(before),
        hex_bytes(now)
    );
    before.clear();
    before.extend_from_slice(now);
}

fn hex_bytes(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

static WATCH_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Take the initial snapshot, at the guest's first syscall.
///
/// Later than it looks: by the time a dynamic program makes a syscall the
/// loader is running, so an address in a library that is not mapped yet reads
/// as zero and the mapping itself is reported as the first change.
fn watch_arm(gm: &GuestMem<'_>, slot: &mut Option<(u64, usize, Vec<u8>)>) {
    let Some(spec) = std::env::var_os("GEX_WATCH") else {
        WATCH_ON.store(false, std::sync::atomic::Ordering::Relaxed);
        return;
    };
    let spec = spec.to_string_lossy().to_string();
    let (a, l) = spec.split_once('+').unwrap_or((spec.as_str(), "8"));
    let (Ok(addr), Ok(len)) = (
        u64::from_str_radix(a.trim_start_matches("0x"), 16),
        usize::from_str_radix(l.trim_start_matches("0x"), 16),
    ) else {
        eprintln!("gex: GEX_WATCH wants <hex addr>[+<hex len>], got {spec:?}");
        WATCH_ON.store(false, std::sync::atomic::Ordering::Relaxed);
        return;
    };
    if addr as usize + len > gm.bytes.len() {
        return;
    }
    let bytes = gm.bytes[addr as usize..addr as usize + len].to_vec();
    eprintln!("gex: watching {addr:#x}+{len:#x}, now {}", hex_bytes(&bytes));
    *slot = Some((addr, len, bytes));
}

/// Print the register file and the return addresses on the stack. No unwind
/// info: scanning for words that point just after a `bl` in executable memory
/// finds the call chain plus some easily-ignored noise.
fn dump_guest(gm: &mut GuestMem<'_>, ctx_addr: u64, tid: i32, vmas: &[gex_kernel::mem::Vma]) {
    let at = |a: u64| -> Option<&gex_kernel::mem::Vma> {
        vmas.iter().find(|v| a >= v.start && a < v.end)
    };
    let describe = |a: u64| -> String {
        match at(a) {
            Some(v) if !v.name.is_empty() => format!("{}+0x{:x}", v.name, a - v.start),
            _ => String::new(),
        }
    };
    let read =
        |gm: &mut GuestMem<'_>, off: usize| gm.read_u64(ctx_addr + off as u64).unwrap_or(0);
    let pc = read(gm, ctx::PC);
    let sp = read(gm, ctx::SP);
    eprintln!("gex: guest abort in thread {tid}");
    eprintln!("  pc  {pc:#018x}  {}", describe(pc));
    eprintln!("  sp  {sp:#018x}  {}", describe(sp));
    for row in 0..8 {
        let mut line = String::new();
        for col in 0..4 {
            let i = row * 4 + col;
            if i > 30 {
                break;
            }
            line.push_str(&format!("  x{i:<2} {:#018x}", read(gm, ctx::x(i))));
        }
        eprintln!(" {line}");
    }
    // glibc keeps a frame pointer, so the call chain is a linked list rather
    // than a guess: x29 points at the saved pair, and the return address is
    // the word above it.
    eprintln!("  frames:");
    let mut fp = read(gm, ctx::x(29));
    for _ in 0..32 {
        if fp == 0 || fp % 8 != 0 {
            break;
        }
        let (Ok(next), Ok(ra)) = (gm.read_u64(fp), gm.read_u64(fp + 8)) else {
            break;
        };
        if ra == 0 {
            break;
        }
        eprintln!("    fp {fp:#014x}  ra {ra:#018x}  {}", describe(ra));
        if next <= fp {
            break;
        }
        fp = next;
    }
    // The stack, upwards from where it is now. 32 KB is more than a canary
    // failure needs and cheap enough not to think about.
    eprintln!("  return addresses on the stack:");
    let mut found = 0;
    let mut a = sp & !7;
    while a < sp + 0x8000 && found < 40 {
        let Ok(w) = gm.read_u64(a) else { break };
        // A return address points *after* a `bl`, so the word before it must
        // be executable too; that alone discards most stray data.
        if w >= 4 && at(w).is_some_and(|v| v.prot & prot::EXEC != 0) {
            eprintln!("    [sp+{:#06x}] {w:#018x}  {}", a - sp, describe(w));
            found += 1;
        }
        a += 8;
    }
}

fn sampler_for(opts: &RunOptions) -> Option<Arc<Sampler>> {
    // Off unless asked for: it is a lock and a hash insert on every back-edge
    // safepoint, which is not free.
    SAMPLER
        .get_or_init(|| opts.sample.then(|| Arc::new(Sampler::default())))
        .clone()
}

/// The guest's network, as the host's real sockets. The kernel does its own
/// waiting, so the socket is non-blocking and the kernel polls it.
#[derive(Debug)]
struct HostNet;

impl gex_kernel::net::NetStack for HostNet {
    fn tcp(&self) -> Result<Arc<dyn gex_kernel::net::Transport>, gex_kernel::Errno> {
        Ok(Arc::new(HostStream::default()))
    }
}

#[derive(Debug, Default)]
struct HostStream {
    sock: Mutex<Option<std::net::TcpStream>>,
    /// Set once the connection has been shown to be up or to have failed, so
    /// the status does not depend on probing a socket that is already open.
    settled: Mutex<Option<gex_kernel::net::Status>>,
}

impl HostStream {
    /// Whether the connection has finished coming up.
    ///
    /// A non-blocking `connect` reports its outcome through the socket's
    /// error, which `take_error` returns exactly once, so the answer is
    /// remembered.
    fn probe(&self) -> gex_kernel::net::Status {
        use gex_kernel::net::Status;
        if let Some(s) = *self.settled.lock().unwrap_or_else(|e| e.into_inner()) {
            return s;
        }
        let guard = self.sock.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sock) = guard.as_ref() else {
            return Status::Closed;
        };
        let s = match sock.take_error() {
            Ok(Some(_)) => Status::Refused,
            Err(_) => Status::Refused,
            // Writability is how a finished connect announces itself; a peer
            // name is the cheapest way to ask.
            Ok(None) => match sock.peer_addr() {
                Ok(_) => Status::Open,
                Err(e) if e.kind() == std::io::ErrorKind::NotConnected => Status::Connecting,
                Err(_) => Status::Refused,
            },
        };
        if s != Status::Connecting {
            *self.settled.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
        }
        s
    }
}

impl gex_kernel::net::Transport for HostStream {
    fn connect(&self, host: &str, port: u16) -> Result<(), gex_kernel::Errno> {
        use gex_kernel::Errno;
        use std::net::ToSocketAddrs;
        // The host's own resolver, which is the one thing a native host has
        // that a page does not.
        let mut addrs = (host, port)
            .to_socket_addrs()
            .map_err(|_| Errno::ENETUNREACH)?;
        let addr = addrs.next().ok_or(Errno::ENETUNREACH)?;
        let sock = std::net::TcpStream::connect(addr).map_err(|_| Errno::ECONNREFUSED)?;
        sock.set_nonblocking(true)
            .map_err(|_| Errno::ENETUNREACH)?;
        *self.sock.lock().unwrap_or_else(|e| e.into_inner()) = Some(sock);
        Ok(())
    }

    fn status(&self) -> gex_kernel::net::Status {
        self.probe()
    }

    fn send(&self, data: &[u8]) -> Result<usize, gex_kernel::Errno> {
        use std::io::Write;
        let mut guard = self.sock.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sock) = guard.as_mut() else {
            return Err(gex_kernel::Errno::ENOTCONN);
        };
        match sock.write(data) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(gex_kernel::Errno::from_io(&e)),
        }
    }

    fn recv(&self, buf: &mut [u8]) -> Result<usize, gex_kernel::Errno> {
        use std::io::Read;
        let mut guard = self.sock.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sock) = guard.as_mut() else {
            return Ok(0);
        };
        match sock.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                Err(gex_kernel::Errno::EAGAIN)
            }
            Err(e) => Err(gex_kernel::Errno::from_io(&e)),
        }
    }

    fn has_data(&self) -> bool {
        use std::io::Read;
        if self.probe() != gex_kernel::net::Status::Open {
            return true;
        }
        let mut guard = self.sock.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sock) = guard.as_mut() else {
            return true;
        };
        // Peek one byte to tell whether data is there; a zero-length read
        // cannot, and there is no `MSG_PEEK` in std.
        let mut probe = [0u8; 1];
        match sock.peek(&mut probe) {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(_) => true,
        }
    }

    /// How much has arrived, by peeking. A real syscall, but `FIONREAD` is
    /// asked rarely and the alternative -- guessing -- is what made glibc's
    /// resolver read a datagram into a zero-length buffer.
    fn available(&self) -> usize {
        let mut guard = self.sock.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sock) = guard.as_mut() else {
            return 0;
        };
        let mut scratch = vec![0u8; 64 * 1024];
        sock.peek(&mut scratch).unwrap_or(0)
    }

    fn close(&self) {
        let _ = self.sock.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

/// The host process's real user and group ids.
///
/// Read from `/proc/self/status` rather than through `libc`, which keeps this
/// crate free of a C dependency; falling back to 1000 costs only fidelity.
fn host_identity() -> (u32, u32) {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return (1000, 1000);
    };
    let field = |name: &str| -> Option<u32> {
        status
            .lines()
            .find(|l| l.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    (field("Uid:").unwrap_or(1000), field("Gid:").unwrap_or(1000))
}

/// Copy the ELF segments and the initial stack into guest memory.
fn place_image(
    store: &mut Store<HostState>,
    memory: gex_mem::GuestMemory,
    program: &Program,
    opts: &RunOptions,
    id: Identity,
) -> Result<()> {
    let image = &program.exe;
    // Segments first: the executable's, then the interpreter's.
    for seg in program.segments() {
        let data = memory.bytes();
        let start = seg.vaddr as usize;
        let end = start + seg.mem_size as usize;
        if end > data.len() {
            bail!(
                "segment 0x{:x}..0x{:x} does not fit in guest memory",
                seg.vaddr,
                seg.vaddr + seg.mem_size
            );
        }
        data[start..end].fill(0);
        data[start..start + seg.bytes.len()].copy_from_slice(&seg.bytes);

        let mut p = 0;
        if seg.readable {
            p |= prot::READ;
        }
        if seg.writable {
            p |= prot::WRITE;
        }
        if seg.executable {
            p |= prot::EXEC;
        }
        store.data_mut().with_kernel(|k| {
            k.mem
                .insert(seg.vaddr, seg.vaddr + seg.mem_size, p, "[image]")
        });
    }

    // Then the stack. Determinism by default (PLAN §11) means a fixed
    // AT_RANDOM unless the caller asks otherwise.
    let layout = build_initial_stack(
        image,
        gex_abi::STACK_TOP,
        &opts.args,
        &opts.env,
        *b"gexdeterministic",
        id,
    );
    let data = memory.bytes();
    let start = layout.low as usize;
    let end = start + layout.bytes.len();
    if end > data.len() {
        bail!("initial stack does not fit in guest memory");
    }
    data[start..end].copy_from_slice(&layout.bytes);

    let stack_low = gex_abi::STACK_TOP - gex_abi::STACK_SIZE;
    store.data_mut().with_kernel(|k| {
        k.mem.insert(
            stack_low,
            gex_abi::STACK_TOP,
            prot::READ | prot::WRITE,
            "[stack]",
        )
    });

    // Finally the initial register state: sp, and x0 cleared the way Linux
    // leaves it.
    let data = memory.bytes();
    let ctx_addr = gex_abi::ctx_addr(0) as usize;
    // Registers first, then the stack pointer: on x86-64 `SP` is one of them.
    for i in 0..31u8 {
        write_u64(data, ctx_addr + ctx::x(i), 0);
    }
    write_u64(data, ctx_addr + ctx::SP, layout.sp);
    write_u64(data, ctx_addr + ctx::PC, image.entry);
    data[ctx_addr + ctx::SAFEPOINT_COUNTER..ctx_addr + ctx::SAFEPOINT_COUNTER + 4]
        .copy_from_slice(&SAFEPOINT_INTERVAL.to_le_bytes());
    // x87 control word: the power-on default glibc expects (round to
    // nearest, 64-bit precision, all exceptions masked).
    // (`MXCSR` is A64's `FPCR` slot, which starts at zero.)
    if gex_abi::X86 {
        data[ctx_addr + ctx::X87_CW..ctx_addr + ctx::X87_CW + 4].copy_from_slice(&0x37fu32.to_le_bytes());
        data[ctx_addr + ctx::MXCSR..ctx_addr + ctx::MXCSR + 4].copy_from_slice(&0x1f80u32.to_le_bytes());
    }
    Ok(())
}

fn write_u64(data: &mut [u8], off: usize, v: u64) {
    data[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn read_u64(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(data[off..off + 8].try_into().unwrap())
}

/// Wire the `gex.*` import surface to the kernel.
fn register_imports(linker: &mut Linker<HostState>) -> Result<()> {
    let m = IMPORT_MODULE;

    linker.func_wrap(
        m,
        imports::SYSCALL,
        |mut caller: Caller<'_, HostState>, _ctx: i32, pc: i64| -> Result<()> {
            if caller.data().nt.is_some() {
                return nt::syscall_import(&mut caller, pc as u64);
            }
            // Every thread shares the dispatch directory (guest memory) but has
            // its own table, filled only when its own store installs a
            // translation. So a thread can be told to call slot N while its
            // table is still shorter than N — a hard trap, not a recoverable
            // miss. Catching up here closes that window; dpkg's decompressor
            // hit it with two threads a moment apart.
            catch_up_in_caller(&mut caller)?;
            let memory = caller
                .data()
                .memory
                .clone()
                .ok_or_else(|| anyhow!("guest memory unavailable"))?;
            // Profiling: count the call against its function (S6).
            profile::syscall_enter(&mut caller, &GuestMem::new(memory.bytes()), pc as u64);
            let data = memory.bytes();
            let state = caller.data_mut();
            // A thread that loops on a blocking syscall -- a condition
            // variable is exactly this -- never reaches a back-edge
            // safepoint, so the process-is-ending check has to be here too.
            if state
                .spawn
                .as_ref()
                .is_some_and(|s| s.stopping.load(std::sync::atomic::Ordering::Acquire))
            {
                state.exit = Some(Exit::Status(0));
                return Err(StopExecution.into());
            }
            let mut gm = GuestMem::new(data);
            // /proc must reflect the state this syscall will see, so the
            // snapshot is refreshed before the call rather than after.
            let proc_state = Arc::clone(&state.proc_state);
            // The syscall number decides whether /proc has to be rebuilt
            // first; most of the hot ones cannot observe it.
            let nr = gm
                .read_u64(state.thread.ctx_addr + ctx::x(8) as u64)
                .unwrap_or(!0);
            watch_check(&gm, pc as u64, state.thread.tid);
            lockstep::log_entry(&gm, state.thread.ctx_addr, state.thread.tid, pc as u64);
            if lockstep::fake_syscall(&mut gm, state.thread.ctx_addr) {
                lockstep::log_result(&gm, state.thread.ctx_addr, state.thread.tid);
                return Ok(());
            }
            // A guest that is about to kill itself is the last moment its
            // state means anything. `abort` unwinds nothing, so by the time
            // the process is gone there is no way to ask where it was.
            if DUMP_ABORT.load(std::sync::atomic::Ordering::Relaxed) {
                let signo = match nr {
                    gex_kernel::syscalls::nr::KILL | gex_kernel::syscalls::nr::TKILL => {
                        gm.read_u64(state.thread.ctx_addr + ctx::x(1) as u64)
                    }
                    gex_kernel::syscalls::nr::TGKILL => {
                        gm.read_u64(state.thread.ctx_addr + ctx::x(2) as u64)
                    }
                    _ => Err(gex_kernel::Errno::EINVAL),
                };
                if signo == Ok(6) {
                    let ctx_addr = state.thread.ctx_addr;
                    let tid = state.thread.tid;
                    let vmas = state.with_kernel(|k| k.mem.vmas().to_vec());
                    dump_guest(&mut gm, ctx_addr, tid, &vmas);
                }
            }
            let outcome = state.with_kernel(|k| {
                if !gex_kernel::skips_proc_snapshot(nr) {
                    k.refresh_proc(&proc_state);
                }
                k.syscall(&mut gm, pc as u64)
            });
            // A blocking syscall is run here, with the kernel unlocked, and
            // then finished under the lock again.
            let outcome = if let SyscallOutcome::Blocking(op) = outcome {
                let compositor = caller.data().wayland.clone();
                let renderer = caller.data().virgl.clone();
                let kernel_for_service = Arc::clone(&caller.data().kernel);
                let service = move |bytes: &mut [u8]| {
                    if let Some(w) = &compositor {
                        let k = kernel_for_service
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        let gm = GuestMem::new(bytes);
                        w.pump(&k, &gm);
                    }
                    // The renderer needs neither the kernel nor guest memory:
                    // at this protocol version every payload is in the stream.
                    if let Some(v) = &renderer {
                        v.pump();
                    }
                };
                // Timers are checked here rather than only at syscall entry:
                // a process asleep in a wait loop issues no syscalls, so
                // nothing else would ever arm `SIGALRM`.
                let kernel_for_signals = Arc::clone(&caller.data().kernel);
                let interrupted = move || {
                    kernel_for_signals
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .signal_pending()
                };
                let r = perform_block(op, memory.bytes(), &service, &interrupted);
                let mut gm = GuestMem::new(memory.bytes());
                caller
                    .data_mut()
                    .with_kernel(|k| k.finish_blocking(&mut gm, r))
            } else {
                outcome
            };
            profile::syscall_exit(caller.data_mut(), memory.bytes());
            // The compositor runs here, between one syscall and the next. A
            // client writes a request and then waits for the reply, and the
            // write is a syscall, so by the time it waits the reply is
            // already in its socket.
            if let Some(w) = caller.data().wayland.clone() {
                let kernel = Arc::clone(&caller.data().kernel);
                let k = kernel.lock().unwrap_or_else(|e| e.into_inner());
                let gm = GuestMem::new(memory.bytes());
                w.pump(&k, &gm);
            }
            if let Some(v) = caller.data().virgl.clone() {
                v.pump();
            }
            // Code that has gone away, retired before the newly mapped code
            // below: a `dlclose`+`dlopen` reuses addresses, and retiring after
            // would throw away the fresh translation.
            let retired: Vec<(u64, u64)> = caller
                .data_mut()
                .with_kernel(|k| std::mem::take(&mut k.retired_code));
            if !retired.is_empty() {
                retire_translations(&mut caller, &retired)?;
            }
            let mapped: Vec<gex_kernel::CodeMap> = caller
                .data_mut()
                .with_kernel(|k| std::mem::take(&mut k.new_code));
            profile::note_mapped(caller.data_mut(), &mapped);
            profile::poll(&mut caller)?;
            let want_objects = RECOMPILE_OBJECTS.load(std::sync::atomic::Ordering::Relaxed);
            for m in mapped.into_iter().filter(|_| want_objects) {
                // Which seeds depend on whether the runtime translator is
                // behind this. With it on, leaders (symbols + address-taken
                // data) are enough and far cheaper than a block at every
                // decodable address, which is complete but quadratic. With it
                // off, the static pass is the only pass and must be complete.
                let seeds = if JIT_ON.load(std::sync::atomic::Ordering::Relaxed) {
                    object_seeds(&mut caller, &m)
                } else {
                    Vec::new()
                };
                if let Err(e) = recompile_object(&mut caller, &m, &seeds) {
                    // Not fatal: the runtime translator is still behind this,
                    // and it will pick the object up a block at a time.
                    eprintln!("gex: could not recompile {}: {e:#}", m.path);
                }
            }
            {
                let (ctx_addr, tid) = (caller.data().thread.ctx_addr, caller.data().thread.tid);
                lockstep::log_result(&GuestMem::new(memory.bytes()), ctx_addr, tid);
            }
            let state = caller.data_mut();
            match outcome {
                SyscallOutcome::Continue => Ok(()),
                SyscallOutcome::Resume(target) => {
                    state.resume_at = Some(target);
                    Err(StopExecution.into())
                }
                SyscallOutcome::Exit(code) => {
                    state.exit = Some(Exit::Status(code));
                    Err(StopExecution.into())
                }
                SyscallOutcome::Exec { path, argv, envp } => {
                    state.pending_exec = Some(ExecRequest { path, argv, envp });
                    Err(StopExecution.into())
                }
                SyscallOutcome::SpawnProcess {
                    pid,
                    child_stack,
                    tls,
                } => {
                    // Same reason as a thread: the store is borrowed here, and
                    // a fork has to copy the whole address space.
                    state.pending_fork = Some(ForkRequest {
                        pid,
                        child_stack,
                        tls,
                    });
                    state.resume_at = Some(
                        gm.read_u64(state.thread.ctx_addr + ctx::PC as u64)
                            .unwrap_or(0),
                    );
                    Err(StopExecution.into())
                }
                SyscallOutcome::SpawnThread {
                    ctx_addr,
                    tid,
                    clear_child_tid,
                } => {
                    // Starting the thread here would mean spawning from
                    // inside a wasm frame; record it and let the run loop do
                    // it between calls, where nothing is borrowed.
                    state.pending_spawn = Some(ThreadState {
                        ctx_addr,
                        tid,
                        clear_child_tid,
                    });
                    state.resume_at = Some(
                        gm.read_u64(state.thread.ctx_addr + ctx::PC as u64)
                            .unwrap_or(0),
                    );
                    Err(StopExecution.into())
                }
                SyscallOutcome::Blocking(_) => {
                    unreachable!("a blocking syscall was already completed")
                }
                SyscallOutcome::ThreadExit => {
                    state.exit = Some(Exit::Status(0));
                    Err(StopExecution.into())
                }
                SyscallOutcome::Killed { signal, core } => {
                    state.exit = Some(Exit::Signal {
                        signal: signal as i32,
                        pc: pc as u64,
                        detail: format!(
                            "killed by signal {signal}{}",
                            if core { " (core dumped)" } else { "" }
                        ),
                        internal: false,
                    });
                    Err(StopExecution.into())
                }
            }
        },
    )?;

    linker.func_wrap(
        m,
        imports::DISPATCH_MISS,
        |mut caller: Caller<'_, HostState>, _ctx: i32, pc: i64| -> Result<i32> {
            let pc = pc as u64;
            // Returning from a signal handler with no `sa_restorer` lands
            // here: the trampoline address deliberately has no translation.
            if pc == gex_kernel::signal::SIGRETURN_TRAMPOLINE {
                let memory = caller
                    .data()
                    .memory
                    .clone()
                    .ok_or_else(|| anyhow!("guest memory unavailable"))?;
                let data = memory.bytes();
                let state = caller.data_mut();
                let mut gm = GuestMem::new(data);
                return match state.with_kernel(|k| k.sigreturn(&mut gm)) {
                    Ok(target) => {
                        state.resume_at = Some(target);
                        Err(StopExecution.into())
                    }
                    Err(_) => {
                        state.exit = Some(Exit::Signal {
                            signal: signal::SIGSEGV,
                            pc,
                            detail: "corrupt signal frame on sigreturn".into(),
                            internal: true,
                        });
                        Err(StopExecution.into())
                    }
                };
            }
            // Translate on demand and patch the slot, then resume there.
            match translate_and_install(&mut caller, pc) {
                Ok(true) => {
                    caller.data_mut().resume_at = Some(pc);
                    Err(StopExecution.into())
                }
                Ok(false) => {
                    if caller.data().nt.is_some() {
                        return nt::fault(&mut caller, nt::Fault::NotExecutable, pc).map(|_| 0);
                    }
                    // Code an ejected project was built without: say so,
                    // rather than let it read as the guest's own crash.
                    if let Some(detail) = aot::unseen_code(&mut caller, pc) {
                        caller.data_mut().exit = Some(Exit::Signal {
                            signal: signal::SIGSEGV,
                            pc,
                            detail,
                            internal: true,
                        });
                        return Err(StopExecution.into());
                    }
                    // A wild branch is nearly always a bad return address or
                    // function pointer: say where the last indirect branch
                    // was aimed and what the stack holds, so the caller can
                    // be found without a full trace.
                    let detail = {
                        let memory = caller.data().memory.clone();
                        let state = caller.data();
                        let base = state.thread.ctx_addr;
                        match memory {
                            Some(memory) => {
                                let gm = GuestMem::new(memory.bytes());
                                let last = gm.read_u64(base + ctx::LAST_TARGET as u64).unwrap_or(0);
                                let src = gm.read_u64(base + ctx::LAST_SOURCE as u64).unwrap_or(0);
                                let code: Vec<String> = gm
                                    .read(src, 24)
                                    .map(|b| b.iter().map(|x| format!("{x:02x}")).collect())
                                    .unwrap_or_default();
                                let sp = gm.read_u64(base + ctx::SP as u64).unwrap_or(0);
                                let stack: Vec<String> = (0..8)
                                    .map(|i| format!("{:#x}", gm.read_u64(sp + i * 8).unwrap_or(0)))
                                    .collect();
                                format!(
                                    "branch to 0x{pc:x}, which is not executable guest memory (last indirect target {last:#x} from block {src:#x} [{}]; sp={sp:#x} stack=[{}])",
                                    code.join(" "),
                                    stack.join(", ")
                                )
                            }
                            None => format!("branch to 0x{pc:x}, which is not executable guest memory"),
                        }
                    };
                    caller.data_mut().exit = Some(Exit::Signal {
                        signal: signal::SIGSEGV,
                        pc,
                        detail,
                        internal: false,
                    });
                    Err(StopExecution.into())
                }
                Err(e) => {
                    caller.data_mut().exit = Some(Exit::Signal {
                        signal: signal::SIGSEGV,
                        pc,
                        detail: format!("translating 0x{pc:x} failed: {e:#}"),
                        internal: true,
                    });
                    Err(StopExecution.into())
                }
            }
        },
    )?;

    linker.func_wrap(
        m,
        imports::ILLEGAL,
        |mut caller: Caller<'_, HostState>, _ctx: i32, pc: i64, raw: i32| -> Result<()> {
            if caller.data().nt.is_some() {
                return nt::fault(&mut caller, nt::Fault::Illegal, pc as u64);
            }
            let pc = pc as u64;
            let raw = raw as u32;
            let memory = caller
                .data()
                .memory
                .clone()
                .ok_or_else(|| anyhow!("guest memory unavailable"))?;
            let data = memory.bytes();
            let state = caller.data_mut();
            let mut gm = GuestMem::new(data);
            // A guest that installed a SIGILL handler gets to see it, which
            // is how some runtimes probe for optional instructions.
            match state.with_kernel(|k| k.raise_fault(&mut gm, signal::SIGILL as u32, pc, pc)) {
                gex_kernel::signal::Delivery::Resume(target) => {
                    state.resume_at = Some(target);
                }
                _ => {
                    // Where the last indirect branch was aimed, and how the
                    // dispatch directory names its page and this one: an
                    // illegal instruction at an address no branch should
                    // reach is usually a dispatch that landed in the wrong
                    // slot, and these are the numbers that say so.
                    let last = gm
                        .read_u64(state.thread.ctx_addr + ctx::LAST_TARGET as u64)
                        .unwrap_or(0);
                    let dir = |a: u64| {
                        gm.read_u32(gex_abi::DISPATCH_DIR + gex_abi::dispatch_page(a) * 4)
                            .unwrap_or(0)
                    };
                    state.exit = Some(Exit::Signal {
                        signal: signal::SIGILL,
                        pc,
                        detail: format!(
                            "instruction 0x{raw:08x} at 0x{pc:x} is not implemented (last indirect target 0x{last:x}; directory entry for its page {:#x}, for this page {:#x})",
                            dir(last),
                            dir(pc)
                        ),
                        internal: true,
                    });
                }
            }
            Err(StopExecution.into())
        },
    )?;

    linker.func_wrap(
        m,
        imports::BREAKPOINT,
        |mut caller: Caller<'_, HostState>, ctx: i32, pc: i64, imm: i32| -> Result<()> {
            // The diagnostic trace points dump the registers and carry on.
            if imm == gex_abi::imports::BRK_TRAP_PC || imm == gex_abi::imports::BRK_TRACE {
                if let Some(memory) = caller.data().memory.clone() {
                    let data = memory.bytes();
                    let c = ctx as u32 as usize;
                    let r = |i: u8| read_u64(data, c + ctx::x(i));
                    if gex_abi::X86 {
                        eprintln!(
                            "[trace] pc={pc:#x} rax={:#x} rcx={:#x} rdx={:#x} rbx={:#x} rsp={:#x} rbp={:#x} rsi={:#x} rdi={:#x} r8={:#x} r9={:#x} r10={:#x} r11={:#x} r12={:#x} r13={:#x} r14={:#x} r15={:#x} fl={:#x}",
                            r(0), r(1), r(2), r(3), r(4), r(5), r(6), r(7), r(8), r(9), r(10), r(11), r(12), r(13), r(14), r(15),
                            read_u64(data, c + ctx::NZCV)
                        );
                    } else {
                        let regs: Vec<String> = (0..31u8).map(|i| format!("x{i}={:#x}", r(i))).collect();
                        eprintln!(
                            "[trace] pc={pc:#x} sp={:#x} {}",
                            read_u64(data, c + ctx::SP),
                            regs.join(" ")
                        );
                    }
                }
                return Ok(());
            }
            if caller.data().nt.is_some() {
                return nt::fault(&mut caller, nt::Fault::Breakpoint, pc as u64);
            }
            // A divide error (`Stmt::Trap`): x86 #DE. Linux reports a zero
            // divisor and a quotient overflow alike, as SIGFPE with
            // FPE_INTDIV at the dividing instruction.
            if imm == gex_abi::imports::BRK_DIVIDE_BY_ZERO || imm == gex_abi::imports::BRK_DIVIDE_OVERFLOW {
                let pc = pc as u64;
                let memory = caller
                    .data()
                    .memory
                    .clone()
                    .ok_or_else(|| anyhow!("guest memory unavailable"))?;
                let data = memory.bytes();
                let state = caller.data_mut();
                let mut gm = GuestMem::new(data);
                match state.with_kernel(|k| k.raise_fault(&mut gm, signal::SIGFPE as u32, pc, pc)) {
                    gex_kernel::signal::Delivery::Resume(target) => state.resume_at = Some(target),
                    _ => {
                        state.exit = Some(Exit::Signal {
                            signal: signal::SIGFPE,
                            pc,
                            detail: format!(
                                "integer divide {} at 0x{pc:x}",
                                if imm == gex_abi::imports::BRK_DIVIDE_BY_ZERO { "by zero" } else { "overflow" }
                            ),
                            internal: false,
                        })
                    }
                }
                return Err(StopExecution.into());
            }
            // A general-protection fault (a privileged instruction in user
            // mode): SIGSEGV at the instruction.
            if imm == gex_abi::imports::BRK_GENERAL_PROTECTION {
                let pc = pc as u64;
                let memory = caller
                    .data()
                    .memory
                    .clone()
                    .ok_or_else(|| anyhow!("guest memory unavailable"))?;
                let data = memory.bytes();
                let state = caller.data_mut();
                let mut gm = GuestMem::new(data);
                match state.with_kernel(|k| k.raise_fault(&mut gm, signal::SIGSEGV as u32, pc, 0)) {
                    gex_kernel::signal::Delivery::Resume(target) => state.resume_at = Some(target),
                    _ => {
                        state.exit = Some(Exit::Signal {
                            signal: signal::SIGSEGV,
                            pc,
                            detail: format!("general protection fault (privileged instruction) at 0x{pc:x}"),
                            internal: false,
                        })
                    }
                }
                return Err(StopExecution.into());
            }
            caller.data_mut().exit = Some(Exit::Signal {
                signal: signal::SIGTRAP,
                pc: pc as u64,
                detail: format!("brk #{imm}"),
                internal: false,
            });
            Err(StopExecution.into())
        },
    )?;

    linker.func_wrap(
        m,
        imports::SYSREG_READ,
        |_caller: Caller<'_, HostState>, _ctx: i32, _pc: i64, reg: i32| -> i64 {
            match reg {
                // CNTVCT_EL0. Served from the host clock at the frequency the
                // guest was told (CNTFRQ), which keeps its arithmetic exact.
                -3 => {
                    if let Some(c) = lockstep::counter() {
                        return c;
                    }
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default();
                    now.as_nanos() as i64
                }
                // MIDR_EL1: a plausible Cortex-A72.
                -7 => 0x410f_d083,
                _ => 0,
            }
        },
    )?;

    linker.func_wrap(
        m,
        imports::SYSREG_WRITE,
        |mut caller: Caller<'_, HostState>, ctx: i32, pc: i64, reg: i32, v: i64| -> Result<()> {
            // A function an ejected project replaced (`Intrinsic::Replace`).
            if reg == gex_xlate::intrinsic::REPLACE_SYSREG {
                return aot::call(&mut caller, ctx, pc as u64, v as u64);
            }
            // The x86-64 lifter routes `cpuid` here: the leaf and subleaf come
            // in `v`, the four results go back into the context.
            if reg == gex_kernel::x86::SYSREG_CPUID {
                let memory = caller
                    .data()
                    .memory
                    .clone()
                    .ok_or_else(|| anyhow!("guest memory unavailable"))?;
                let data = memory.bytes();
                let mut gm = GuestMem::new(data);
                gex_kernel::x86::serve_cpuid(&mut gm, ctx as u32 as u64, v as u64);
            }
            // An x87 transcendental or remainder, computed on the register
            // stack in the context.
            if reg == gex_kernel::x86::SYSREG_X87 {
                let memory = caller
                    .data()
                    .memory
                    .clone()
                    .ok_or_else(|| anyhow!("guest memory unavailable"))?;
                let mut gm = GuestMem::new(memory.bytes());
                gex_kernel::x86::serve_x87(&mut gm, ctx as u32 as u64, v as u64);
            }
            Ok(())
        },
    )?;

    linker.func_wrap(
        m,
        imports::CACHE_OP,
        |mut caller: Caller<'_, HostState>, _ctx: i32, _pc: i64, op: i32, addr: i64| -> Result<()> {
            if op == gex_abi::cache_op::DC_ZVA {
                let memory = caller
                    .data()
                    .memory
                    .clone()
                    .ok_or_else(|| anyhow!("guest memory unavailable"))?;
                let data = memory.bytes();
                let block = gex_abi::DCZID_BLOCK_BYTES;
                let start = ((addr as u64) & !(block - 1)) as usize;
                let end = start + block as usize;
                if end <= data.len() {
                    data[start..end].fill(0);
                }
            }
            // `ic ivau` tells the translator the bytes here are no longer the
            // instructions that were translated; without honouring it the guest
            // runs the previous compilation. Retired by dispatch page rather
            // than cache line — coarser, but a generator sweeps a whole
            // function so later lines find the page already retired.
            // A block entered for a neighbour sharing its dispatch slot
            // (x86-64): forget the neighbour so the miss that follows lifts it
            // again, with this block as a mate, into a trampoline.
            if op == gex_abi::cache_op::SLOT_CONFLICT {
                if let Some(shared) = caller.data().jit.as_ref().map(|j| Arc::clone(&j.shared)) {
                    let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                    sh.translated.remove(&(addr as u64));
                }
                return Ok(());
            }
            if op == gex_abi::cache_op::IC_INVALIDATE {
                let addr = addr as u64;
                let page = addr / gex_abi::DISPATCH_PAGE;
                retire_translations(
                    &mut caller,
                    &[(page * gex_abi::DISPATCH_PAGE, (page + 1) * gex_abi::DISPATCH_PAGE)],
                )?;
            }
            Ok(())
        },
    )?;

    // Fused multiply-add. The host has one; wasm does not, and a multiply
    // followed by an add rounds twice.
    linker.func_wrap(
        m,
        imports::FMA,
        |_caller: Caller<'_, HostState>, _ctx: i32, a: i64, b: i64, c: i64, double: i32| -> i64 {
            if double != 0 {
                f64::from_bits(a as u64)
                    .mul_add(f64::from_bits(b as u64), f64::from_bits(c as u64))
                    .to_bits() as i64
            } else {
                f32::from_bits(a as u32)
                    .mul_add(f32::from_bits(b as u32), f32::from_bits(c as u32))
                    .to_bits() as i64
            }
        },
    )?;

    linker.func_wrap(
        m,
        imports::SAFEPOINT,
        |mut caller: Caller<'_, HostState>, _ctx: i32, pc: i64| -> Result<()> {
            if let Some(s) = caller.data().sampler.clone() {
                s.sample(pc as u64);
            }
            profile::poll(&mut caller)?;
            // Same reason as the syscall hook: a loop that never calls the
            // kernel still reaches a back-edge, and this is where it does.
            catch_up_in_caller(&mut caller)?;
            let memory = caller
                .data()
                .memory
                .clone()
                .ok_or_else(|| anyhow!("guest memory unavailable"))?;
            let data = memory.bytes();
            let state = caller.data_mut();
            let mut gm = GuestMem::new(data);
            // The context is complete here, so a handler frame built now
            // captures exactly the state the guest would see on hardware.
            let ctx_addr = state.thread.ctx_addr;
            watch_check(&gm, pc as u64, state.thread.tid);
            let _ = gm.write_u64(ctx_addr + gex_abi::ctx::PC as u64, pc as u64);
            // Rearm before delivering, so a handler that loops still polls.
            let counter = ctx_addr as usize + gex_abi::ctx::SAFEPOINT_COUNTER;
            if counter + 4 <= gm.bytes.len() {
                gm.bytes[counter..counter + 4].copy_from_slice(&SAFEPOINT_INTERVAL.to_le_bytes());
            }
            // The process may have ended in another thread while this one was
            // running. A safepoint is the only place it is safe to notice.
            if state
                .spawn
                .as_ref()
                .is_some_and(|s| s.stopping.load(std::sync::atomic::Ordering::Acquire))
            {
                state.exit = Some(Exit::Status(0));
                return Err(StopExecution.into());
            }
            match state.with_kernel(|k| k.deliver_pending(&mut gm)) {
                gex_kernel::signal::Delivery::None => Ok(()),
                gex_kernel::signal::Delivery::Resume(target) => {
                    state.resume_at = Some(target);
                    Err(StopExecution.into())
                }
                gex_kernel::signal::Delivery::Terminate { signal, core } => {
                    state.exit = Some(Exit::Signal {
                        signal: signal as i32,
                        pc: pc as u64,
                        detail: format!(
                            "killed by signal {signal}{}",
                            if core { " (core dumped)" } else { "" }
                        ),
                        internal: false,
                    });
                    Err(StopExecution.into())
                }
            }
        },
    )?;

    Ok(())
}

/// Translate the block at `pc`, install it and everything reachable from it,
/// and report whether there was anything there to translate.
///
/// Blocks land in a module that *imports* the guest memory and the dispatch
/// table, so freshly translated code shares state with everything already
/// running and can branch into it through the table.
fn translate_and_install(caller: &mut Caller<'_, HostState>, pc: u64) -> Result<bool> {
    // With runtime translation off, a miss is still worth catching up on --
    // another thread or a freshly mapped object may already have covered this
    // address -- but nothing new is translated for it. A miss that survives
    // that is a hole in the static recompilation and should be reported as
    // one rather than quietly filled in.
    if !JIT_ON.load(std::sync::atomic::Ordering::Relaxed) {
        let replayed = replay_registry(caller)?;
        let covered = replayed && {
            let jit = caller.data().jit.as_ref().unwrap();
            let sh = jit.shared.lock().unwrap_or_else(|e| e.into_inner());
            sh.translated.contains(&pc)
        };
        return Ok(covered);
    }
    // Another thread may already have translated this code. Catching up on
    // the shared registry is far cheaper than translating again, and it is
    // also how a newly spawned thread fills a table that starts out holding
    // only what the image module put there.
    let replayed_any = replay_registry(caller)?;
    if replayed_any && {
        let jit = caller.data().jit.as_ref().unwrap();
        let sh = jit.shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.translated.contains(&pc)
    } {
        return Ok(true);
    }

    let (limits, shared) = {
        let jit = caller
            .data()
            .jit
            .as_ref()
            .ok_or_else(|| anyhow!("runtime translation is not set up"))?;
        (jit.limits, Arc::clone(&jit.shared))
    };
    // From here to the publish below, this is the only thread deciding which
    // table slots anything occupies. See `JitShared::translating`.
    let page_table = {
        let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.page_table.clone()
    };
    if pc % gex_abi::INST_ALIGN != 0 || pc >= gex_abi::DISPATCH_MAX_PAGES * gex_abi::DISPATCH_PAGE {
        return Ok(false);
    }
    // Only translate what the guest says is executable, so a wild branch into
    // data reports SIGSEGV rather than running whatever bytes are there.
    if !caller
        .data_mut()
        .with_kernel(|k| k.mem.check_access(pc, gex_abi::INST_ALIGN, prot::EXEC))
    {
        return Ok(false);
    }

    let memory = caller
        .data()
        .memory
        .clone()
        .ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let known: std::collections::HashSet<u64> = {
        let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.translated.clone()
    };
    let exec_ranges: Vec<(u64, u64)> = caller.data_mut().with_kernel(|k| {
        k.mem
            .vmas()
            .iter()
            .filter(|v| v.prot & prot::EXEC != 0)
            .map(|v| (v.start, v.end))
            .collect()
    });
    // Writable-and-executable ranges (a JIT's), whose blocks guard their own
    // bytes (x86-64; A64 announces its code writes with `ic ivau`). See
    // `Block::guard`.
    if gex_abi::X86 {
    let volatile: Vec<(u64, u64)> = caller.data_mut().with_kernel(|k| {
        k.mem
            .vmas()
            .iter()
            .filter(|v| v.prot & prot::EXEC != 0 && v.prot & prot::WRITE != 0)
            .map(|v| (v.start, v.end))
            .collect()
    });
    gex_xlate::set_volatile_ranges(volatile);
    }
    let (translated_module, stats) = {
        let data = memory.bytes();
        gex_xlate::translate_runtime(
            data,
            pc,
            &|a| exec_ranges.iter().any(|(s, e)| a >= *s && a + gex_abi::INST_ALIGN <= *e),
            &|a| known.contains(&a),
            page_table,
            limits,
            // A cap keeps one miss from translating an entire library
            // synchronously; the next miss picks up where this stopped.
            4096,
            true,
            &Default::default(),
        )
    };
    if std::env::var_os("GEX_TRACE_MISSES").is_some() {
        eprintln!("[miss] pc={pc:#x} blocks={}", stats.blocks);
    }
    if translated_module.exported_blocks.is_empty() {
        return Ok(false);
    }

    let engine = {
        let jit = caller.data().jit.as_ref().unwrap();
        jit.engine.clone()
    };
    // Cranelift is the expensive half of a miss and its output is a pure
    // function of the wasm, so it is cached by that hash across runs and
    // processes. Without the cache every dynamic program recompiles ld.so and
    // libc from scratch (eight seconds on `dpkg --version`).
    let cache = caller.data().jit.as_ref().and_then(|j| j.cache.clone());
    let module = match cache {
        Some((dir, build)) => {
            let key = gex_cache::key(&build, [translated_module.wasm.as_slice()]);
            let wasm = translated_module.wasm.clone();
            gex_cache::load_or_compile(&engine, &dir, &key, move || Ok((wasm, Vec::new())))
                .context("compiling runtime-translated wasm")?
                .module
        }
        None => Module::new(&engine, &translated_module.wasm)
            .context("validating runtime-translated wasm")?,
    };

    // Publish first, install second. The store's watermark only ever advances
    // through `replay_registry`, so installing here as well would double-count
    // whenever another thread appended between the catch-up above and this
    // push -- which is exactly what filled one store with ten thousand
    // instances of the same modules.
    publish_unit(&shared, &translated_module, module, stats.blocks)?;
    replay_registry(caller)?;

    // If the address that faulted did not end up with code, resuming would
    // fault again at the same place forever. Say so instead.
    let covered = {
        let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.translated.contains(&pc)
    };
    if !covered {
        bail!("translated blocks but none covered 0x{pc:x}");
    }
    Ok(true)
}

/// Replay anything this store has not seen, cheaply enough to call often: the
/// check is a length comparison, and the registry is append-only so a store
/// catches up by walking forward from its own watermark.
fn catch_up_in_caller(caller: &mut Caller<'_, HostState>) -> Result<()> {
    let behind = match caller.data().jit.as_ref() {
        Some(jit) => {
            let sh = jit.shared.lock().unwrap_or_else(|e| e.into_inner());
            sh.registry.len() > jit.replayed
        }
        None => false,
    };
    if behind {
        replay_registry(caller)?;
    }
    Ok(())
}

fn replay_registry(caller: &mut Caller<'_, HostState>) -> Result<bool> {
    let (shared, from) = {
        let Some(jit) = caller.data().jit.as_ref() else {
            return Ok(false);
        };
        (Arc::clone(&jit.shared), jit.replayed)
    };
    let pending: Vec<Arc<CompiledUnit>> = {
        let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        if sh.registry.len() <= from {
            return Ok(false);
        }
        sh.registry[from..].to_vec()
    };
    for unit in &pending {
        install_unit(&mut *caller, unit)?;
        caller.data_mut().jit.as_mut().unwrap().replayed += 1;
    }
    Ok(true)
}

/// Merge a finished translation into the shared state, checking the invariant
/// that no dispatch base is owned by two guest pages — if it were, an indirect
/// branch would land inside the wrong function and fail somewhere unrelated.
fn publish_unit(
    shared: &Arc<Mutex<JitShared>>,
    translated: &gex_xlate::TranslatedModule,
    module: Module,
    blocks: usize,
) -> Result<Arc<CompiledUnit>> {
    let unit = Arc::new(CompiledUnit {
        module,
        exported_blocks: translated.exported_blocks.clone(),
        dir_entries: translated.dir_entries.clone(),
        table_slots: translated.table_slots,
        page_table: translated.page_table.clone(),
    });
    let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
    for (page, base) in translated.page_table.entries() {
        match sh.slot_owner.entry(base) {
            std::collections::hash_map::Entry::Occupied(e) if *e.get() != page => {
                // Loudly, and not only through the error: an error out of here
                // becomes a guest fault somewhere unrelated, which is exactly
                // the kind of silence this check exists to break.
                eprintln!(
                    "gex: BUG: dispatch base {base} is claimed by guest page \
                     {:#x} and again by {page:#x}; slot allocation is not \
                     atomic",
                    e.get()
                );
                bail!("two guest pages were given the same dispatch slots");
            }
            std::collections::hash_map::Entry::Occupied(_) => {}
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(page);
            }
        }
    }
    sh.translated
        .extend(translated.exported_blocks.iter().map(|(a, _)| *a));
    let mut merged = sh.page_table.clone();
    merged.merge(&translated.page_table);
    sh.page_table = merged;
    sh.registry.push(Arc::clone(&unit));
    sh.blocks += blocks;
    sh.calls += 1;
    Ok(unit)
}

/// Instantiate one compiled unit into this store and point its table slots at
/// the result.
fn install_unit(
    mut cx: impl wasmtime::AsContextMut<Data = HostState>,
    unit: &CompiledUnit,
) -> Result<()> {
    let (memory, linker, tables) = {
        let data = cx.as_context().data();
        let jit = data
            .jit
            .as_ref()
            .ok_or_else(|| anyhow!("runtime translation is not set up"))?;
        (
            data.memory
                .clone()
                .ok_or_else(|| anyhow!("guest memory unavailable"))?,
            jit.linker.clone(),
            jit.tables.clone(),
        )
    };
    // Slots are allocated in one space that spans every table, so the figure
    // the unit carries is a running total and each table's share is that
    // total clamped to its own window. Natively only the first ever has any.
    let per = gex_xlate::PageTable::SLOTS_PER_TABLE as u64;
    let miss = tables[0]
        .get(cx.as_context_mut(), 0)
        .ok_or_else(|| anyhow!("dispatch table is empty"))?;
    for (i, t) in tables.iter().enumerate() {
        let want = (unit.table_slots as u64)
            .saturating_sub(i as u64 * per)
            .min(per);
        let have = t.size(cx.as_context_mut());
        if want > have {
            t.grow(cx.as_context_mut(), want - have, miss.clone())
                .with_context(|| format!("growing dispatch table {i}"))?;
        }
    }

    let mut linker = linker;
    linker.allow_shadowing(true);
    linker.define(
        cx.as_context_mut(),
        gex_abi::IMPORT_MODULE,
        exports::MEMORY,
        memory.shared().clone(),
    )?;
    for (i, t) in tables.iter().enumerate() {
        linker.define(
            cx.as_context_mut(),
            gex_abi::IMPORT_MODULE,
            exports::dispatch_table(i),
            *t,
        )?;
    }
    let proc = cx
        .as_context()
        .data()
        .spawn
        .as_ref()
        .and_then(|s| s.prof_proc.clone());
    profile::link(&mut linker, cx.as_context_mut(), &proc)?;
    let instance = linker
        .instantiate(cx.as_context_mut(), &unit.module)
        .context("instantiating a runtime-translated module")?;
    profile::attach(cx.as_context_mut(), &instance)?;

    // The module carries element segments for the table it imports, so
    // instantiating it already filled its slots. Writing the directory is
    // idempotent: it lives in shared guest memory.
    write_dispatch_dir(memory, &unit.dir_entries)?;
    cx.as_context_mut()
        .data_mut()
        .jit
        .as_mut()
        .unwrap()
        .instances
        .push(instance);
    Ok(())
}

/// Every dispatch table a translated image module exports, in index order.
/// Several because V8 caps a table at 10M entries; natively tables past the
/// first stay empty but must exist because the code names them.
fn dispatch_tables(
    store: &mut Store<HostState>,
    instance: &wasmtime::Instance,
) -> Result<Vec<Table>> {
    let mut out = Vec::new();
    for i in 0..gex_abi::DISPATCH_TABLES {
        match instance.get_export(&mut *store, exports::dispatch_table(i)) {
            Some(Extern::Table(t)) => out.push(t),
            // A module from before there were several exports only the first.
            // That is fine as long as nothing dispatches past it, which
            // natively nothing does.
            _ if i > 0 => break,
            _ => bail!("translated module does not export its dispatch table"),
        }
    }
    Ok(out)
}

/// Forget the translations of code the guest has unmapped. A dispatch slot
/// answers for an address, not the bytes at it, so after a `dlclose`+`dlopen`
/// reuses the range the guest would run the old library's code.
///
/// Three things go together: the page's directory entry (so a branch misses),
/// its slot assignment (so the next translation gets fresh slots), and its
/// addresses in `translated` (so discovery does not skip them). Old slots are
/// abandoned, not reused: replay safety (newest wins) needs a base to belong
/// to one page for the whole run.
fn retire_translations(
    caller: &mut impl wasmtime::AsContextMut<Data = HostState>,
    ranges: &[(u64, u64)],
) -> Result<()> {
    let Some(shared) = caller
        .as_context()
        .data()
        .jit
        .as_ref()
        .map(|j| Arc::clone(&j.shared))
    else {
        return Ok(());
    };
    let memory = caller
        .as_context()
        .data()
        .memory
        .clone()
        .ok_or_else(|| anyhow!("guest memory unavailable"))?;

    let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
    let mut pages = 0usize;
    for &(start, end) in ranges {
        sh.translated.retain(|a| *a < start || *a >= end);
        let (first, last) = (
            start / gex_abi::DISPATCH_PAGE,
            (end - 1) / gex_abi::DISPATCH_PAGE,
        );
        for page in first..=last {
            if sh.page_table.forget(page).is_some() {
                pages += 1;
            }
            let off = (gex_abi::DISPATCH_DIR + page * 4) as usize;
            let data = memory.bytes();
            if off + 4 <= data.len() {
                data[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
            }
        }
    }
    if pages > 0 && std::env::var_os("GEX_VERBOSE_OBJECTS").is_some() {
        eprintln!("[gex] retired {pages} dispatch pages the guest unmapped");
    }
    Ok(())
}

/// Publish page-directory entries so translated code becomes reachable.
fn write_dispatch_dir(memory: gex_mem::GuestMemory, entries: &[(u64, u32)]) -> Result<()> {
    let data = memory.bytes();
    for (page, base) in entries {
        let off = (gex_abi::DISPATCH_DIR + page * 4) as usize;
        if off + 4 > data.len() {
            bail!("dispatch directory entry for page {page} is out of range");
        }
        data[off..off + 4].copy_from_slice(&base.to_le_bytes());
    }
    Ok(())
}

/// The exported entry points of the object mapped at `map`, biased to where
/// it actually landed.
///
/// Empty when the file cannot be read or has no symbols, which tells
/// `translate_object` to scan instead.
fn object_seeds(
    caller: &mut impl wasmtime::AsContextMut<Data = HostState>,
    map: &gex_kernel::CodeMap,
) -> Vec<u64> {
    let Some(bytes) = caller
        .as_context_mut()
        .data_mut()
        .with_kernel(|k| k.read_file(&map.path))
    else {
        return Vec::new();
    };
    // Parsed at its own addresses; the bias is worked out from where the
    // executable segment was asked to go against where it ended up.
    let Ok(image) = gex_ld::elf::load(&bytes, 0) else {
        return Vec::new();
    };
    let Some(text) = image.segments.iter().find(|s| s.executable) else {
        return Vec::new();
    };
    let bias = map.start.wrapping_sub(text.vaddr & !0xfff);
    let (lo, hi) = (text.vaddr, text.vaddr + text.bytes.len() as u64);
    let mut seeds: Vec<u64> = image
        .symbols
        .iter()
        .filter(|s| s.value >= lo && s.value < hi)
        .map(|s| s.value.wrapping_add(bias))
        .collect();
    seeds.push(image.entry.wrapping_add(bias));

    // Every word in the object's own data that points into its own text.
    // Symbols and discovery miss a function whose address was only taken (a
    // vtable, jump table, callback), which is exactly what indirect branches
    // reach. Values are unrelocated, so the same bias applies.
    for seg in &image.segments {
        if seg.executable {
            continue;
        }
        for (i, chunk) in seg.bytes.chunks_exact(8).enumerate() {
            if !(seg.vaddr as usize + i * 8).is_multiple_of(8) {
                continue;
            }
            let v = u64::from_le_bytes(chunk.try_into().unwrap());
            if v >= lo && v < hi && v % gex_abi::INST_ALIGN == 0 {
                seeds.push(v.wrapping_add(bias));
            }
        }
    }
    seeds.sort_unstable();
    seeds.dedup();
    seeds
}

/// A mapping larger than this is not a shared library, and recompiling it
/// would be a long pause for nothing.
const MAX_OBJECT_BYTES: u64 = 64 << 20;

/// A runaway guard on one object, high enough not to bind on anything real: it
/// exists only so a range that decodes as code but is not cannot translate
/// forever. Objects split across as many modules as they need
/// ([`gex_xlate::MAX_FUNCS_PER_MODULE`]), so this no longer bounds a real
/// object's size.
const MAX_OBJECT_BLOCKS: usize = 4_000_000;

/// Statically recompile a shared library the guest has just mapped. Doing the
/// whole object at once makes the output depend only on the library and its
/// load address (unlike the demand-driven path, whose result varied with which
/// address was reached first), so it caches and settles.
fn recompile_object(
    caller: &mut impl wasmtime::AsContextMut<Data = HostState>,
    map: &gex_kernel::CodeMap,
    seeds: &[u64],
) -> Result<()> {
    if caller.as_context().data().jit.is_none() {
        return Ok(());
    }
    // The interpreter maps its own segments before it has a kernel to ask, so
    // a range with nothing decodable in it is normal and not worth reporting.
    let span = map.end.saturating_sub(map.start);
    if span == 0 || span > MAX_OBJECT_BYTES {
        return Ok(());
    }
    let (limits, shared) = {
        let jit = caller.as_context().data().jit.as_ref().unwrap();
        (jit.limits, Arc::clone(&jit.shared))
    };
    let page_table = {
        let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.page_table.clone()
    };
    let known: std::collections::HashSet<u64> = {
        let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.translated.clone()
    };
    let memory = caller
        .as_context()
        .data()
        .memory
        .clone()
        .ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let exec_ranges: Vec<(u64, u64)> = caller.as_context_mut().data_mut().with_kernel(|k| {
        k.mem
            .vmas()
            .iter()
            .filter(|v| v.prot & prot::EXEC != 0)
            .map(|v| (v.start, v.end))
            .collect()
    });
    // Start from the library's exported functions (`.dynsym` survives
    // stripping): those are the addresses anything outside can call, and
    // discovery does the rest. Scanning every decodable address is complete
    // but finds ~200,000 interior blocks in libc that entry blocks cover.
    let started = std::time::Instant::now();
    let (modules, stats) = {
        let data = memory.bytes();
        gex_xlate::translate_object(
            data,
            map.start,
            map.end,
            seeds,
            &|a| exec_ranges.iter().any(|(s, e)| a >= *s && a + gex_abi::INST_ALIGN <= *e),
            &|a| known.contains(&a),
            page_table,
            limits,
            MAX_OBJECT_BLOCKS,
            true,
            &Default::default(),
        )
    };
    if modules.iter().all(|m| m.exported_blocks.is_empty()) {
        return Ok(());
    }
    let engine = caller.as_context().data().jit.as_ref().unwrap().engine.clone();
    let cache = caller
        .as_context()
        .data()
        .jit
        .as_ref()
        .and_then(|j| j.cache.clone());
    // An object too large for one wasm module comes back as several, in
    // address order. They are published in that order because each one's page
    // assignments are the next one's starting point.
    let n_modules = modules.len();
    let mut wasm_bytes = 0usize;
    let lifted = started.elapsed();
    for translated in &modules {
        if translated.exported_blocks.is_empty() {
            continue;
        }
        wasm_bytes += translated.wasm.len();
        let module = match &cache {
            Some((dir, build)) => {
                let key = gex_cache::key(build, [translated.wasm.as_slice()]);
                let wasm = translated.wasm.clone();
                gex_cache::load_or_compile(&engine, dir, &key, move || Ok((wasm, Vec::new())))
                    .context("compiling a statically recompiled object")?
                    .module
            }
            None => Module::new(&engine, &translated.wasm)
                .context("validating a statically recompiled object")?,
        };
        let unit = publish_unit(&shared, translated, module, translated.n_blocks)?;
        install_unit(caller.as_context_mut(), &unit)?;
        caller.as_context_mut().data_mut().jit.as_mut().unwrap().replayed += 1;
    }
    if std::env::var("GEX_VERBOSE_OBJECTS").is_ok() {
        eprintln!(
            "[gex] recompiled {} [{:#x}..{:#x}): {} blocks in {:.2}s \
             ({:.2}s to wasm, {:.2}s in cranelift), {:.1} MB of wasm{}",
            map.path,
            map.start,
            map.end,
            stats.blocks,
            started.elapsed().as_secs_f64(),
            lifted.as_secs_f64(),
            (started.elapsed() - lifted).as_secs_f64(),
            wasm_bytes as f64 / (1 << 20) as f64,
            if n_modules > 1 {
                format!(" across {n_modules} modules")
            } else {
                String::new()
            }
        );
    }
    Ok(())
}

/// Whether a trap is a dispatch through a table slot that is not there yet.
///
/// Both shapes mean the same thing: the slot is past the end of this store's
/// table, or within it but never filled.
fn is_stale_table_trap(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<wasmtime::Trap>(),
        Some(wasmtime::Trap::TableOutOfBounds) | Some(wasmtime::Trap::IndirectCallToNull)
    )
}

/// Replay what other threads have translated and say where to resume.
///
/// `None` when there was nothing to replay, which means the trap was not a
/// stale table after all and the caller should report it.
fn catch_up_and_resume(store: &mut Store<HostState>, ctx_addr: u64) -> Result<Option<u64>> {
    let behind = match store.data().jit.as_ref() {
        Some(jit) => {
            let sh = jit.shared.lock().unwrap_or_else(|e| e.into_inner());
            sh.registry.len() > jit.replayed
        }
        None => return Ok(None),
    };
    if !behind {
        return Ok(None);
    }
    catch_up_table(store)?;
    let Some(memory) = store.data().memory.clone() else {
        return Ok(None);
    };
    let pc = read_u64(memory.bytes(), (ctx_addr + ctx::PC as u64) as usize);
    Ok((pc != 0).then_some(pc))
}

fn classify_trap(e: anyhow::Error, store: &Store<HostState>) -> anyhow::Error {
    let pc = store.data().memory.as_ref().map(|_| 0u64).unwrap_or(0);
    if let Some(trap) = e.downcast_ref::<wasmtime::Trap>() {
        let detail = match trap {
            // The guard in tier-1 codegen steers a too-large effective
            // address into the unmapped top page, so this is exactly a guest
            // wild pointer (PLAN §7.2).
            wasmtime::Trap::MemoryOutOfBounds => {
                "guest memory access out of bounds (SIGSEGV)".to_string()
            }
            wasmtime::Trap::HeapMisaligned => {
                "unaligned atomic access (SIGBUS on hardware)".to_string()
            }
            wasmtime::Trap::IntegerDivisionByZero => {
                "integer division by zero escaped the lifter's guard".to_string()
            }
            other => format!("wasm trap: {other}"),
        };
        return anyhow!("{detail} (pc≈0x{pc:x})");
    }
    e
}
