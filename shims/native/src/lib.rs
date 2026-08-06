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
    /// Shared across threads and inherited by children, so a profile of a
    /// process tree is one profile.
    sampler: Option<Arc<Sampler>>,
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
    table: Table,
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
fn run_program_with(
    program: &Program,
    opts: &RunOptions,
    xlate: &Options,
    inherited: Option<(Kernel, Arc<Mutex<ProcState>>)>,
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
    if let Some(dir) = &profile_dir {
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
    // `/proc/self/exe` is the resolved program, not argv[0]: a shell that
    // re-executes itself relies on the difference.
    let exe_path = opts
        .args
        .first()
        .and_then(|a| std::fs::canonicalize(a).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| opts.args.first().cloned().unwrap_or_default());
    let fresh = inherited.is_none();
    let (kernel, proc_state) = match inherited {
        Some((mut k, ps)) => {
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
            Arc::new(Mutex::new(ProcState {
                pid: 1,
                ppid: 0,
                pgid: 1,
                sid: 1,
                maps: String::new(),
                cmdline: Vec::new(),
                environ: Vec::new(),
                exe: exe_path,
                cwd: "/".into(),
                fds: Vec::new(),
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
            jit: None,
            spawn: None,
            pending_spawn: None,
            pending_fork: None,
            pending_exec: None,
        },
    );

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
    let table = match instance.get_export(&mut store, exports::DISPATCH_TABLE) {
        Some(Extern::Table(t)) => t,
        _ => bail!("translated module does not export its dispatch table"),
    };
    if opts.jit {
        store.data_mut().jit = Some(Jit {
            shared: Arc::new(Mutex::new(JitShared {
                page_table: gex_xlate::PageTable::from_entries(&meta.dir_entries, meta.next_slot),
                translated: Default::default(),
                registry: Vec::new(),
                blocks: 0,
                calls: 0,
            })),
            engine: engine.clone(),
            linker: linker.clone(),
            table,
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
        sampler: sampler.clone(),
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

    let stopped = run_thread_loop(&mut store, &instance, ctx_addr, program.entry())?;
    if let Stopped::Exec(req) = stopped {
        stop_other_threads(&spawn, opts.verbose);
        let kernel = take_kernel(&spawn);
        return exec_program(req, opts, xlate, kernel, Arc::clone(&spawn.proc_state));
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
fn perform_block(op: BlockOp, data: &mut [u8]) -> Result<u64, gex_kernel::errno::Errno> {
    use gex_kernel::errno::Errno;
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
            std::thread::sleep(d);
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
                    std::thread::sleep(std::time::Duration::from_millis(1));
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
                if gex_kernel::syscalls::poll_ready(&fds) {
                    let mut mem = GuestMem::new(data);
                    return gex_kernel::syscalls::poll_finish(&fds, &report, &mut mem);
                }
                if deadline.is_some_and(|d| start.elapsed() >= d) {
                    let mut mem = GuestMem::new(data);
                    return gex_kernel::syscalls::poll_finish(&fds, &report, &mut mem);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        BlockOp::WriteStream { handle, buf, len } => {
            let data = GuestMem::new(data).read(buf, len)?.to_vec();
            loop {
                if handle.poll_writable() {
                    return handle.write_at(0, &data);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        BlockOp::ReadStream { handle, buf, len } => {
            let mut tmp = vec![0u8; len];
            loop {
                if handle.poll_readable() {
                    let n = handle.read_at(0, &mut tmp)?;
                    let mut mem = GuestMem::new(data);
                    mem.write(buf, &tmp[..n as usize])?;
                    return Ok(n);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        BlockOp::WaitChild {
            children,
            procs,
            want,
            wstatus,
            nohang,
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
                            // The wait status Linux reports for a normal
                            // exit: the code in bits 8..16.
                            let mut mem = GuestMem::new(data);
                            mem.write_u32(wstatus, ((code as u32) & 0xff) << 8)?;
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
        // One profile for the whole process tree: a package manager spends
        // most of its time in programs it started.
        sampler: cx.sampler.clone(),
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
            let code = match run_guest_process(child_cx, thread, entry) {
                Ok(exit) => exit.code(),
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
                    127
                }
            };
            // Through the table, so this process's own children -- if any are
            // still running -- become init's rather than being lost.
            procs.exit(pid, code);
            let (st, done) = status;
            *st.lock().unwrap_or_else(|e| e.into_inner()) = Some(code);
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
            jit: None,
            spawn: Some(Arc::clone(&cx)),
            pending_spawn: None,
            pending_fork: None,
            pending_exec: None,
        },
    );
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
    let table = match instance.get_export(&mut store, exports::DISPATCH_TABLE) {
        Some(Extern::Table(t)) => t,
        _ => bail!("translated module does not export its dispatch table"),
    };
    if let Some(shared) = &cx.jit_shared {
        store.data_mut().jit = Some(Jit {
            shared: Arc::clone(shared),
            engine: cx.engine.clone(),
            linker: cx.linker.clone(),
            table,
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
            exec_program(req, &opts, &cx.xlate, kernel, Arc::clone(&cx.proc_state))
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
fn exec_program(
    req: ExecRequest,
    opts: &RunOptions,
    xlate: &Options,
    mut kernel: Kernel,
    proc_state: Arc<Mutex<ProcState>>,
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
    let opts = RunOptions {
        args: req.argv,
        env: req.envp,
        ..opts.clone()
    };
    run_program_with(&program, &opts, xlate, Some((kernel, proc_state)))
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
            jit: None,
            spawn: Some(Arc::clone(&cx)),
            pending_spawn: None,
            pending_fork: None,
            pending_exec: None,
        },
    );
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
    let table = match instance.get_export(&mut store, exports::DISPATCH_TABLE) {
        Some(Extern::Table(t)) => t,
        _ => bail!("translated module does not export its dispatch table"),
    };
    if let Some(shared) = &cx.jit_shared {
        store.data_mut().jit = Some(Jit {
            shared: Arc::clone(shared),
            engine: cx.engine.clone(),
            linker: cx.linker.clone(),
            table,
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
    kernel.vfs.mount("/dev", Arc::new(DevFs::new()));
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
            let outcome = state.with_kernel(|k| {
                if !gex_kernel::skips_proc_snapshot(nr) {
                    k.refresh_proc(&proc_state);
                }
                k.syscall(&mut gm, pc as u64)
            });
            // A blocking syscall is run here, with the kernel unlocked, and
            // then finished under the lock again.
            let outcome = if let SyscallOutcome::Blocking(op) = outcome {
                let r = perform_block(op, memory.bytes());
                let mut gm = GuestMem::new(memory.bytes());
                caller
                    .data_mut()
                    .with_kernel(|k| k.finish_blocking(&mut gm, r))
            } else {
                outcome
            };
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
        |caller: Caller<'_, HostState>, _ctx: i32, _pc: i64, op: i32, addr: i64| -> Result<()> {
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
            // IC IVAU / ISB are the self-modifying-code hooks (PLAN §6.2).
            // With no runtime translation yet there is nothing to invalidate.
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
    let module = Module::new(&engine, &translated_module.wasm)
        .context("validating runtime-translated wasm")?;

    // Publish first, install second. The store's watermark only ever advances
    // through `replay_registry`, so installing here as well would double-count
    // whenever another thread appended between the catch-up above and this
    // push -- which is exactly what filled one store with ten thousand
    // instances of the same modules.
    {
        let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.translated
            .extend(translated_module.exported_blocks.iter().map(|(a, _)| *a));
        sh.page_table = translated_module.page_table.clone();
        sh.registry.push(Arc::new(CompiledUnit {
            module,
            exported_blocks: translated_module.exported_blocks.clone(),
            dir_entries: translated_module.dir_entries.clone(),
            table_slots: translated_module.table_slots,
            page_table: translated_module.page_table.clone(),
        }));
        sh.blocks += stats.blocks;
        sh.calls += 1;
    }
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

/// Instantiate one compiled unit into this store and point its table slots at
/// the result.
fn install_unit(
    mut cx: impl wasmtime::AsContextMut<Data = HostState>,
    unit: &CompiledUnit,
) -> Result<()> {
    let (memory, linker, table) = {
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
            jit.table,
        )
    };

    let have = table.size(cx.as_context_mut());
    if unit.table_slots > have {
        let miss = table
            .get(cx.as_context_mut(), 0)
            .ok_or_else(|| anyhow!("dispatch table is empty"))?;
        table
            .grow(cx.as_context_mut(), unit.table_slots - have, miss)
            .context("growing the dispatch table")?;
    }

    let mut linker = linker;
    linker.allow_shadowing(true);
    linker.define(
        cx.as_context_mut(),
        gex_abi::IMPORT_MODULE,
        exports::MEMORY,
        memory.shared().clone(),
    )?;
    linker.define(
        cx.as_context_mut(),
        gex_abi::IMPORT_MODULE,
        exports::DISPATCH_TABLE,
        table,
    )?;
    let instance = linker
        .instantiate(cx.as_context_mut(), &unit.module)
        .context("instantiating a runtime-translated module")?;

    for (addr, name) in &unit.exported_blocks {
        let Some(Extern::Func(f)) = instance.get_export(cx.as_context_mut(), name) else {
            bail!("runtime module is missing its export {name}");
        };
        let page = gex_abi::dispatch_page(*addr);
        let Some(base) = unit.page_table.lookup(page) else {
            bail!("no dispatch page allocated for block 0x{addr:x}");
        };
        let slot = gex_abi::dispatch_index(*addr, base);
        table
            .set(cx.as_context_mut(), slot, Ref::Func(Some(f)))
            .with_context(|| format!("installing block 0x{addr:x} in slot {slot}"))?;
    }
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
