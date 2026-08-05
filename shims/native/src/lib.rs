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
use gex_ld::{build_initial_stack, Identity, Image};
use gex_xlate::{Options, Stats};
use wasmtime::{Caller, Config, Engine, Extern, Linker, Memory, Module, Store, Val};

/// How a guest run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// `exit_group(status)`.
    Status(i32),
    /// A fatal signal the runtime synthesised.
    Signal { signal: i32, pc: u64, detail: String },
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
}

/// A trap used to unwind out of translated code when the run is over.
///
/// Exiting by trapping keeps the common syscall path free of a return-value
/// check: only the rare terminating call pays anything.
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
    let (module_bytes, stats) = gex_xlate::translate_image(image, xlate);
    if opts.verbose {
        eprintln!(
            "[gex] translated {} blocks / {} instructions -> {} KiB wasm ({} dispatch slots, \
             {} entry points from data, {} untranslatable blocks)",
            stats.blocks,
            stats.instructions,
            stats.wasm_bytes / 1024,
            module_bytes.text_slots,
            stats.discovered_from_data,
            stats.illegal_blocks,
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

    place_image(&mut store, memory, image, opts, Identity { uid, gid })?;

    let enter = instance
        .get_func(&mut store, exports::ENTER)
        .ok_or_else(|| anyhow!("translated module does not export `enter`"))?;

    let result = enter.call(
        &mut store,
        &[Val::I32(ctx_addr as i32), Val::I64(image.entry as i64)],
        &mut [],
    );

    let exit = match result {
        Ok(()) => store
            .data()
            .exit
            .clone()
            // Falling off the end of translated code means the guest returned
            // from its entry point, which Linux treats as an exit.
            .unwrap_or(Exit::Status(0)),
        Err(e) => match store.data().exit.clone() {
            Some(x) => x,
            None => return Err(classify_trap(e, &store)),
        },
    };
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
    image: &Image,
    opts: &RunOptions,
    id: Identity,
) -> Result<()> {
    // Segments first.
    for seg in &image.segments {
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
                SyscallOutcome::Exit(code) => {
                    state.exit = Some(Exit::Status(code));
                    Err(StopExecution.into())
                }
            }
        },
    )?;

    linker.func_wrap(
        m,
        imports::DISPATCH_MISS,
        |mut caller: Caller<'_, HostState>, _ctx: i32, pc: i64| -> Result<i32> {
            // From M3 this is where gex-jit translates the target and patches
            // the dispatch slot. Until then an unreachable address is a hard
            // error, reported precisely rather than as a wasm trap.
            let pc = pc as u64;
            caller.data_mut().exit = Some(Exit::Signal {
                signal: signal::SIGSEGV,
                pc,
                detail: format!(
                    "branch to 0x{pc:x}, which has no translation \
                     (runtime translation lands in M3)"
                ),
            });
            Err(StopExecution.into())
        },
    )?;

    linker.func_wrap(
        m,
        imports::ILLEGAL,
        |mut caller: Caller<'_, HostState>, _ctx: i32, pc: i64, raw: i32| -> Result<()> {
            let pc = pc as u64;
            let raw = raw as u32;
            caller.data_mut().exit = Some(Exit::Signal {
                signal: signal::SIGILL,
                pc,
                detail: format!("instruction 0x{raw:08x} at 0x{pc:x} is not implemented"),
            });
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
        |_caller: Caller<'_, HostState>, _ctx: i32, _pc: i64| {},
    )?;

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
