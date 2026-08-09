//! The native host: wasmtime plus the kernel bindings.
//!
//! This is the CI workhorse of PLAN §9. It owns the wasm engine, wires the
//! `gex.*` imports to `gex-kernel`, places the guest image and initial stack
//! in linear memory, and calls `enter`. The browser shim will do the same
//! things through Workers and a SharedArrayBuffer; keeping the two hosts thin
//! is what makes a single kernel implementation serve both.

#![forbid(unsafe_code)]

use anyhow::{anyhow, bail, Context, Result};
use gex_abi::{ctx, exports, imports, GUEST_MEM_PAGES, IMPORT_MODULE};
use gex_kernel::mem::prot;
use gex_kernel::vfs::{DevFs, HostFs, ProcFs, TmpFs};
use gex_kernel::{BlockOp, GuestMem, Kernel, KernelProcSource, ProcState, SyscallOutcome, ThreadState};
use gex_ld::{build_initial_stack, Identity, Image, Program};
use gex_xlate::{Options, Stats};
use std::sync::{Arc, Mutex};
pub mod gl;
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
        /// True when the signal reflects a gex limitation rather than the
        /// guest's own behaviour — an instruction we cannot execute, an
        /// address with no translation. Those deserve a message on stderr; an
        /// ordinary fatal signal does not, because Linux prints nothing
        /// either and the shell reports the exit status.
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

    /// The status `wait4` reports, which is not the same number.
    ///
    /// A shell shows `128 + signal` for a death by signal, and that is what
    /// [`Exit::code`] gives. The kernel's own encoding is different: the
    /// signal goes in the low seven bits, with `0x80` set if a core would
    /// have been written, and a normal exit puts its code in bits 8 to 15.
    /// `WIFSIGNALED` tests the low bits, so storing `128 + signal` makes a
    /// killed child look like one that exited normally with a large code --
    /// which is what gex reported, and why `timeout` could not tell that the
    /// program it killed had been killed.
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
    /// Who the guest thinks it is. `None` reports the host's own ids, which
    /// is what a passthrough filesystem needs to stay consistent -- a guest
    /// that stats a file it owns and then calls `getuid` must get the same
    /// answer. A sysroot is a different situation: it is the guest's whole
    /// world, and a package manager in it expects to be root.
    pub identity: Option<(u32, u32)>,
    /// Sample the guest's back-edges and report the hottest at exit.
    pub sample: bool,
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
    /// letting the runtime translator find it a dispatch miss at a time.
    ///
    /// Off by default, and the reason is measured rather than assumed. On
    /// `dpkg --version` the demand-driven path costs 0.60s warm; recompiling
    /// every object costs 32s warm and three and a half minutes cold. The
    /// modules are the problem: seeding from every decodable address gives
    /// 200,000 blocks for libc alone, and instantiating that into each store
    /// swamps everything, while the program touches a few thousand of them.
    ///
    /// It is kept because the *approach* is right and the seeding is wrong.
    /// Seeding from the dynamic symbol table instead of from every address
    /// would cut it by an order of magnitude, and a tier-2 pass over a whole
    /// object can see things a demand-driven one cannot.
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
            wayland: None,
            // On unless the build cannot render or the caller says
            // `--no-virgl`.
            //
            // It costs nothing until something connects: the name is bound,
            // and the host renderer -- which is a Mesa initialisation -- is
            // not brought up until a client arrives. What it buys is that an
            // unmodified OpenGL program in the guest simply works, which is
            // the whole point of serving VirGL rather than an API of our own.
            //
            // Off-by-default was worse than it looks. Mesa's vtest winsys
            // does not fall back when the socket is missing: it takes the
            // ECONNREFUSED, writes its handshake into the dead descriptor
            // anyway, and then blocks reading a reply that cannot come.
            // `glxinfo` under a forgotten `--virgl` does not fail, it hangs
            // in `read` forever, and nothing on the way there says why.
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
            // On by default: it is the difference between half a minute and
            // half a second on a program the size of SQLite, and a cache miss
            // costs only the write.
            cache: true,
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
    limits: gex_lift::BlockLimits,
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
    /// Every dispatch table, in index order; `tables[0]` is the one that
    /// holds the miss span and everything a program this size ever needs.
    ///
    /// There are several because V8 caps a table at ten million entries, and
    /// the emitted code selects between them with a `br_table`, so every
    /// index the code names has to exist even here where nothing reaches the
    /// second. Natively there is no such cap and tables 1 and up stay empty.
    tables: Vec<Table>,
    /// Where compiled runtime translations are kept, and the build they
    /// belong to. `None` when the cache is off or this build cannot name
    /// itself.
    cache: Option<(std::path::PathBuf, String)>,
    /// Instances live here because the table holds funcrefs into them.
    instances: Vec<wasmtime::Instance>,
    /// Registry entries already instantiated into this store.
    replayed: usize,
    limits: gex_lift::BlockLimits,
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

/// A trap used to unwind out of translated code.
///
/// Both stopping and *redirecting* use it. Redirecting is what signal
/// delivery needs: the guest's registers are already in the context at every
/// host call, so abandoning the wasm stack and re-entering at a new PC loses
/// nothing (PLAN §2.4). Unwinding this way also keeps the common syscall path
/// free of a return-value check.
#[derive(Debug)]
struct StopExecution;

impl std::fmt::Display for StopExecution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "guest execution finished")
    }
}

impl std::error::Error for StopExecution {}

/// The cache key for an image: everything that determines the wasm.
///
/// The *placed* segments rather than the file, because that is what the
/// translator reads -- two builds of the same program that differ only in a
/// section the loader drops should share an entry, and the same file loaded at
/// a different bias must not.
fn cache_key(image: &Image, xlate: &Options, build: &str) -> String {
    let mut fields: Vec<Vec<u8>> = vec![
        image.bias.to_le_bytes().into(),
        image.entry.to_le_bytes().into(),
        image.text_range.0.to_le_bytes().into(),
        image.text_range.1.to_le_bytes().into(),
        format!("{xlate:?}").into_bytes(),
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

/// Build, instantiate and run a guest image to completion.
/// Check that a module is well-formed wasm, without compiling or running it.
///
/// Emitting a module that does not validate is never useful, and the lifter
/// can produce one: an `smov x0, v0.h[2]` asked for a 64-bit value from a lane
/// extract that yields a 32-bit one, and nothing noticed until wasmtime
/// refused the module 182 MB in. Validation is a fraction of the cost of
/// translating, so `gexc build` does it every time, and an exhaustive
/// translation of a large real binary is the best lifter test there is --
/// it reaches encodings no program gex has ever run would execute.
pub fn validate_module(wasm: &[u8]) -> Result<()> {
    let mut config = Config::new();
    config.wasm_tail_call(true);
    config.wasm_threads(true);
    config.wasm_bulk_memory(true);
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
    run_program_with(program, opts, xlate, None)
}

/// `run_program`, optionally reusing a kernel from a process that `execve`d.
///
/// Exec keeps the process: its pid, its descriptors, its working directory.
/// What it replaces is the address space and the program in it, which from
/// here is the same work as starting fresh -- so this is the same function,
/// told not to build a new kernel.
/// What survives an `execve`.
///
/// The kernel and the process table, obviously. Less obviously the host-side
/// devices: the compositor and the virtio-gpu renderer each bind a name in
/// the guest's socket namespace, and `execve` re-enters the function that
/// created them. Building a second one rebinds the name, so a client that
/// connects afterwards is accepted by the new device while the process that
/// would service it holds the old -- which is a guest blocked forever on a
/// reply nobody is going to write. A shell's fork-then-exec hits this every
/// time; `glxinfo` under Xvfb was the case that found it.
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
) -> Result<(Exit, Stats)> {
    let image = &program.exe;

    // A profile records where a previous run's execution actually went, which
    // is how gex learns the entry points static discovery cannot reach. It
    // feeds `extra_entries`, so it changes the translation and therefore the
    // cache key: a run that discovers something new compiles once more and
    // then settles.
    //
    // Worth knowing before measuring anything: "once more" is a whole
    // recompilation, and Cranelift on a megabyte of guest text is tens of
    // seconds. A binary with many independent entry points pays it once per
    // path. `busybox md5sum` took 29 seconds and then 0.148; `busybox wget`
    // took 27 and then 0.656. Both look exactly like a runtime problem in the
    // program being measured, and neither is -- so warm the path first, or
    // pass --no-cache and measure the build honestly.
    // A build that cannot identify itself must not use the cache: it cannot
    // be distinguished from a different one, and reusing another build's
    // compiled code is how a fixed translator serves broken output.
    let build = gex_cache::build_fingerprint();
    let use_cache = opts.cache && build.is_some();
    let build = build.unwrap_or_default();
    let profile_dir = use_cache.then(gex_cache::profile_dir).flatten();
    let image_key = cache_key(image, &Options::default(), &build);
    let mut xlate = xlate.clone();
    // Off unless asked for, because it was measured and did not pay.
    //
    // The profile records where execution actually went, so the next
    // translation can include entry points static discovery cannot reach.
    // The catch is that it changes the translation and therefore the cache
    // key, so the second run of every program recompiles from scratch --
    // for, on Debian bash, 230 extra blocks out of 51,000.
    //
    // Measured four consecutive runs, wall clock, with compilation parallel:
    //
    //     bash            with 9s 6s 0s 1s     without 8s 1s 1s 2s
    //     busybox md5sum  with 6s 5s 0s 0s     without 5s 1s 0s 0s
    //
    // Both reach the same steady state. The profile costs five or six
    // seconds on the second run and buys nothing measurable after it. The
    // coverage is real -- fewer dispatch misses -- it just does not pay for
    // its own recompilation on programs of this size, and those are the
    // programs people run.
    //
    // Still written on every run, so turning it on costs nothing but the
    // recompile it asks for. Kept rather than deleted because a long-running
    // program with many cold indirect branches is the case that could still
    // want it, and that case has not been measured.
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
    // gex's own output is already the optimised form -- the register cache,
    // dead-flag elimination and loop fusion have run. What Cranelift adds on
    // top is register allocation and peepholes, and on a large program the
    // time it takes to add them dominates everything else: a 14 MB module out
    // of SQLite costs half a minute to compile and under two seconds to
    // translate. `GEX_OPT_LEVEL` picks the trade.
    config.cranelift_opt_level(match std::env::var("GEX_OPT_LEVEL").as_deref() {
        Ok("none") => wasmtime::OptLevel::None,
        Ok("speed_and_size") => wasmtime::OptLevel::SpeedAndSize,
        _ => wasmtime::OptLevel::Speed,
    });
    let engine = Engine::new(&config).context("creating the wasm engine")?;

    // Only the executable is translated ahead of time. The interpreter and
    // everything it maps arrive through runtime translation, which is both
    // simpler and correct: at build time we do not know where they land.
    //
    // Translation happens inside the cache's miss path, so a hit skips it
    // outright -- on SQLite that is another second and a half. The `stats`
    // a hit reports are therefore empty apart from the block count the
    // sidecar carries, because nothing recomputed them.
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

    let (module, meta) = match use_cache.then(gex_cache::cache_dir).flatten() {
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
            k.exe_path = exe_path.clone();
            k.argv = opts.args.clone();
            k.envp = opts.env.clone();
            k.live_threads = 1;
            k.next_ctx_slot = 1;
            (k, ps)
        }
        None => (
            Kernel::new(ctx_addr, image.brk, exe_path.clone()),
            // `..Default::default()` for the rest, deliberately. This site
            // listed every field, so adding one to `ProcState` broke it --
            // and it broke the *native* shim only, which is how `gexc` sat
            // unbuilt for hours while the browser target compiled fine and
            // every check came back green.
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
    // Mounts survive an exec, so they are only established for a new process.
    if fresh {
        mount_filesystems(&mut kernel, opts, Arc::clone(&proc_state))?;
    }
    // The compositor binds a name in the guest's unix-socket namespace, which
    // is shared across the process tree, so a client the guest forks later
    // finds it. Bound before anything runs, because a client that connects
    // before the name exists gets ECONNREFUSED and gives up.
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

    let instance = linker
        .instantiate(&mut store, &module)
        .context("instantiating the translated module")?;
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
    // to build new modules with. Holding them here is what lets a dispatch
    // miss become a translation rather than an error.
    let tables = dispatch_tables(&mut store, &instance)?;
    let table = tables[0];
    // The machinery here is "install a translated module into this store": a
    // table, a linker, a registry other stores replay from. Translating on a
    // dispatch miss is one user of it; statically recompiling a whole object
    // the moment it is mapped is another, and that one is not runtime
    // translation at all. `--no-jit` turns off the miss handler, not this.
    if opts.jit || opts.recompile_objects {
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

    place_image(
        &mut store,
        memory.clone(),
        program,
        opts,
        Identity {
            uid,
            gid,
            interp_base: program.interp_base(),
        },
    )?;

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

    let stopped = run_thread_loop(&mut store, &instance, ctx_addr, program.entry())?;
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

    // The process is over. Every other thread has to stop before the stores
    // and the memory they are using go away: set the flag their safepoints
    // watch, and release anyone parked on a futex, since no guest code is
    // going to wake them now. They are given a bounded moment to notice --
    // long enough for a thread between safepoints, short enough that a thread
    // wedged in a host call cannot hold up the exit.
    stop_other_threads(&spawn, opts.verbose);

    if let Some(s) = &sampler {
        s.report(&image.symbols, 20);
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

/// Perform a syscall's blocking half with the kernel unlocked.
/// What one store may hold.
///
/// wasmtime defaults to ten thousand instances per store, which is a sensible
/// number for a program that instantiates a few modules and a fatal one here:
/// gex adds an instance per runtime translation, so the ceiling is a bound on
/// how much guest code a process may execute. `cc1` compiling SQLite's
/// amalgamation reached it and died -- and died as a segmentation fault at a
/// guest address, because a translation that cannot be instantiated is
/// reported to the guest as a fault. A 9 MB translation unit looked like a
/// miscompile; a 900 KB one worked, which made it look like one that depended
/// on size. It was a resource limit all along.
///
/// The numbers are deliberately far above anything real rather than merely
/// above what SQLite needed. Tables and memories are raised with instances
/// because every instance may bring one.
fn store_limits() -> wasmtime::StoreLimits {
    wasmtime::StoreLimitsBuilder::new()
        .instances(1 << 20)
        .tables(1 << 20)
        .memories(1 << 20)
        .build()
}

/// Run a blocking operation with the kernel unlocked.
///
/// `service` is called on every turn of every wait loop. It is how anything
/// the host provides keeps running while the guest is asleep -- and without
/// it the compositor is a deadlock: a client writes a request, blocks waiting
/// for the reply, and the only thing that would produce the reply runs when
/// the guest makes a syscall, which it will not do again until the reply
/// arrives. Xwayland found that, and found it intermittently, because a guest
/// that happens to poll for other reasons breaks the cycle by accident.
/// Run a syscall that had to wait, outside the kernel lock.
///
/// `interrupted` asks whether a signal has arrived that the guest can see. A
/// blocking syscall has to give up when one has, with `EINTR`, or the handler
/// only runs once the wait finishes on its own -- which is not a signal
/// arriving late, it is the signal not working. `timeout 3 sleep 30` came
/// back after thirty seconds reporting success; `alarm` could not bound
/// anything; nothing could interrupt a read.
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
        // Natively this is nearly unreachable: the host's own stdin blocks, so
        // a stream is only ever "not readable yet" for a pipe whose writer has
        // not written. Polling is enough for that, and it keeps the kernel
        // unlocked while it happens.
        // Both of these are polls. Natively there is a real thread here, so
        // sleeping in it costs nothing anyone else needs.
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
            // Which process is waiting, for a table shared between address
            // spaces. There is one address space here, so the children list
            // above is the whole answer.
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
                // A halt reaches a parked `wait4` here. `EINTR` is what the
                // guest would see if a signal had arrived, and it sends every
                // caller back round to a syscall -- where the halt check
                // stops it.
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
                        // A thread that fails takes its process with it. On
                        // Linux an unhandled fault is fatal to the whole
                        // thread group, and here it also has to be: the other
                        // threads would otherwise sit in a futex forever, the
                        // process would never exit, and every descriptor it
                        // holds would stay open. A reader waiting on the other
                        // end of one of its pipes waits for good.
                        //
                        // That is how a trap in dpkg's decompressor turned
                        // into `apt-get install` hanging rather than failing.
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
                // A dispatch through a slot this thread's table has not
                // reached yet.
                //
                // Every thread of a process shares the dispatch directory --
                // it is in guest memory -- but each has its own wasm table,
                // filled only when its own store installs a translation. So
                // one thread can publish a directory entry naming slot N
                // while another's table is still shorter than N, and that
                // thread's next branch is not a recoverable miss but a trap.
                //
                // It is recoverable here, and exactly, because both indirect
                // transfers write the target into the context before they
                // take it: the guest made no progress, and its own record of
                // where it was going is correct. Catch up and re-enter there.
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
/// A forked child is the same program at the same addresses, so it can reuse
/// every module the parent has translated -- what it cannot share is the
/// memory those modules act on. It gets a fresh one with the parent's mapped
/// regions copied into it, which is the whole of what `fork` means here: one
/// address space per process, and no copy-on-write, because wasm gives the
/// host no way to fault on a write.
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
                    // A process that dies takes its threads and its
                    // descriptors with it. Without this the threads sit in a
                    // futex nobody will wake, they hold the last references
                    // to the kernel, and every pipe the process had stays
                    // open -- so whoever is reading the other end waits for
                    // an end of file that cannot come. A trap in dpkg's
                    // decompressor hung `apt-get install` that way rather
                    // than failing it.
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
            // still running -- become init's rather than being lost.
            procs.exit(pid, code);
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
    let instance = linker
        .instantiate(&mut store, &cx.image_module)
        .context("instantiating the image module for a child process")?;
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
    match run_thread_loop(&mut store, &instance, thread.ctx_addr, entry)? {
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
    let instance = linker
        .instantiate(&mut store, &cx.image_module)
        .context("instantiating the image module for a new thread")?;
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
    if let Stopped::Exec(_) = run_thread_loop(&mut store, &instance, thread.ctx_addr, entry)? {
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

/// A `/proc` source over the snapshot the host refreshes.
/// Where the guest spends its time, sampled at back-edge safepoints.
///
/// A safepoint fires every thousand-odd back-edges, so the sample is biased
/// towards loops -- which is exactly the bias wanted. A program that is
/// spinning is a program taking back-edges, and this says which ones.
///
/// It is not a general profiler: straight-line code between loops is
/// invisible to it, and so is anything the guest does without looping. What
/// it is good at is answering "what is it doing" when the answer is "going
/// round and round somewhere".
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

/// Whether to print the guest's registers and stack when it aborts.
///
/// `abort` is the end of the road for a guest: it raises SIGABRT at itself and
/// nothing runs afterwards, so unless the state is captured at the syscall it
/// is gone. glibc's `__stack_chk_fail` arrives here, and the interesting
/// question -- which function's canary failed -- is answerable only from the
/// return addresses still on the stack.
static DUMP_ABORT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A watch on a range of guest memory: `GEX_WATCH=<hex addr>[+<hex len>]`.
///
/// Corruption is found by asking who wrote to a place, and a translated guest
/// has no hardware watchpoint to ask with. This is the next best thing: the
/// bytes are remembered, and every safepoint and every syscall compares them
/// and reports the first pc at which they differ. A back-edge poll is a coarse
/// net -- the write happened somewhere in the loop, not at the pc printed --
/// but it narrows a whole program down to one loop, which is the hard part.
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

/// Print the register file and the return addresses on the stack.
///
/// There is no unwind information here and none is wanted: a scan of the stack
/// for words that point just after a `bl` in an executable mapping finds the
/// call chain and some noise, and the noise is easy to read past. Each hit is
/// reported against the mapping that contains it, so an address in a library
/// is a name and an offset rather than a bare number.
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

/// The guest's network, as the host's real sockets.
///
/// The kernel wants a transport that never blocks -- it does the waiting
/// itself, because it is what can park a guest thread -- so the socket is put
/// in non-blocking mode and a background reader is not needed: the kernel
/// polls, and polling a non-blocking socket is a syscall.
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
        // A zero-length read cannot say whether anything is there, so this
        // peeks one byte and puts it back by never taking it: `MSG_PEEK` is
        // not in std, so the socket is asked to read into a scratch buffer
        // the caller will read again -- which would lose the byte. Instead,
        // ask whether a read would block, which is what `WouldBlock` means.
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
    write_u64(data, ctx_addr + ctx::SP, layout.sp);
    write_u64(data, ctx_addr + ctx::PC, image.entry);
    data[ctx_addr + ctx::SAFEPOINT_COUNTER..ctx_addr + ctx::SAFEPOINT_COUNTER + 4]
        .copy_from_slice(&SAFEPOINT_INTERVAL.to_le_bytes());
    for i in 0..31u8 {
        write_u64(data, ctx_addr + ctx::x(i), 0);
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
            // Every thread of a process shares the dispatch directory -- it
            // is in guest memory -- but each has its own wasm table, and a
            // table only gains a translation when its own store installs it.
            // So a thread can be told by the directory to call slot N while
            // its table is still shorter than N, which is not a dispatch miss
            // it can recover from but a hard trap with nowhere to land.
            //
            // dpkg's decompressor hit this: two threads entering the same
            // freshly translated loop, one of them a moment behind.
            //
            // Catching up here costs a comparison of two integers on the
            // common path and closes the window to whatever a thread can
            // execute between one syscall and the next.
            catch_up_in_caller(&mut caller)?;
            let memory = caller
                .data()
                .memory
                .clone()
                .ok_or_else(|| anyhow!("guest memory unavailable"))?;
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
            // A shared library has just been mapped. Recompile the whole
            // object now rather than letting the runtime translator find it
            // one dispatch miss at a time.
            // Code that has gone away. Done before the newly mapped code
            // below, because a `dlclose` followed by a `dlopen` lands the new
            // object on the addresses the old one had, and retiring after
            // would throw away the translation just made for it.
            let retired: Vec<(u64, u64)> = caller
                .data_mut()
                .with_kernel(|k| std::mem::take(&mut k.retired_code));
            if !retired.is_empty() {
                retire_translations(&mut caller, &retired)?;
            }
            let mapped: Vec<gex_kernel::CodeMap> = caller
                .data_mut()
                .with_kernel(|k| std::mem::take(&mut k.new_code));
            let want_objects = RECOMPILE_OBJECTS.load(std::sync::atomic::Ordering::Relaxed);
            for m in mapped.into_iter().filter(|_| want_objects) {
                // Seeded from what the object says about itself: its
                // exported symbols, and every address its own data takes.
                //
                // A block at every decodable address is the only *complete*
                // seeding, and it is quadratic -- starting a block at each of
                // the L instructions in a basic block emits L blocks totalling
                // L(L+1)/2 instructions. libc comes out at 297,215 blocks and
                // a 217 MB compiled module, 128 times its own text, and ten
                // seconds to map. Leaders give 62,085 blocks.
                //
                // What leaders cannot give is certainty. An indirect branch
                // can go to an address no symbol names and no data holds, and
                // in a stripped object built without branch protection --
                // which bookworm's libc is -- nothing in the file says where.
                // That residue is what the runtime translator is for.
                // Which seeding depends on whether anything is behind this.
                // With the runtime translator on, leaders are enough and far
                // cheaper. With it off, the static pass is the only pass and
                // has to be complete, whatever that costs.
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
                    caller.data_mut().exit = Some(Exit::Signal {
                        signal: signal::SIGSEGV,
                        pc,
                        detail: format!("branch to 0x{pc:x}, which is not executable guest memory"),
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
                    state.exit = Some(Exit::Signal {
                        signal: signal::SIGILL,
                        pc,
                        detail: format!("instruction 0x{raw:08x} at 0x{pc:x} is not implemented"),
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
        |mut caller: Caller<'_, HostState>, _ctx: i32, pc: i64, imm: i32| -> Result<()> {
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
        |_caller: Caller<'_, HostState>, _ctx: i32, _pc: i64, _reg: i32, _v: i64| {},
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
            // `ic ivau` is the guest saying, in the only way the architecture
            // gives it, that the bytes at this address are no longer the
            // instructions that were there. It is what every code generator
            // emits after writing, and the one hook a translator cannot
            // ignore: a dispatch slot answers for an address, so without this
            // the guest calls its newly compiled function and runs the
            // previous one.
            //
            // Retirement is by dispatch page rather than by the cache line
            // the instruction names. Coarser than it needs to be, and cheap
            // in the shape this actually arrives in: a generator writes a
            // whole function, then sweeps it with one `ic ivau` per line, and
            // every line after the first in a page finds it already retired.
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
    if pc % 4 != 0 || pc >= gex_abi::DISPATCH_MAX_PAGES * gex_abi::DISPATCH_PAGE {
        return Ok(false);
    }
    // Only translate what the guest says is executable, so a wild branch into
    // data reports SIGSEGV rather than running whatever bytes are there.
    if !caller
        .data_mut()
        .with_kernel(|k| k.mem.check_access(pc, 4, prot::EXEC))
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
    let (translated_module, stats) = {
        let data = memory.bytes();
        gex_xlate::translate_runtime(
            data,
            pc,
            &|a| exec_ranges.iter().any(|(s, e)| a >= *s && a + 4 <= *e),
            &|a| known.contains(&a),
            page_table,
            limits,
            // A cap keeps one miss from translating an entire library
            // synchronously; the next miss picks up where this stopped.
            4096,
            true,
        )
    };
    if translated_module.exported_blocks.is_empty() {
        return Ok(false);
    }

    let engine = {
        let jit = caller.data().jit.as_ref().unwrap();
        jit.engine.clone()
    };
    // Cranelift is the expensive half of a miss -- the lift above takes
    // milliseconds and this takes seconds -- and its output is a pure
    // function of the wasm handed to it. So it is cached by the hash of
    // exactly that, across runs and across processes.
    //
    // This matters far more than it looks. Only the executable is translated
    // ahead of time; the interpreter and every shared library arrive through
    // this path. Without the cache, every run of every dynamic program
    // recompiles ld.so and libc from scratch -- `dpkg --version` loads its
    // own module in 0.03s and then spends eight seconds here -- and a package
    // manager forks a dozen such processes.
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

/// Instantiate any registry entries this store has not seen yet.
///
/// Returns whether anything was replayed. The registry is append-only and
/// ordered, so a store catches up by walking forward from its own watermark;
/// two stores that have replayed the same count hold the same code.
/// Replay anything this store has not seen, cheaply enough to call often.
///
/// The check is a length comparison; the work only happens when another
/// thread has translated something since.
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

/// Merge a finished translation into the shared state.
///
/// Page bases come from a mark shared by every clone of the page table (see
/// `PageTable`), so two translations running at once cannot be handed the same
/// slots however long either of them takes. This is the check that the
/// property still holds: if a base ever turns up owned by two different guest
/// pages, an indirect branch to one of them will land inside a function
/// belonging to the other, and the guest will fail somewhere else entirely --
/// which is how this cost a day to find the first time.
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
    let instance = linker
        .instantiate(cx.as_context_mut(), &unit.module)
        .context("instantiating a runtime-translated module")?;

    // Nothing to install by hand: the module carries element segments for
    // the table it imports, so instantiating it above already filled every
    // slot it owns. This loop used to be a name lookup and a table write per
    // block, and on a whole shared library it cost more than the compile.
    // Idempotent: the directory lives in shared guest memory, so a second
    // store writing the same entries changes nothing.
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
///
/// There is more than one because V8 caps a table at ten million entries and
/// the emitted code selects between them with a `br_table`; natively there is
/// no such cap and every table past the first stays empty, but they still
/// have to exist and be linked, because the code names them.
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

/// Forget the translations of code the guest has unmapped.
///
/// A dispatch slot answers for a guest address, not for the bytes that were
/// at it. `dlclose` unmaps a library; a later `dlopen` maps a different one
/// over the same range; and the directory still points the old translations
/// at those addresses. The guest calls into the new library and runs the old
/// one's code. Nothing reports an error -- an X server resetting after its
/// last client disconnects simply dies somewhere unrelated, inside its own
/// dynamic loader, several dlopens later.
///
/// Three things have to go, together:
///
///   * the page's directory entry, so a branch there misses;
///   * the page's slot assignment, so the next translation is given fresh
///     slots rather than rewriting the ones an older unit still owns;
///   * the addresses in `translated`, so discovery does not skip them as
///     already done.
///
/// The old slots are abandoned rather than reused. Reuse would mean a stale
/// unit replayed into a new store could refill them, and the ordering that
/// makes replay safe -- newest wins -- only holds while a base belongs to one
/// page for the life of the run.
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

/// Turn a wasm trap into a guest signal.
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
    //
    // This is what makes symbol seeding complete enough to run without a
    // runtime translator behind it. Discovery finds what a block branches to;
    // symbols find what another object can call. Neither finds a function
    // whose address was only ever taken -- a vtable, a jump table, an
    // initialiser array, a callback stored in a struct -- and those are
    // exactly the targets of the indirect branches that were dying.
    //
    // The alternative is starting a block at every decodable address, which
    // is complete and costs 200,000 blocks for libc against a few tens of
    // thousands here, nearly all of them interior addresses nothing ever
    // jumps to.
    //
    // The values are unrelocated, so they are the library's own vaddrs and
    // the same bias applies.
    for seg in &image.segments {
        if seg.executable {
            continue;
        }
        for (i, chunk) in seg.bytes.chunks_exact(8).enumerate() {
            if !(seg.vaddr as usize + i * 8).is_multiple_of(8) {
                continue;
            }
            let v = u64::from_le_bytes(chunk.try_into().unwrap());
            if v >= lo && v < hi && v % 4 == 0 {
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

/// A cap on one object, high enough not to bind on anything real: a megabyte
/// of guest text is a quarter of a million instructions and so at most that
/// many blocks. It exists only so a mapping of something that is not code
/// cannot run away.
///
/// It used to be 200,000, which libc exceeded -- and with no runtime
/// translator behind it, a capped object is an object the guest dies in.
/// This path always has one: `recompile_object` returns early without it.
///
/// It was 900,000 for a while, and that number came from wasm rather than
/// from taste: a module may hold a million functions, gex emits one per block
/// plus one per region, and libz3 -- 22 MB of guest text, 993,645 blocks and
/// 18,583 regions -- came to about 1,012,000. It missed by one percent, and
/// the whole 37-second translation was thrown away for it. Capping kept most
/// of the work but left the tail to the runtime translator, which is exactly
/// what static recompilation is for.
///
/// Objects are now split across as many modules as they need
/// ([`gex_xlate::MAX_FUNCS_PER_MODULE`]), so the per-module ceiling no longer
/// bounds the object. What is left is the runaway guard the constant was
/// originally for: a range that decodes as code but is not, or a discovery
/// bug, should stop rather than translate forever.
const MAX_OBJECT_BLOCKS: usize = 4_000_000;

/// Statically recompile a shared library the guest has just mapped.
///
/// The point of doing it here, all at once, is that it is *static*: the whole
/// object is translated from its own bytes, so the result depends on the
/// library and where it was loaded and on nothing else. The demand-driven
/// path produces a different module depending on which address the guest
/// reached first, which is why its output never settled and its cache never
/// quite hit.
///
/// Cached by the hash of exactly what was translated -- the mapped bytes and
/// the base address -- so the second run of any program using the library
/// pays nothing.
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
    // Where to start: the library's own exported functions. A shipped object
    // keeps `.dynsym` even when it has been stripped of `.symtab`, and those
    // symbols are exactly the addresses anything outside can call. Discovery
    // does the rest -- successors, and the folded target of a PLT's indirect
    // branch.
    //
    // Falling back to scanning every decodable address is correct but not
    // affordable: on libc it finds 200,000 blocks where the symbols find a
    // small fraction of that, and the difference is all interior addresses
    // that a block starting at the function entry already covers.
    let started = std::time::Instant::now();
    let (modules, stats) = {
        let data = memory.bytes();
        gex_xlate::translate_object(
            data,
            map.start,
            map.end,
            seeds,
            &|a| exec_ranges.iter().any(|(s, e)| a >= *s && a + 4 <= *e),
            &|a| known.contains(&a),
            page_table,
            limits,
            MAX_OBJECT_BLOCKS,
            true,
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
