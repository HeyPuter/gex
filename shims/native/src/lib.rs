//! The native host: wasmtime plus the kernel bindings.
//!
//! This is the CI workhorse of PLAN §9. It owns the wasm engine, wires the
//! `gex.*` imports to `gex-kernel`, places the guest image and initial stack
//! in linear memory, and calls `enter`. The browser shim will do the same
//! things through Workers and a SharedArrayBuffer; keeping the two hosts thin
//! is what makes a single kernel implementation serve both.

#![forbid(unsafe_code)]

use anyhow::{anyhow, bail, Context, Result};
use gex_abi::{ctx, exports, imports, IMPORT_MODULE};
use gex_kernel::mem::prot;
use gex_kernel::vfs::{DevFs, HostFs, ProcFs, TmpFs};
use gex_kernel::{GuestMem, Kernel, ProcState, SyscallOutcome};
use std::sync::{Arc, Mutex};
use gex_ld::{build_initial_stack, Identity, Image, Program};
use gex_xlate::{Options, Stats};
use wasmtime::{
    Caller, Config, Engine, Extern, Linker, Memory, Module, Ref, Store, Table, Val,
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
#[derive(Clone, Debug, Default)]
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
}

/// Host-side state threaded through every import.
struct HostState {
    kernel: Kernel,
    memory: Option<Memory>,
    /// Snapshot `/proc` reads from, refreshed before each syscall that could
    /// observe it.
    proc_state: Arc<Mutex<ProcState>>,
    /// Set when a host function decided the run must stop.
    exit: Option<Exit>,
    /// Set when a host function wants execution to continue elsewhere.
    resume_at: Option<u64>,
    /// Everything needed to translate and install code at run time.
    jit: Option<Jit>,
}

/// Runtime translation state (PLAN §6.2, M3).
struct Jit {
    engine: Engine,
    linker: Linker<HostState>,
    table: Table,
    /// Which guest pages own which table slots, carried across compilations
    /// so a page keeps one base for the life of the process.
    page_table: gex_xlate::PageTable,
    /// Addresses whose dispatch slot now holds real code.
    translated: std::collections::HashSet<u64>,
    /// Modules compiled so far, kept alive because the table holds funcrefs
    /// into them.
    modules: Vec<wasmtime::Instance>,
    limits: gex_lift::BlockLimits,
    /// Diagnostics, reported under --verbose.
    blocks: usize,
    calls: usize,
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
    // Only the executable is translated ahead of time. The interpreter and
    // everything it maps arrive through runtime translation, which is both
    // simpler and correct: at build time we do not know where they land.
    let (module_bytes, stats) = gex_xlate::translate_image(image, xlate);
    if opts.verbose {
        eprintln!(
            "[gex] translated {} blocks / {} instructions -> {} KiB wasm ({} dispatch slots, \
             {} entry points from data, {} untranslatable blocks, {} dead flag statements)",
            stats.blocks,
            stats.instructions,
            stats.wasm_bytes / 1024,
            module_bytes.table_slots,
            stats.discovered_from_data,
            stats.illegal_blocks,
            stats.dead_flag_stmts,
        );
    }

    let mut config = Config::new();
    // Tail calls are load-bearing: the whole block-per-function design in
    // PLAN §2.4 depends on them.
    config.wasm_tail_call(true);
    config.wasm_threads(true);
    config.wasm_bulk_memory(true);
    let engine = Engine::new(&config).context("creating the wasm engine")?;

    let module = Module::new(&engine, &module_bytes.wasm).context("validating translated wasm")?;

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
            kernel,
            memory: None,
            proc_state,
            exit: None,
            resume_at: None,
            jit: None,
        },
    );

    let mut linker = Linker::new(&engine);
    register_imports(&mut linker)?;

    let instance = linker
        .instantiate(&mut store, &module)
        .context("instantiating the translated module")?;

    let memory = match instance.get_export(&mut store, exports::MEMORY) {
        Some(Extern::Memory(m)) => m,
        _ => bail!("translated module does not export its memory"),
    };
    store.data_mut().memory = Some(memory);

    // Runtime translation needs the table to patch and the engine and linker
    // to build new modules with. Holding them here is what lets a dispatch
    // miss become a translation rather than an error.
    let table = match instance.get_export(&mut store, exports::DISPATCH_TABLE) {
        Some(Extern::Table(t)) => t,
        _ => bail!("translated module does not export its dispatch table"),
    };
    if opts.jit {
        store.data_mut().jit = Some(Jit {
            engine: engine.clone(),
            linker: linker.clone(),
            table,
                page_table: module_bytes.page_table.clone(),
            translated: Default::default(),
            modules: Vec::new(),
            limits: xlate.limits,
            blocks: 0,
            calls: 0,
        });
    }

    place_image(
        &mut store,
        memory,
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
        let data = memory.data_mut(&mut store);
        for (page, base) in &module_bytes.dir_entries {
            let off = (gex_abi::DISPATCH_DIR + page * 4) as usize;
            if off + 4 <= data.len() {
                data[off..off + 4].copy_from_slice(&base.to_le_bytes());
            }
        }
    }

    let enter = instance
        .get_func(&mut store, exports::ENTER)
        .ok_or_else(|| anyhow!("translated module does not export `enter`"))?;

    // The run is a loop rather than a single call because a signal, a
    // sigreturn or (from M3) a freshly translated block redirects execution
    // by unwinding and re-entering at a new address.
    let mut pc = program.entry();
    let exit = loop {
        let result = enter.call(
            &mut store,
            &[Val::I32(ctx_addr as i32), Val::I64(pc as i64)],
            &mut [],
        );
        match result {
            // Falling off the end of translated code means the guest returned
            // from its entry point, which Linux treats as an exit.
            Ok(()) => break store.data().exit.clone().unwrap_or(Exit::Status(0)),
            Err(e) => {
                if let Some(next) = store.data_mut().resume_at.take() {
                    pc = next;
                    continue;
                }
                match store.data().exit.clone() {
                    Some(x) => break x,
                    None => return Err(classify_trap(e, &store)),
                }
            }
        }
    };
    if opts.verbose {
        if let Some(jit) = store.data().jit.as_ref() {
            eprintln!(
                "[gex] runtime translation: {} blocks in {} compilations",
                jit.blocks, jit.calls
            );
        }
    }
    Ok((exit, stats))
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
    memory: Memory,
    program: &Program,
    opts: &RunOptions,
    id: Identity,
) -> Result<()> {
    let image = &program.exe;
    // Segments first: the executable's, then the interpreter's.
    for seg in program.segments() {
        let data = memory.data_mut(&mut *store);
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
        store
            .data_mut()
            .kernel
            .mem
            .insert(seg.vaddr, seg.vaddr + seg.mem_size, p, "[image]");
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
    let data = memory.data_mut(&mut *store);
    let start = layout.low as usize;
    let end = start + layout.bytes.len();
    if end > data.len() {
        bail!("initial stack does not fit in guest memory");
    }
    data[start..end].copy_from_slice(&layout.bytes);

    let stack_low = gex_abi::STACK_TOP - gex_abi::STACK_SIZE;
    store.data_mut().kernel.mem.insert(
        stack_low,
        gex_abi::STACK_TOP,
        prot::READ | prot::WRITE,
        "[stack]",
    );

    // Finally the initial register state: sp, and x0 cleared the way Linux
    // leaves it.
    let data = memory.data_mut(&mut *store);
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
                .ok_or_else(|| anyhow!("guest memory unavailable"))?;
            // Split the borrow: the kernel needs `&mut [u8]` and `&mut Kernel`
            // at once, and they live in different places.
            // /proc must reflect the state this syscall will see, so the
            // snapshot is refreshed before the call rather than after.
            {
                let state = caller.data();
                state.kernel.refresh_proc(&state.proc_state);
            }
            let (data, state) = memory.data_and_store_mut(&mut caller);
            let mut gm = GuestMem::new(data);
            match state.kernel.syscall(&mut gm, pc as u64) {
                SyscallOutcome::Continue => Ok(()),
                SyscallOutcome::Resume(target) => {
                    state.resume_at = Some(target);
                    Err(StopExecution.into())
                }
                SyscallOutcome::Exit(code) => {
                    state.exit = Some(Exit::Status(code));
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
                    .ok_or_else(|| anyhow!("guest memory unavailable"))?;
                let (data, state) = memory.data_and_store_mut(&mut caller);
                let mut gm = GuestMem::new(data);
                return match state.kernel.sigreturn(&mut gm) {
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
                .ok_or_else(|| anyhow!("guest memory unavailable"))?;
            let (data, state) = memory.data_and_store_mut(&mut caller);
            let mut gm = GuestMem::new(data);
            // A guest that installed a SIGILL handler gets to see it, which
            // is how some runtimes probe for optional instructions.
            match state
                .kernel
                .raise_fault(&mut gm, signal::SIGILL as u32, pc, pc)
            {
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
        |mut caller: Caller<'_, HostState>, _ctx: i32, _pc: i64, op: i32, addr: i64| -> Result<()> {
            if op == gex_abi::cache_op::DC_ZVA {
                let memory = caller
                    .data()
                    .memory
                    .ok_or_else(|| anyhow!("guest memory unavailable"))?;
                let data = memory.data_mut(&mut caller);
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
                .ok_or_else(|| anyhow!("guest memory unavailable"))?;
            let (data, state) = memory.data_and_store_mut(&mut caller);
            let mut gm = GuestMem::new(data);
            // The context is complete here, so a handler frame built now
            // captures exactly the state the guest would see on hardware.
            let ctx_addr = state.kernel.ctx_addr;
            let _ = gm.write_u64(ctx_addr + gex_abi::ctx::PC as u64, pc as u64);
            // Rearm before delivering, so a handler that loops still polls.
            let counter = ctx_addr as usize + gex_abi::ctx::SAFEPOINT_COUNTER;
            if counter + 4 <= gm.bytes.len() {
                gm.bytes[counter..counter + 4]
                    .copy_from_slice(&SAFEPOINT_INTERVAL.to_le_bytes());
            }
            match state.kernel.deliver_pending(&mut gm) {
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
    let (limits, page_table) = {
        let jit = caller
            .data()
            .jit
            .as_ref()
            .ok_or_else(|| anyhow!("runtime translation is not set up"))?;
        (jit.limits, jit.page_table.clone())
    };
    if pc % 4 != 0 || pc >= gex_abi::DISPATCH_MAX_PAGES * gex_abi::DISPATCH_PAGE {
        return Ok(false);
    }
    // Only translate what the guest says is executable, so a wild branch into
    // data reports SIGSEGV rather than running whatever bytes are there.
    if !caller.data().kernel.mem.check_access(pc, 4, prot::EXEC) {
        return Ok(false);
    }

    let memory = caller
        .data()
        .memory
        .ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let known: std::collections::HashSet<u64> = caller
        .data()
        .jit
        .as_ref()
        .map(|j| j.translated.clone())
        .unwrap_or_default();
    let exec_ranges: Vec<(u64, u64)> = caller
        .data()
        .kernel
        .mem
        .vmas()
        .iter()
        .filter(|v| v.prot & prot::EXEC != 0)
        .map(|v| (v.start, v.end))
        .collect();
    let (translated_module, stats) = {
        let data = memory.data(&mut *caller);
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

    // The new module imports the memory and the table by name.
    let mut linker = linker;
    linker.define(&mut *caller, gex_abi::IMPORT_MODULE, exports::MEMORY, memory)?;
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
    write_dispatch_dir(caller, memory, &translated_module.dir_entries)?;

    // If the address that faulted did not end up with code, resuming would
    // fault again at the same place forever. Say so instead.
    if !installed.contains(&pc) {
        bail!("translated {} blocks but none covered 0x{pc:x}", installed.len());
    }

    let jit = caller.data_mut().jit.as_mut().unwrap();
    jit.translated.extend(installed);
    jit.page_table = translated_module.page_table.clone();
    jit.modules.push(instance);
    jit.blocks += stats.blocks;
    jit.calls += 1;
    Ok(true)
}

/// Publish page-directory entries so translated code becomes reachable.
fn write_dispatch_dir(
    caller: &mut Caller<'_, HostState>,
    memory: Memory,
    entries: &[(u64, u32)],
) -> Result<()> {
    let data = memory.data_mut(&mut *caller);
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
    let pc = store
        .data()
        .memory
        .map(|_| 0u64)
        .unwrap_or(0);
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
