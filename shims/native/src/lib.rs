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
use gex_kernel::{BlockOp, GuestMem, Kernel, ProcState, SyscallOutcome, ThreadState};
use std::sync::{Arc, Mutex};
use gex_ld::{build_initial_stack, Identity, Image, Program};
use gex_xlate::{Options, Stats};
use wasmtime::{
    Caller, Config, Engine, Extern, Linker, Module, Ref, Store, Table, Val,
};

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
    /// Translate code on demand when a branch reaches an address with no
    /// translation.
    pub jit: bool,
    /// Reuse compiled modules across runs (PLAN §10).
    pub cache: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            args: Vec::new(),
            env: Vec::new(),
            trace: false,
            verbose: false,
            sysroot: None,
            jit: false,
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
    /// Everything needed to translate and install code at run time.
    jit: Option<Jit>,
    /// Everything a new guest thread needs to build a store of its own.
    spawn: Option<Arc<SpawnContext>>,
    /// Set by a `clone` that a host thread still has to be started for.
    pending_spawn: Option<ThreadState>,
}

/// What a guest thread needs to start running.
///
/// A wasmtime store belongs to one host thread, so a new guest thread cannot
/// be handed the parent's: it builds its own and instantiates the same
/// modules into it. Everything here is either immutable or already shared.
struct SpawnContext {
    engine: Engine,
    /// The image module, instantiated fresh in each thread's store.
    image_module: Module,
    /// A linker with only the `gex.*` imports registered -- no store-bound
    /// definitions, so it can be cloned into another thread.
    linker: Linker<HostState>,
    memory: gex_mem::GuestMemory,
    kernel: Arc<Mutex<Kernel>>,
    proc_state: Arc<Mutex<ProcState>>,
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
    let image = &program.exe;

    // A profile records where a previous run's execution actually went, which
    // is how gex learns the entry points static discovery cannot reach. It
    // feeds `extra_entries`, so it changes the translation and therefore the
    // cache key: a run that discovers something new compiles once more and
    // then settles.
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
        eprintln!("[gex] module ready in {:.2}s", phase.elapsed().as_secs_f64());
    }
    let phase = std::time::Instant::now();

    let ctx_addr = gex_abi::ctx_addr(0);
    // `/proc/self/exe` is the resolved program, not argv[0]: a shell that
    // re-executes itself relies on the difference.
    let exe_path = opts
        .args
        .first()
        .and_then(|a| std::fs::canonicalize(a).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| opts.args.first().cloned().unwrap_or_default());
    let kernel = Kernel::new(ctx_addr, image.brk, exe_path.clone());
    // Report the host's identity: with hostfs passthrough, a guest that
    // stats a file and then calls getuid must see a consistent answer.
    let (uid, gid) = host_identity();
    let proc_state = Arc::new(Mutex::new(ProcState {
        pid: 1,
        maps: String::new(),
        cmdline: Vec::new(),
        environ: Vec::new(),
        exe: exe_path,
        cwd: "/".into(),
        fds: Vec::new(),
    }));
    let mut kernel = Kernel {
        trace: opts.trace,
        uid,
        gid,
        argv: opts.args.clone(),
        envp: opts.env.clone(),
        ..kernel
    };
    mount_filesystems(&mut kernel, opts, Arc::clone(&proc_state))?;

    let mut store = Store::new(
        &engine,
        HostState {
            kernel: Arc::new(Mutex::new(kernel)),
            thread: ThreadState::new(ctx_addr, 1),
            memory: None,
            proc_state,
            exit: None,
            resume_at: None,
            jit: None,
            spawn: None,
            pending_spawn: None,
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
                page_table: gex_xlate::PageTable::from_entries(
                    &meta.dir_entries,
                    meta.next_slot,
                ),
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
        eprintln!("[gex] guest state placed in {:.2}s", phase.elapsed().as_secs_f64());
    }
    // Everything a new guest thread needs. The linker here is the pristine
    // one -- only the `gex.*` imports -- because a linker carrying store-bound
    // definitions could not be cloned into another thread.
    let spawn = Arc::new(SpawnContext {
        engine: engine.clone(),
        image_module: module.clone(),
        linker: base_linker,
        memory: memory.clone(),
        kernel: Arc::clone(&store.data().kernel),
        proc_state: Arc::clone(&store.data().proc_state),
        jit_shared: store.data().jit.as_ref().map(|j| Arc::clone(&j.shared)),
        limits: xlate.limits,
    });
    store.data_mut().spawn = Some(Arc::clone(&spawn));

    let exit = run_thread_loop(&mut store, &instance, ctx_addr, program.entry())?;

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

/// Perform a syscall's blocking half with the kernel unlocked.
fn perform_block(op: BlockOp, data: &mut [u8]) -> Result<u64, gex_kernel::errno::Errno> {
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
    }
}

/// Run one guest thread until it stops.
///
/// A loop rather than a single call because a signal, a `sigreturn`, a
/// freshly translated block or a `clone` all redirect execution by unwinding
/// out of wasm and re-entering at a new address. `clone` in particular has to
/// start its host thread from here: doing it inside the syscall import would
/// mean spawning while a wasm frame is live and the store is borrowed.
fn run_thread_loop(
    store: &mut Store<HostState>,
    instance: &wasmtime::Instance,
    ctx_addr: u64,
    entry: u64,
) -> Result<Exit> {
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
        if let Some(child) = store.data_mut().pending_spawn.take() {
            let cx = store
                .data()
                .spawn
                .clone()
                .ok_or_else(|| anyhow!("clone with no spawn context"))?;
            // Detached: a guest thread is joined through its own futex, not
            // through the host's handle, and `exit_group` ends the process
            // whatever its threads are doing.
            std::thread::Builder::new()
                .name(format!("gex-tid-{}", child.tid))
                .spawn(move || {
                    if let Err(e) = run_guest_thread(cx, child) {
                        eprintln!("gexc: thread {} failed: {e:#}", child.tid);
                    }
                })
                .context("spawning a guest thread")?;
        }
        match result {
            // Falling off the end of translated code means the guest returned
            // from its entry point, which Linux treats as an exit.
            Ok(()) => return Ok(store.data().exit.clone().unwrap_or(Exit::Status(0))),
            Err(e) => {
                if let Some(next) = store.data_mut().resume_at.take() {
                    pc = next;
                    continue;
                }
                match store.data().exit.clone() {
                    Some(x) => return Ok(x),
                    None => return Err(classify_trap(e, store)),
                }
            }
        }
    }
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
            jit: None,
            spawn: Some(Arc::clone(&cx)),
            pending_spawn: None,
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
    let _ = run_thread_loop(&mut store, &instance, thread.ctx_addr, entry)?;
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
    kernel.vfs.mount(
        "/proc",
        Arc::new(ProcFs::new(Arc::new(SharedProc(proc_state)))),
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
    let _ = kernel.vfs.set_cwd(if opts.sysroot.is_some() { "/" } else { &cwd });
    Ok(())
}

/// A `/proc` source over the snapshot the host refreshes.
struct SharedProc(Arc<Mutex<ProcState>>);

impl gex_kernel::vfs::ProcSource for SharedProc {
    fn pid(&self) -> i32 {
        self.0.lock().unwrap().pid
    }
    fn maps(&self) -> String {
        self.0.lock().unwrap().maps.clone()
    }
    fn cmdline(&self) -> Vec<u8> {
        self.0.lock().unwrap().cmdline.clone()
    }
    fn environ(&self) -> Vec<u8> {
        self.0.lock().unwrap().environ.clone()
    }
    fn exe(&self) -> String {
        self.0.lock().unwrap().exe.clone()
    }
    fn cwd(&self) -> String {
        self.0.lock().unwrap().cwd.clone()
    }
    fn fds(&self) -> Vec<(i32, String)> {
        self.0.lock().unwrap().fds.clone()
    }
    fn stat(&self) -> String {
        let s = self.0.lock().unwrap();
        format!("{} (gex) R 0 {} {} 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 0\n", s.pid, s.pid, s.pid)
    }
    fn status(&self) -> String {
        let s = self.0.lock().unwrap();
        format!("Name:\tgex\nState:\tR (running)\nPid:\t{}\nThreads:\t1\n", s.pid)
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
    (
        field("Uid:").unwrap_or(1000),
        field("Gid:").unwrap_or(1000),
    )
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
            let memory = caller
                .data()
                .memory
                .clone()
                .ok_or_else(|| anyhow!("guest memory unavailable"))?;
            let data = memory.bytes();
            let state = caller.data_mut();
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
                caller.data_mut().with_kernel(|k| k.finish_blocking(&mut gm, r))
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
                        detail: format!(
                            "branch to 0x{pc:x}, which is not executable guest memory"
                        ),
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
                        detail: format!(
                            "instruction 0x{raw:08x} at 0x{pc:x} is not implemented"
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

    linker.func_wrap(
        m,
        imports::SAFEPOINT,
        |mut caller: Caller<'_, HostState>, _ctx: i32, pc: i64| -> Result<()> {
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
                gm.bytes[counter..counter + 4]
                    .copy_from_slice(&SAFEPOINT_INTERVAL.to_le_bytes());
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
    if replay_registry(caller)? && {
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
        )
    };
    if translated_module.exported_blocks.is_empty() {
        return Ok(false);
    }

    let (engine, linker) = {
        let jit = caller.data().jit.as_ref().unwrap();
        (jit.engine.clone(), jit.linker.clone())
    };
    let module = Module::new(&engine, &translated_module.wasm)
        .context("validating runtime-translated wasm")?;

    // Grow the shared table to cover the pages this module claimed, filling
    // the new slots with the miss stub so an unreached address in a newly
    // covered page still asks for translation.
    let table = caller.data().jit.as_ref().unwrap().table;
    let have = table.size(&mut *caller);
    if translated_module.table_slots > have {
        let miss = table
            .get(&mut *caller, 0)
            .ok_or_else(|| anyhow!("dispatch table is empty"))?;
        table
            .grow(&mut *caller, translated_module.table_slots - have, miss)
            .context("growing the dispatch table")?;
    }

    // The new module imports the memory and the table by name. The memory is
    // already defined on the linker this was cloned from -- it has to be, the
    // image module imports it too -- and the table is redefined per call, so
    // both are shadowing writes.
    let mut linker = linker;
    linker.allow_shadowing(true);
    linker.define(
        &mut *caller,
        gex_abi::IMPORT_MODULE,
        exports::MEMORY,
        memory.shared().clone(),
    )?;
    linker.define(
        &mut *caller,
        gex_abi::IMPORT_MODULE,
        exports::DISPATCH_TABLE,
        table,
    )?;
    let instance = linker
        .instantiate(&mut *caller, &module)
        .context("instantiating runtime-translated module")?;

    // Install each block, then publish the page directory entries. Doing it
    // in that order means a page never becomes reachable before the slots it
    // points at hold real code.
    let mut installed = Vec::with_capacity(translated_module.exported_blocks.len());
    for (addr, name) in &translated_module.exported_blocks {
        let Some(Extern::Func(f)) = instance.get_export(&mut *caller, name) else {
            bail!("runtime module is missing its export {name}");
        };
        let page = gex_abi::dispatch_page(*addr);
        // The base comes from the shared page assignment, not from this
        // module's list of *new* pages: a block often lands in a page some
        // earlier translation already claimed.
        let Some(base) = translated_module.page_table.lookup(page) else {
            bail!("no dispatch page allocated for block 0x{addr:x}");
        };
        let slot = gex_abi::dispatch_index(*addr, base);
        table
            .set(&mut *caller, slot, Ref::Func(Some(f)))
            .with_context(|| format!("installing block 0x{addr:x} in slot {slot}"))?;
        installed.push(*addr);
    }
    write_dispatch_dir(memory, &translated_module.dir_entries)?;

    // If the address that faulted did not end up with code, resuming would
    // fault again at the same place forever. Say so instead.
    if !installed.contains(&pc) {
        bail!("translated {} blocks but none covered 0x{pc:x}", installed.len());
    }

    {
        let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.translated.extend(installed);
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
    let jit = caller.data_mut().jit.as_mut().unwrap();
    jit.instances.push(instance);
    // This store now holds the unit it just pushed.
    jit.replayed += 1;
    Ok(true)
}

/// Instantiate any registry entries this store has not seen yet.
///
/// Returns whether anything was replayed. The registry is append-only and
/// ordered, so a store catches up by walking forward from its own watermark;
/// two stores that have replayed the same count hold the same code.
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
