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
pub mod aot_only;
pub mod libprov;
pub(crate) mod elfprov;
pub(crate) mod wasmjit;
pub mod aot_share;
pub mod gl;
pub mod profile;
pub mod nt;
pub mod nt_cgen;
pub mod wasmprog;
pub mod wasmdll;
mod ready;
pub(crate) mod ftab;
pub mod wayland;

use wasmtime::{Caller, Config, Engine, Extern, Linker, Module, Ref, Store, Table, Val};

/// A host function's result for wasmtime 49 (its own error type): the
/// anyhow error inside, which `downcast_ref` still finds (`StopExecution`).
pub(crate) fn host<T>(r: anyhow::Result<T>) -> wasmtime::Result<T> {
    r.map_err(wasmtime::Error::from_anyhow)
}

/// `anyhow::Context` for wasmtime's own error type (wasmtime 49's
/// `wasmtime::Error` is not a `std::error::Error`, so anyhow's trait does
/// not apply to it): the same `context`/`with_context`, giving an
/// `anyhow::Error`.
pub(crate) trait WasmtimeContext<T> {
    fn context<C: std::fmt::Display + Send + Sync + 'static>(self, c: C) -> anyhow::Result<T>;
    fn with_context<C: std::fmt::Display + Send + Sync + 'static, F: FnOnce() -> C>(self, f: F) -> anyhow::Result<T>;
}

impl<T> WasmtimeContext<T> for std::result::Result<T, wasmtime::Error> {
    fn context<C: std::fmt::Display + Send + Sync + 'static>(self, c: C) -> anyhow::Result<T> {
        self.map_err(|e| anyhow::Error::from(e).context(c))
    }
    fn with_context<C: std::fmt::Display + Send + Sync + 'static, F: FnOnce() -> C>(self, f: F) -> anyhow::Result<T> {
        self.map_err(|e| anyhow::Error::from(e).context(f()))
    }
}

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
    /// A native wasm32 busybox (docs/wine-native.md §5.1) presented as
    /// `/bin/sh` and the common coreutils in `/bin` and `/usr/bin`, so a
    /// script's shell and tools run natively instead of being translated.
    pub busybox: Option<std::path::PathBuf>,
    /// The program file when argv[0] does not name it (`--busybox` running
    /// `/bin/sh`): what `/proc/self/exe` is, which hush re-executes.
    pub exe: Option<String>,
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
    /// The guest memory (docs/mem64.md): wasm32 unless this process needs
    /// more address space.
    pub mem: gex_abi::MemFlavor,
    /// Run ahead-of-time code only (`aot_only`): an ejected Windows
    /// program's runtime, with no tier-1 module and no run-time translator.
    pub aot_only: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            mem: gex_abi::MemFlavor::M32,
            args: Vec::new(),
            env: Vec::new(),
            trace: false,
            verbose: false,
            sysroot: None,
            busybox: None,
            exe: None,
            identity: None,
            sample: false,
            pins: std::collections::HashMap::new(),
            report_pins: false,
            wayland: None,
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
            aot_only: false,
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
    /// Set with `exit` when only this thread ends (`exit` with others left),
    /// not the process.
    pub(crate) thread_only: bool,
    /// Set when a host function wants execution to continue elsewhere.
    resume_at: Option<u64>,
    /// Why `resume_at` was set (GEX_NT_STATS only; `note_resume`).
    pub(crate) resume_cause: Option<String>,
    /// The function tier's call-outs in progress on this thread, innermost
    /// last (`call_out`).
    callouts: Vec<Callout>,
    /// The `enter` of the module this thread's loop started in, for nested
    /// loops entered from code that has none (a function-tier unit or a
    /// function JIT module) when no runtime-translated module exists yet.
    root_enter: Option<wasmtime::Func>,
    /// This store's NT thread, once a syscall has looked it up.
    nt_thread: Option<Arc<gex_kernel::nt::Thread>>,
    /// What the watch stub decided, for `call_out`'s loop.
    pub(crate) callout_signal: Option<CalloutSignal>,
    /// Exceptions being dispatched nested (`nt::raise_nested`), innermost
    /// last: the stack pointer at each `NtRaiseException`. Control that
    /// reaches the host with the stack above one has left that dispatch.
    pub(crate) exc_levels: Vec<u64>,
    /// The host function watched slots hold (`watch_stub`).
    watch_stub: Option<wasmtime::Func>,
    /// Where the guest is spending its time, when anyone asked.
    sampler: Option<Arc<Sampler>>,
    /// The compositor, when the guest was given one.
    wayland: Option<wayland::Wayland>,
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
    /// How the process ended when a thread other than the first ended it
    /// (`exit_group`, a fatal signal, a gex failure): the first thread only
    /// sees `stopping`, and reports this instead of its own way out.
    ended: Arc<Mutex<Option<Exit>>>,
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
    jit_shared: Option<Arc<Mutex<JitShared>>>,
    limits: gex_xlate::BlockLimits,
    /// Where this process's objects are, for naming profile counts.
    prof_proc: Option<Arc<profile::ProcProf>>,
    /// The NT process, for a Windows program's threads. See `nt`.
    nt: Option<Arc<gex_kernel::nt::Process>>,
}

/// Tier-1 counters for `GEX_NT_STATS`: translations published, units
/// installed into stores, pages retired, the slot high-water mark, and the
/// dispatch-table entries grown across every store.
pub(crate) mod jit_stats {
    use std::sync::atomic::AtomicU64;
    pub static PUBLISHED: AtomicU64 = AtomicU64::new(0);
    pub static BLOCKS: AtomicU64 = AtomicU64::new(0);
    pub static INSTALLS: AtomicU64 = AtomicU64::new(0);
    pub static RETIRED: AtomicU64 = AtomicU64::new(0);
    pub static SLOTS: AtomicU64 = AtomicU64::new(0);
    pub static GROWN: AtomicU64 = AtomicU64::new(0);
    pub static WASM_BYTES: AtomicU64 = AtomicU64::new(0);
    /// Tier-1 miss translations: compiled-module cache hits and misses, and
    /// microseconds spent compiling (and loading) them.
    pub static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
    pub static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);
    pub static COMPILE_US: AtomicU64 = AtomicU64::new(0);
    /// Blocks that failed their page check and were translated again.
    pub static STALE: AtomicU64 = AtomicU64::new(0);
    /// The last few translated pcs, and since the last line the lowest and
    /// highest, so a translation storm shows where it is without a print per
    /// miss (which changes the timing enough to hide it).
    pub static LAST: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
    pub static LO: AtomicU64 = AtomicU64::new(u64::MAX);
    pub static HI: AtomicU64 = AtomicU64::new(0);
    pub fn note(pc: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let n = PUBLISHED.load(Relaxed) as usize;
        LAST[n % 4].store(pc, Relaxed);
        LO.fetch_min(pc, Relaxed);
        HI.fetch_max(pc, Relaxed);
    }
    pub fn line() -> String {
        use std::sync::atomic::Ordering::Relaxed;
        let (lo, hi) = (LO.swap(u64::MAX, Relaxed), HI.swap(0, Relaxed));
        format!(
            "jit pc={lo:#x}..{hi:#x} last={:x?} pub={} blocks={} wasm={}M inst={} retired={} stale={} slots={} grown={} t1cache={}/{} t1compile_ms={}",
            LAST.iter().map(|a| a.load(Relaxed)).collect::<Vec<_>>(),
            PUBLISHED.load(Relaxed),
            BLOCKS.load(Relaxed),
            WASM_BYTES.load(Relaxed) >> 20,
            INSTALLS.load(Relaxed),
            RETIRED.load(Relaxed),
            STALE.load(Relaxed),
            SLOTS.load(Relaxed),
            GROWN.load(Relaxed),
            CACHE_HITS.load(Relaxed),
            CACHE_HITS.load(Relaxed) + CACHE_MISSES.load(Relaxed),
            COMPILE_US.load(Relaxed) / 1000
        )
    }
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
    /// This store's function table (`ftab`), made at the first need.
    ftab: Option<ftab::Store>,
}

/// How long `JitShared::registry` is (stored after every push, under its
/// lock): `catch_up_in_caller`'s check without the lock. A hint only: one
/// process's in a forked child may be longer than the child's registry,
/// which costs that check its lock, as before.
static REGISTRY_LEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// One runtime-translated module, kept so another thread's store can
/// instantiate it rather than translating the same code again.
struct CompiledUnit {
    /// None for a unit that only writes dispatch slots (`bridges`).
    module: Option<Module>,
    exported_blocks: Vec<(u64, String)>,
    dir_entries: Vec<(u64, u32)>,
    table_slots: u64,
    /// Table-0 slots to point at the function table's bridge (`ftab`):
    /// addresses a function body serves.
    bridges: Vec<u32>,
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
    /// Misses at an address already translated, with the last few miss
    /// sources: past [`REMISS_LIMIT`] the run stops (see `remiss`).
    remiss: std::collections::HashMap<u64, (u32, [u64; 4])>,
    /// How often each page has been retired, for that diagnostic.
    retired_pages: std::collections::HashMap<u64, u32>,
    /// Blocks a publish did not count as translated because their page was
    /// retired after the translation's snapshot (their slots are abandoned).
    stale_blocks: usize,
    /// Which translation (`TranslatedModule::module_id`) provides each
    /// translated address: a block failing its page check is translated
    /// again only while its own module still does (`PAGE_STALE`).
    provider: std::collections::HashMap<u64, u64>,
    /// The function table's process half (`ftab`).
    ftab: ftab::Shared,
    /// A native wasm32 program (`wasmprog`): its native frames cannot be
    /// unwound and resumed, so a fork is made without leaving them.
    wasm_program: bool,
    /// Wine's PE side as one wasm module (`wasmdll`, shared mode): installed
    /// at the first skeleton that names it, bound DLL by DLL.
    wine_pe: Option<Arc<wasmdll::WinePe>>,
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
/// The `x87` import's work: `gex_ir::X87Op::code` decoded onto
/// `gex_f80`'s pure operations. Returns (the first result, the sign and
/// exponent of an extended result, the status-word bits).
pub fn x87_op(op: i32, a_sig: u64, a_se: u16, b_sig: u64, b_se: u16, cw: u16) -> (u64, u32, u32) {
    use gex_kernel::f80::{self, F80};
    let (a, b) = (F80 { sig: a_sig, se: a_se }, F80 { sig: b_sig, se: b_se });
    match op {
        1 | 2 => {
            let (r, sw) = f80::narrow_op(a_sig, a_se, cw, op == 1);
            (r, 0, sw)
        }
        _ if op & 0xf00 == 0x100 => {
            let (r, sw) = f80::to_int_op(a_sig, a_se, cw, ((op >> 1) & 0x7f) as u8, op & 1 != 0);
            (r, 0, sw)
        }
        _ if op & 0xf00 == 0x200 => {
            let (r, sw) = f80::arith_op((op & 3) as u8, a, b, cw);
            (r.sig, r.se as u32, sw)
        }
        _ if op & 0xf00 == 0x300 => {
            let (cc, sw) = f80::compare_op(a, b, op & 1 != 0);
            (cc as u64, 0, sw)
        }
        _ => panic!("x87 import: unknown operation {op:#x}"),
    }
}

/// `GEX_PPM_EVERY=N`: `--wayland-out` writes every Nth frame only, each
/// named by its real index (so `GEX_NT_INPUT`'s frame numbers still line up
/// and the newest name is the frame count). A long run otherwise writes a
/// frame at every present, 6 MB each at 1920x1080.
pub(crate) fn ppm_every() -> u32 {
    static EVERY: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *EVERY.get_or_init(|| std::env::var("GEX_PPM_EVERY").ok().and_then(|v| v.parse().ok()).filter(|&n| n > 0).unwrap_or(1))
}

/// The guest memory's type: 4 GiB of wasm32, or for a 64-bit guest memory
/// (docs/mem64.md) all of `LIMIT64` at once, which natively is only a
/// reservation, so the high arena never needs the memory to grow.
pub fn guest_memory_type() -> wasmtime::MemoryType {
    if gex_abi::mem64() {
        wasmtime::MemoryTypeBuilder::new()
            .memory64(true)
            .shared(true)
            .min(gex_abi::LIMIT64_PAGES)
            .max(Some(gex_abi::LIMIT64_PAGES))
            .build()
            .expect("a memory64 type")
    } else {
        wasmtime::MemoryType::shared(GUEST_MEM_PAGES as u32, GUEST_MEM_PAGES as u32)
    }
}

pub fn validate_module(wasm: &[u8]) -> Result<()> {
    let mut config = Config::new();
    config.wasm_tail_call(true);
    config.wasm_threads(true);
    // wasmtime 49 creates no SharedMemory unless asked: the guest's memory is one.
    config.shared_memory(true);
    // Wasm exception handling: setjmp/longjmp in native Wine and wasm32
    // programs (clang's SjLj lowering), docs/wine-native.md §4.2.
    config.wasm_exceptions(true);
    config.wasm_bulk_memory(true);
    // The profile counters are a second memory (`profile`).
    config.wasm_multi_memory(true);
    // A 64-bit guest memory (docs/mem64.md).
    if gex_abi::mem64() {
        config.wasm_memory64(true);
    }
    let engine = Engine::new(&config).context("creating the wasm engine")?;
    Module::validate(&engine, wasm).map_err(anyhow::Error::from)
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
}

/// The wasm engine every guest process runs on. Code compiled ahead of time
/// for a run (an ejected project's `.cwasm`) must be compiled by an engine
/// configured exactly so.
pub fn engine() -> Result<Engine> {
    let mut config = Config::new();
    // Deep guest call chains are wasm recursion (units call units): room for
    // them, on threads with stacks to match (`guest_thread_stack`).
    config.max_wasm_stack(wasm_stack_bytes());
    // wasmtime 49 checks it against the async stack, used or not.
    config.async_stack_size(wasm_stack_bytes() + (1 << 20));
    // Traps are control flow here (a call-out's return, a resume, a callback's
    // end, all StopExecution), hundreds a second in Noita, and each walked
    // and symbolised the whole wasm stack for a backtrace nothing reads
    // (`classify_trap` reports the trap's kind). GEX_WASM_BACKTRACE=1 keeps
    // them, for debugging.
    config.wasm_backtrace(std::env::var("GEX_WASM_BACKTRACE").as_deref() == Ok("1"));
    // Tail calls are load-bearing: the whole block-per-function design in
    // PLAN §2.4 depends on them.
    config.wasm_tail_call(true);
    config.wasm_threads(true);
    // wasmtime 49 creates no SharedMemory unless asked: the guest's memory is one.
    config.shared_memory(true);
    // Wasm exception handling: setjmp/longjmp in native Wine and wasm32
    // programs (clang's SjLj lowering), docs/wine-native.md §4.2.
    config.wasm_exceptions(true);
    config.wasm_bulk_memory(true);
    // The profile counters are a second memory (`profile`).
    config.wasm_multi_memory(true);
    // A 64-bit guest memory (docs/mem64.md): a shared memory is a static
    // allocation, so the reservation has to hold all of it. Only then: a
    // wasm32 process keeps the engine it always had.
    if gex_abi::mem64() {
        config.wasm_memory64(true);
        config.memory_reservation(gex_abi::LIMIT64);
    }
    // `GEX_NO_SIGNAL_TRAPS=1` (debugging): explicit bounds checks, so a wild address in generated code traps
    // with a backtrace instead of faulting outside every guard region (which the signal handler cannot
    // attribute and the process dies of).
    if std::env::var_os("GEX_NO_SIGNAL_TRAPS").is_some() {
        config.signals_based_traps(false);
    }
    // gex's output is already optimised; Cranelift only adds register
    // allocation and peepholes, whose time dominates on a large program (half
    // a minute to compile SQLite vs under two seconds to translate).
    // `GEX_OPT_LEVEL` picks the trade.
    #[cfg(feature = "compile")]
    config.cranelift_opt_level(match std::env::var("GEX_OPT_LEVEL").as_deref() {
        Ok("none") => wasmtime::OptLevel::None,
        Ok("speed_and_size") => wasmtime::OptLevel::SpeedAndSize,
        _ => wasmtime::OptLevel::Speed,
    });
    // `GEX_PERFMAP=1`: name every compiled function in /tmp/perf-<pid>.map,
    // so a host profiler (perf, samply) attributes JIT and unit code.
    if std::env::var_os("GEX_PERFMAP").is_some() {
        config.profiler(wasmtime::ProfilingStrategy::PerfMap);
    }
    Engine::new(&config).context("creating the wasm engine")
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
    //
    // A native wasm32 program has no guest code to decode and leaves these
    // alone: it is an i386 ELF (and its kernel speaks the i386 ABI, below),
    // but an x86-64 shell that runs it goes on translating its own code
    // after the wait.
    if !wasmprog::is_wasm32(program) || std::env::var_os("GEX_W3_COMPAT").is_some() {
        gex_xlate::set_compat(program.compat());
        lockstep::set_compat(program.compat());
        gex_kernel::set_compat32(program.compat() && gex_abi::X86);
    }
    ftab::selftest_init();

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
    let engine = engine()?;

    // Only the executable is translated ahead of time; the interpreter and its
    // libraries arrive through runtime translation, because at build time we do
    // not know where they land. Translation is inside the cache miss path, so a
    // hit skips it and reports empty stats apart from the sidecar block count.
    let mut stats = Stats::default();
    let translate = || {
        // Without the translator (gex-rt) only an ejected project's
        // AOT-only process gets here, and it never calls this.
        #[cfg(not(feature = "translate"))]
        panic!("{} has no translator to translate the program image", env!("CARGO_PKG_NAME"));
        #[allow(unreachable_code)]
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

    if opts.aot_only {
        aot_only::enable();
    }
    let (module, meta) = if opts.aot_only {
        // An ejected Windows program: no tier-1 module, only the root's
        // entry (`aot_only`).
        (gex_cache::compile(&engine, &aot_only::base_module()).context("compiling the AOT-only base module")?, Default::default())
    } else if let Some(aot) = &opts.aot {
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
                gex_cache::compile(&engine, &m.wasm).context("validating translated wasm")?,
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
    JIT_ON.store(opts.jit && !opts.aot_only, std::sync::atomic::Ordering::Relaxed);
    std::sync::LazyLock::force(&DUMP_START);
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
        .exe
        .as_ref()
        .or(opts.args.first())
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
    let kept_wayland = inherited.as_ref().and_then(|i| i.wayland.clone());
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
    // Set again by `wasmprog` if this program is native wasm32.
    kernel.wasm32 = false;
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
            // A GL client's frames are on the host's surface, not in a
            // buffer the compositor sees: the device hands them over.
            if let Some(dev) = crate::gl::DEVICE.get() {
                let w = w.clone();
                dev.set_frame_sink(Box::new(move |f| w.present_external(f.width, f.height, &f.rgba)));
            }
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
            thread_only: false,
            resume_at: None,
            resume_cause: None,
            callouts: Vec::new(),
            root_enter: None,
            nt_thread: None,
            callout_signal: None,
            exc_levels: Vec::new(),
            watch_stub: None,
            sampler: sampler.clone(),
            wayland: compositor.clone(),
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
    if opts.aot_only {
        aot_only::override_imports(&mut linker)?;
    }
    // Kept before any store-bound definition is added, so a thread can clone
    // it and define its own memory and table.
    let base_linker = linker.clone();

    // The guest's memory belongs to the host, not to any module: every guest
    // thread runs in its own store, and a module-defined memory would give
    // each of them a private copy of the address space.
    let shared = wasmtime::SharedMemory::new(
        &engine,
        guest_memory_type(),
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
    if let Some(dev) = crate::gl::DEVICE.get() {
        dev.set_memory(memory.clone());
    }

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
                remiss: Default::default(),
                retired_pages: Default::default(),
                stale_blocks: 0,
                provider: Default::default(),
                ftab: Default::default(),
                wasm_program: false,
                wine_pe: None,
            })),
            engine: engine.clone(),
            linker: linker.clone(),
            tables,
            cache: jit_cache.clone(),
            instances: Vec::new(),
            replayed: 0,
            limits: xlate.limits,
            ftab: None,
        });
    }
    if !aot_shared.is_empty() {
        aot::install_shared(&mut store, aot_shared)?;
    }

    let entry = match nt_setup {
        Some(setup) => {
            let pc = nt::boot(&mut store, memory.clone(), setup)?;
            nt_cgen::drain_in_store(&mut store)?;
            pc
        }
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

    // A native wasm32 program (stream W): its module, not its x86 bytes,
    // runs at the entry. After the directory above, which its bridges
    // overwrite.
    if nt_setup.is_none() {
        wasmprog::install(&mut store, &memory, program)?;
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
        stopping: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        running: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        ended: Arc::new(Mutex::new(None)),
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

    if opts.aot_only {
        aot_only::bind_root(&mut store, &instance)?;
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
        );
    }
    let Stopped::Exited(exit) = stopped else {
        unreachable!()
    };
    // Another thread may have ended the process (this one then only saw
    // `stopping`).
    let exit = spawn.ended.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or(exit);

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
        s.append_out(&s.counts.lock().unwrap_or_else(|e| e.into_inner()));
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
    ///
    /// The turn ends early when a syscall elsewhere may have made a
    /// descriptor ready (`ready`); `$seen` is the sequence taken before this
    /// turn's check.
    macro_rules! turn {
        ($seen:expr) => {
            if interrupted() {
                return Err(Errno::EINTR);
            }
            ready::wait($seen, std::time::Duration::from_millis(1));
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
            // In slices, so a signal for this thread ends the wait (Linux
            // interrupts a futex wait to run a handler; glibc takes EINTR as
            // a spurious wake). Mono's GC suspends threads that sit in
            // condition-variable waits exactly this way.
            let mem = GuestMem::new(data);
            let start = std::time::Instant::now();
            let slice = std::time::Duration::from_millis(20);
            loop {
                let left = match deadline {
                    Some(d) if start.elapsed() >= d => return Err(Errno::ETIMEDOUT),
                    Some(d) => (d - start.elapsed()).min(slice),
                    None => slice,
                };
                match table.wait(uaddr, val, Some(left), &|| mem.read_u32(uaddr).ok()) {
                    Err(Errno::ETIMEDOUT) => {
                        if deadline.is_some_and(|d| start.elapsed() >= d) {
                            return Err(Errno::ETIMEDOUT);
                        }
                        if interrupted() {
                            return Err(Errno::EINTR);
                        }
                    }
                    r => return r.map(|_| 0),
                }
            }
        }
        BlockOp::FutexLockPi { table, uaddr, tid, deadline } => {
            use gex_kernel::futex::pi;
            let start = std::time::Instant::now();
            loop {
                let mem = GuestMem::new(&mut *data);
                let v = gex_atomic::load_u32(mem.bytes, uaddr).ok_or(Errno::EFAULT)?;
                match v & pi::TID_MASK {
                    // Free: take it. The waiters bit stays (set) because
                    // others may still sleep on it, so the next unlock goes
                    // through the kernel and wakes one.
                    0 => {
                        if gex_atomic::cas_u32(mem.bytes, uaddr, v, tid | pi::WAITERS).ok_or(Errno::EFAULT)?.is_ok() {
                            return Ok(0);
                        }
                        continue;
                    }
                    owner if owner == tid => return Err(Errno::EDEADLK),
                    _ => {}
                }
                let want = v | pi::WAITERS;
                if v != want && gex_atomic::cas_u32(mem.bytes, uaddr, v, want).ok_or(Errno::EFAULT)?.is_err() {
                    continue;
                }
                let left = match deadline {
                    Some(d) if start.elapsed() >= d => return Err(Errno::ETIMEDOUT),
                    Some(d) => d - start.elapsed(),
                    None => std::time::Duration::from_millis(100),
                };
                // In slices: the owner's UNLOCK_PI wakes this, and the word
                // is re-read either way.
                let _ = table.wait(uaddr, want, Some(left.min(std::time::Duration::from_millis(100))), &|| {
                    mem.read_u32(uaddr).ok()
                });
            }
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
            let seen = ready::seq();
            match socket.status() {
                gex_kernel::net::Status::Connecting => {
                    turn!(seen);
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
                let seen = ready::seq();
                service(data);
                if gex_kernel::syscalls::poll_ready(&fds) {
                    let mut mem = GuestMem::new(data);
                    return gex_kernel::syscalls::poll_finish(&fds, &report, &mut mem);
                }
                if deadline.is_some_and(|d| start.elapsed() >= d) {
                    let mut mem = GuestMem::new(data);
                    return gex_kernel::syscalls::poll_finish(&fds, &report, &mut mem);
                }
                turn!(seen);
            }
        }
        BlockOp::WriteStream { handle, buf, len } => {
            let data = GuestMem::new(data).read(buf, len)?.to_vec();
            let mut done = 0usize;
            while done < data.len() {
                let seen = ready::seq();
                if !handle.poll_writable() {
                    // A partial write already delivered is the caller's
                    // answer; only an untouched write reports the signal.
                    if interrupted() {
                        return if done > 0 { Ok(done as u64) } else { Err(Errno::EINTR) };
                    }
                    ready::wait(seen, std::time::Duration::from_millis(1));
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
                let seen = ready::seq();
                service(data);
                if handle.poll_readable() {
                    let n = handle.read_at(0, &mut tmp)?;
                    let mut mem = GuestMem::new(data);
                    mem.write(buf, &tmp[..n as usize])?;
                    return Ok(n);
                }
                turn!(seen);
            }
        }
        BlockOp::SigSuspend { .. } => loop {
            // Nothing to poll but the signal state, which `interrupted` is.
            let seen = ready::seq();
            service(data);
            if interrupted() {
                return Err(Errno::EINTR);
            }
            ready::wait(seen, std::time::Duration::from_millis(1));
        },
        BlockOp::AcceptWait { socket } => {
            // Only the wait: the descriptor is made by the kernel afterwards,
            // which is the one place that has a descriptor table.
            loop {
                let seen = ready::seq();
                service(data);
                if socket.poll_readable() {
                    return Ok(0);
                }
                turn!(seen);
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
    store.data_mut().root_enter = Some(enter.clone());
    let mut pc = entry;
    loop {
        let result = enter
            .call(&mut *store, &[Val::I32(ctx_addr as i32), Val::I64(pc as i64)], &mut [])
            .map_err(anyhow::Error::from);
        if let Some(req) = store.data_mut().pending_fork.take() {
            fork_process(&*store, req)?;
        }
        if let Some(child) = store.data_mut().pending_spawn.take() {
            let cx = store
                .data()
                .spawn
                .clone()
                .ok_or_else(|| anyhow!("clone with no spawn context"))?;
            start_guest_thread(cx, child)?;
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
                    crate::ftab::stats::RESUMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    note_resume(store.data_mut(), next, "run loop");
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
                // A wild pointer in a Linux guest with no SIGSEGV handler:
                // the process dies of SIGSEGV, as it would natively (the
                // translated code cannot say which instruction faulted, so a
                // handler, which would resume there, is not run; that case
                // stays a gex error below).
                if matches!(e.downcast_ref::<wasmtime::Trap>(), Some(wasmtime::Trap::MemoryOutOfBounds))
                    && store.data().nt.is_none()
                {
                    let action = store.data_mut().with_kernel(|k| k.signals.action(signal::SIGSEGV as u32));
                    // GEX_TRACE_SEGV=1: where (with GEX_WASM_BACKTRACE=1, the wasm frames).
                    if std::env::var_os("GEX_TRACE_SEGV").is_some() {
                        eprintln!("gex: guest memory access out of bounds: {e:?}");
                        // The context's PC is the last one stored: the faulting block's entry, or near it.
                        if let Some(memory) = store.data().memory.clone() {
                            let at = read_u64(memory.bytes(), (ctx_addr + ctx::PC as u64) as usize);
                            eprintln!("gex: thread {} context pc {at:#x}", store.data().thread.tid);
                            let maps = store.data_mut().with_kernel(|k| k.mem.proc_maps());
                            for l in maps.lines() {
                                let mut r = l.split(|c| c == '-' || c == ' ');
                                let (Some(a), Some(b)) = (r.next(), r.next()) else { continue };
                                let (Ok(a), Ok(b)) = (u64::from_str_radix(a, 16), u64::from_str_radix(b, 16)) else { continue };
                                if (a..b).contains(&at) {
                                    eprintln!("gex: in {l}");
                                }
                            }
                        }
                    }
                    if action.handler == 0 {
                        return Ok(Stopped::Exited(Exit::Signal {
                            signal: signal::SIGSEGV,
                            pc: 0,
                            detail: "guest memory access out of bounds".into(),
                            internal: false,
                        }));
                    }
                }
                return Err(classify_trap(e, store));
            }
        }
    }
}

/// Start a host thread for a guest thread a `clone` (or an NT thread
/// creation) asked for. Called between guest calls, where nothing is borrowed.
fn start_guest_thread(cx: Arc<SpawnContext>, child: ThreadState) -> Result<()> {
    // Detached: a guest thread is joined through its own futex, not
    // through the host's handle, and `exit_group` ends the process
    // whatever its threads are doing.
    cx.running.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    let running = Arc::clone(&cx.running);
    std::thread::Builder::new()
        .name(format!("gex-tid-{}", child.tid))
        .stack_size(guest_thread_stack())
        .spawn(move || {
            match run_guest_thread(Arc::clone(&cx), child) {
                Err(e) => {
                    eprintln!("gexc: thread {} failed: {e:#}", child.tid);
                    // A thread that fails takes its process with it, as on
                    // Linux: otherwise the other threads sit in a futex
                    // forever and every descriptor it holds stays open.
                    end_process(&cx, Exit::Signal {
                        signal: signal::SIGSEGV,
                        pc: 0,
                        detail: format!("thread {} failed: {e:#}", child.tid),
                        internal: true,
                    });
                    cx.kernel
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .fds
                        .close_all();
                }
                // `exit_group` or a fatal signal on this thread: the process
                // ends with it, and the first thread, parked in a futex
                // (`pthread_join`) or running, must hear how.
                Ok(Some(exit)) => end_process(&cx, exit),
                Ok(None) => {}
            }
            running.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        })
        .context("spawning a guest thread")?;
    Ok(())
}

/// Build and start a child process.
///
/// A forked child is the same program at the same addresses, so it reuses
/// every module the parent translated. What it cannot share is the memory: it
/// gets a fresh one with the parent's mapped regions copied in — one address
/// space per process, no copy-on-write, since wasm cannot fault on a write.
fn fork_process(store: &impl wasmtime::AsContext<Data = HostState>, req: ForkRequest) -> Result<()> {
    let pid = req.pid;
    if let Some(aot) = store.as_context().data().spawn.as_ref().and_then(|s| s.opts.aot.clone()) {
        if aot.single_process {
            bail!(
                "gex-eject: the program forked, and the session {} was ejected from never did: \
                 its runtime is single-process. Record a session that forks",
                aot.label
            );
        }
    }
    let cx = store
        .as_context()
        .data()
        .spawn
        .clone()
        .ok_or_else(|| anyhow!("fork with no spawn context"))?;
    let parent_mem = store
        .as_context()
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
        (child, regions, store.as_context().data().thread.ctx_addr)
    };

    let shared = wasmtime::SharedMemory::new(
        &cx.engine,
        guest_memory_type(),
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
        let _ = gex_kernel::syscalls::set_result(&mut gm, ctx_addr, 0);
        if req.child_stack != 0 {
            let _ = gm.write_u64(ctx_addr + ctx::SP as u64, req.child_stack);
        }
        if let Some(tls) = req.tls {
            // PowerPC's thread pointer is r2.
            let slot = if gex_abi::PPC { ctx::x(2) } else { ctx::TPIDR };
            let _ = gm.write_u64(ctx_addr + slot as u64, tls);
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
        stopping: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        running: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        ended: Arc::new(Mutex::new(None)),
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
        .stack_size(guest_thread_stack())
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
            // Its descriptors close before its parent can see it gone, as on
            // Linux: a listening socket's port is free once `wait4` returns.
            {
                let mut k = cleanup.kernel.lock().unwrap_or_else(|e| e.into_inner());
                k.fds.close_all();
                k.sockets.clear();
            }
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
            thread_only: false,
            resume_at: None,
            resume_cause: None,
            callouts: Vec::new(),
            root_enter: None,
            nt_thread: None,
            callout_signal: None,
            exc_levels: Vec::new(),
            watch_stub: None,
            sampler: cx.sampler.clone(),
            wayland: cx.wayland.clone(),
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
            ftab: None,
        });
    }
    catch_up_table(&mut store)?;
    let stopped = run_thread_loop(&mut store, &instance, thread.ctx_addr, entry)?;
    profile::harvest(&mut store)?;
    match stopped {
        Stopped::Exited(exit) => {
            stop_other_threads(&cx, false);
            Ok(cx.ended.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or(exit))
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
) -> Result<(Exit, Stats)> {
    // The file comes from the guest's namespace, not the host's, so a sysroot
    // is honoured and `/bin/sh` means the guest's.
    let bytes = kernel
        .read_file(&req.path)
        .ok_or_else(|| anyhow!("execve: cannot read {}", req.path))?;
    let program = gex_ld::load_program(
        &bytes,
        gex_abi::PIE_BASE,
        // Where `gexc run` and the browser put it: the kernel knows the
        // loader by that address (its reservations for code stay below 4 GiB
        // in a 64-bit memory).
        gex_abi::INTERP_BASE,
        &|p| kernel.read_file(p),
    )
    .with_context(|| format!("execve: loading {}", req.path))?;
    // What the translators knew of the old image is about addresses the new
    // one holds now.
    let mut xlate = xlate.clone();
    xlate.intrinsics = gex_xlate::intrinsic::program_intrinsics(&program);
    if let Some(t) = gex_kernel::ftab::translator() {
        t.exec(&program, &bytes);
    }
    // Descriptors marked close-on-exec go now; the rest survive, which is how
    // a shell hands a pipe to the command it starts.
    kernel.close_on_exec();
    // And the handlers, which are addresses in the image being replaced.
    kernel.signals.exec();
    // Every other thread is gone; their signal state with them.
    kernel.thread_signals.clear();
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
        &xlate,
        Some(Inherited {
            kernel,
            proc_state,
            wayland,
        }),
        None,
    )
}

/// A guest thread: its own store, its own instances, the same memory.
/// Run one spawned thread to its end. `Some` when its end is the process's
/// (`exit_group`, a fatal signal), not just its own.
fn run_guest_thread(cx: Arc<SpawnContext>, thread: ThreadState) -> Result<Option<Exit>> {
    let mut store = Store::new(
        &cx.engine,
        HostState {
            kernel: Arc::clone(&cx.kernel),
            thread,
            memory: Some(cx.memory.clone()),
            proc_state: Arc::clone(&cx.proc_state),
            exit: None,
            thread_only: false,
            resume_at: None,
            resume_cause: None,
            callouts: Vec::new(),
            root_enter: None,
            nt_thread: None,
            callout_signal: None,
            exc_levels: Vec::new(),
            watch_stub: None,
            sampler: cx.sampler.clone(),
            wayland: cx.wayland.clone(),
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
            ftab: None,
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
    if cx.opts.aot_only {
        aot_only::bind_root(&mut store, &instance)?;
    }
    let stopped = run_thread_loop(&mut store, &instance, thread.ctx_addr, entry)?;
    libprov::guest_stop().forget(thread.tid as u32);
    profile::harvest(&mut store)?;
    match stopped {
        Stopped::Exec(_) => bail!("execve from a non-main thread is not supported"),
        // Stopped because another thread ended the process: that one says how.
        Stopped::Exited(_) if cx.stopping.load(std::sync::atomic::Ordering::Acquire) => Ok(None),
        Stopped::Exited(_) if store.data().thread_only => Ok(None),
        Stopped::Exited(exit) => Ok(Some(exit)),
    }
}

/// A thread other than the first ended the process with `exit`: keep the
/// first such end and stop every thread.
fn end_process(cx: &SpawnContext, exit: Exit) {
    {
        let mut ended = cx.ended.lock().unwrap_or_else(|e| e.into_inner());
        if ended.is_none() && !cx.stopping.load(std::sync::atomic::Ordering::Acquire) {
            *ended = Some(exit);
        }
    }
    stop_other_threads(cx, false);
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

/// What `--busybox` presents (the applets tools/wine-wasm/busybox.config
/// builds; the list is tools/wine-wasm/busybox.applets, which the browser's
/// run-node.mjs reads too).
pub static BUSYBOX_APPLETS: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| include_str!("../../../tools/wine-wasm/busybox.applets").lines().filter(|l| !l.is_empty()).collect());

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
    if let Some(bb) = &opts.busybox {
        // Each name a file mount of the one binary: busybox picks its
        // applet by argv[0]. GEX_BUSYBOX_APPLETS replaces the list.
        let list = std::env::var("GEX_BUSYBOX_APPLETS").unwrap_or_else(|_| BUSYBOX_APPLETS.join(","));
        for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            for dir in ["/bin", "/usr/bin"] {
                kernel.vfs.mount(&format!("{dir}/{name}"), Arc::new(HostFs::read_only(bb)));
            }
        }
    }
    let mut devfs = DevFs::new();
    if opts.gl {
        // Named for what it is rather than as `/dev/dri/renderD128`: this is
        // not a DRM device and a program that probed it as one would be
        // misled. The guest's GL library looks for this name.
        let dev = crate::gl::GlDevice::new(1024, 1024);
        let _ = crate::gl::DEVICE.set(Arc::clone(&dev));
        devfs.add("gex-gl", dev);
    }
    kernel.vfs.mount("/dev", Arc::new(devfs));
    // POSIX shared memory (shm_open) lives in /dev/shm: a guest's own,
    // in memory (Chromium's, in a CEF game, refuses to start without it).
    kernel.vfs.mount("/dev/shm", Arc::new(TmpFs::new()));
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
    /// `GEX_SAMPLE_OUT=path`: append every sampled address and its count to
    /// `path`, one `pc count` line each (hex, decimal), so a run that never
    /// exits still leaves a full profile. Lines for the same address add up.
    pub fn append_out(&self, counts: &std::collections::HashMap<u64, u64>) {
        let Some(path) = std::env::var_os("GEX_SAMPLE_OUT") else { return };
        let mut text = String::new();
        for (pc, n) in counts {
            text.push_str(&format!("{pc:x} {n}\n"));
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = f.write_all(text.as_bytes());
        }
    }

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
static DUMP_AT: std::sync::LazyLock<Option<f64>> =
    std::sync::LazyLock::new(|| std::env::var("GEX_DUMP_AT").ok().and_then(|v| v.parse().ok()));
static DUMP_START: std::sync::LazyLock<std::time::Instant> = std::sync::LazyLock::new(std::time::Instant::now);
static DUMPED: std::sync::LazyLock<Mutex<std::collections::HashSet<i32>>> = std::sync::LazyLock::new(Default::default);

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
    eprintln!("gex: guest state of thread {tid}");
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
/// A guest write into the runtime's own low memory (below
/// `gex_abi::GUEST_FLOOR`), where hardware would have faulted: said, then
/// raised as an access violation for a Windows program, SIGSEGV otherwise.
fn low_access(caller: &mut Caller<'_, HostState>, what: &str, addr: u64, pc: u64) -> Result<()> {
    eprintln!("gex: {what} at {addr:#x}, in the runtime's low memory (a null pointer's field?), in block {pc:#x}: an access violation");
    if caller.data().nt.is_some() {
        return nt::fault(caller, nt::Fault::LowWrite(addr), pc);
    }
    caller.data_mut().exit = Some(Exit::Signal {
        signal: 11,
        pc,
        detail: format!("{what} at {addr:#x}, below the guest's floor"),
        internal: false,
    });
    Err(StopExecution.into())
}

fn register_imports(linker: &mut Linker<HostState>) -> Result<()> {
    let m = IMPORT_MODULE;
    libprov::register(linker)?;

    linker.func_wrap(
        m,
        imports::SYSCALL,
        |mut caller: Caller<'_, HostState>, _ctx: i32, pc: i64| -> wasmtime::Result<()> { crate::host((|| -> anyhow::Result<()> {
            if caller.data().nt.is_some() {
                let r = nt::syscall_import(&mut caller, pc as u64);
                check_i386_high(&caller, "an NT syscall");
                return r;
            }
            // An ejected project's runtime allows only the recorded calls.
            if let Some(stop) = aot::check_syscall(&mut caller, pc as u64) {
                return stop;
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
            let dump_bt = DUMP_AT.filter(|&at| DUMP_START.elapsed().as_secs_f64() >= at).and_then(|_| {
                let tid = caller.data().thread.tid;
                (!DUMPED.lock().unwrap_or_else(|e| e.into_inner()).contains(&tid))
                    .then(|| format!("{:?}", wasmtime::WasmBacktrace::force_capture(&caller)))
            });
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
            // `GEX_DUMP_AT=SECONDS`: each thread's state once, at its first syscall after that long -- where a
            // run that has stopped making progress is (a thread waiting in a loop of sleeps shows up; one
            // blocked for good does not).
            if let Some(at) = *DUMP_AT {
                if DUMP_START.elapsed().as_secs_f64() >= at
                    && DUMPED.lock().unwrap_or_else(|e| e.into_inner()).insert(state.thread.tid)
                {
                    let ctx_addr = state.thread.ctx_addr;
                    let tid = state.thread.tid;
                    let vmas = state.with_kernel(|k| k.mem.vmas().to_vec());
                    eprintln!("gex: GEX_DUMP_AT: thread {tid} at syscall {nr}");
                    dump_guest(&mut gm, ctx_addr, tid, &vmas);
                    if let Some(bt) = &dump_bt {
                        eprintln!("  wasm: {bt}");
                    }
                }
            }
            if DUMP_ABORT.load(std::sync::atomic::Ordering::Relaxed) {
                // x86-64 numbers and argument registers (rsi = x(6), rdx = x(2)) when the guest is x86-64.
                let signo = if gex_abi::X86 {
                    match gm.read_u64(state.thread.ctx_addr + ctx::x(0) as u64).unwrap_or(!0) {
                        62 | 200 => gm.read_u64(state.thread.ctx_addr + ctx::x(6) as u64),
                        234 => gm.read_u64(state.thread.ctx_addr + ctx::x(2) as u64),
                        _ => Err(gex_kernel::Errno::EINVAL),
                    }
                } else {
                    match nr {
                        gex_kernel::syscalls::nr::KILL | gex_kernel::syscalls::nr::TKILL => {
                            gm.read_u64(state.thread.ctx_addr + ctx::x(1) as u64)
                        }
                        gex_kernel::syscalls::nr::TGKILL => {
                            gm.read_u64(state.thread.ctx_addr + ctx::x(2) as u64)
                        }
                        _ => Err(gex_kernel::Errno::EINVAL),
                    }
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
                let kernel_for_service = Arc::clone(&caller.data().kernel);
                let service = move |bytes: &mut [u8]| {
                    if let Some(w) = &compositor {
                        let k = kernel_for_service
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        let gm = GuestMem::new(bytes);
                        w.pump(&k, &gm);
                    }
                };
                // Timers are checked here rather than only at syscall entry:
                // a process asleep in a wait loop issues no syscalls, so
                // nothing else would ever arm `SIGALRM`.
                let kernel_for_signals = Arc::clone(&caller.data().kernel);
                // Signals for *this* thread: masks and thread-directed signals
                // are per thread, and the kernel's loaded state may be another
                // thread's by the time this asks.
                let waiting_tid = caller.data().thread.tid;
                let interrupted = move || {
                    kernel_for_signals
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .signal_pending_for(waiting_tid)
                };
                // Blocked in the kernel counts as stopped for a provider's
                // collection; the thread parks on its way out if one runs.
                libprov::guest_stop().enter_kernel(waiting_tid as u32);
                let r = perform_block(op, memory.bytes(), &service, &interrupted);
                libprov::guest_stop().leave_kernel(waiting_tid as u32, memory.bytes());
                let mut gm = GuestMem::new(memory.bytes());
                caller
                    .data_mut()
                    .with_kernel(|k| k.finish_blocking(&mut gm, r))
            } else {
                outcome
            };
            // Wake threads parked on descriptors if this call may have made
            // one ready (`ready`).
            if caller.data_mut().with_kernel(|k| k.take_fd_activity()) {
                ready::notify();
            }
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
            // A library a stateful provider replaces (stream U): bound before
            // the loader can call into it.
            libprov::on_linux_maps(&mut caller, &mapped)?;
            // Libraries some of whose exports a provider serves (glibc's
            // mem*): bound before the dynamic linker relocates them.
            elfprov::on_elf_maps(&mut caller, &mapped)?;
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
            check_i386_high(&caller, "a syscall");
            let state = caller.data_mut();
            match outcome {
                SyscallOutcome::Continue => Ok(()),
                SyscallOutcome::Resume(target) => {
                    state.resume_cause = Some("linux syscall".into());
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
                    // A native wasm32 program: its frames are native code
                    // that cannot be resumed at a guest PC, so the child is
                    // made here and the parent carries on (`wasmprog`).
                    if state.jit.as_ref().is_some_and(|j| j.shared.lock().unwrap_or_else(|e| e.into_inner()).wasm_program) {
                        fork_process(&caller, ForkRequest { pid, child_stack, tls })?;
                        return Ok(());
                    }
                    let state = caller.data_mut();
                    // Same reason as a thread: the store is borrowed here, and
                    // a fork has to copy the whole address space.
                    state.pending_fork = Some(ForkRequest {
                        pid,
                        child_stack,
                        tls,
                    });
                    state.resume_cause = Some("fork".into());
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
                    // Started here, and the caller carries on (the child
                    // gets a store of its own; nothing of this one is
                    // borrowed). Leaving for the run loop would unwind every
                    // wasm frame, which under the function table means every
                    // caller resumes at an interior address.
                    let cx = state.spawn.clone().ok_or_else(|| anyhow!("clone with no spawn context"))?;
                    start_guest_thread(cx, ThreadState { ctx_addr, tid, clear_child_tid })?;
                    Ok(())
                }
                SyscallOutcome::Blocking(_) => {
                    unreachable!("a blocking syscall was already completed")
                }
                SyscallOutcome::ThreadExit => {
                    state.exit = Some(Exit::Status(0));
                    state.thread_only = true;
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
        })()) },
    )?;

    linker.func_wrap(
        m,
        gex_abi::ftab::LOOKUP,
        |mut caller: Caller<'_, HostState>, _ctx: i32, addr: i64| -> wasmtime::Result<i32> { crate::host((|| -> anyhow::Result<i32> { ftab::lookup(&mut caller, addr as u64) })()) },
    )?;

    linker.func_wrap(
        m,
        imports::CALL_OUT,
        |mut caller: Caller<'_, HostState>, ctx: i32, target: i64, ret: i64, sp_want: i64| -> wasmtime::Result<i32> { crate::host((|| -> anyhow::Result<i32> {
            call_out(&mut caller, ctx, target as u64, ret as u64, sp_want as u64)
        })()) },
    )?;

    linker.func_wrap(
        m,
        imports::DISPATCH_MISS,
        |mut caller: Caller<'_, HostState>, _ctx: i32, pc: i64| -> wasmtime::Result<i32> { crate::host((|| -> anyhow::Result<i32> {
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
                        state.resume_cause = Some("sigreturn".into());
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
            if nt::exc_escape(&mut caller, pc) {
                return Err(StopExecution.into());
            }
            // Translate on demand and patch the slot, then resume there.
            match translate_and_install(&mut caller, pc) {
                // Installed in this store: the miss stub dispatches again
                // in place, keeping every wasm frame below it.
                Ok(true) => Ok(gex_emit::module::DISPATCH_MISS_REDISPATCH),
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
        })()) },
    )?;

    linker.func_wrap(
        m,
        imports::ILLEGAL,
        |mut caller: Caller<'_, HostState>, _ctx: i32, pc: i64, raw: i32| -> wasmtime::Result<()> { crate::host((|| -> anyhow::Result<()> {
            let resumable = pc as u64 & gex_cgen::RESUMABLE_PC != 0;
            let pc = (pc as u64 & !gex_cgen::RESUMABLE_PC) as i64;
            if caller.data().nt.is_some() {
                // What gex could not decode, or a deliberate trap: the bytes
                // say which (the guest's handler may take it either way).
                if let Some(m) = caller.data().memory.clone() {
                    let gm = GuestMem::new(m.bytes());
                    let bytes: Vec<String> = gm.read(pc as u64, 16).map(|b| b.iter().map(|x| format!("{x:02x}")).collect()).unwrap_or_default();
                    eprintln!("gex-nt: illegal instruction at {:#x}: {}", pc as u64, bytes.join(" "));
                }
                return nt::fault_at(&mut caller, nt::Fault::Illegal, pc as u64, resumable);
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
                    state.resume_cause = Some("SIGILL".into());
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
        })()) },
    )?;

    linker.func_wrap(
        m,
        imports::BREAKPOINT,
        |mut caller: Caller<'_, HostState>, ctx: i32, pc: i64, imm: i32| -> wasmtime::Result<()> { crate::host((|| -> anyhow::Result<()> {
            let resumable = pc as u64 & gex_cgen::RESUMABLE_PC != 0;
            let pc = (pc as u64 & !gex_cgen::RESUMABLE_PC) as i64;
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
                return nt::fault_at(&mut caller, nt::Fault::from_brk(imm), pc as u64, resumable);
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
                    gex_kernel::signal::Delivery::Resume(target) => {
                        state.resume_cause = Some("SIGFPE".into());
                        state.resume_at = Some(target)
                    }
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
                    gex_kernel::signal::Delivery::Resume(target) => {
                        state.resume_cause = Some("SIGSEGV".into());
                        state.resume_at = Some(target)
                    }
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
            // An x87 exception the control word unmasks: not delivered, so
            // the process dies of SIGFPE, saying so.
            if imm == gex_abi::imports::BRK_X87_EXCEPTION {
                caller.data_mut().exit = Some(Exit::Signal {
                    signal: signal::SIGFPE,
                    pc: pc as u64,
                    detail: format!("unmasked x87 floating-point exception at 0x{pc:x}; gex does not deliver x87 exceptions"),
                    internal: true,
                });
                return Err(StopExecution.into());
            }
            caller.data_mut().exit = Some(Exit::Signal {
                signal: signal::SIGTRAP,
                pc: pc as u64,
                detail: format!("brk #{imm}"),
                internal: false,
            });
            Err(StopExecution.into())
        })()) },
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
        |mut caller: Caller<'_, HostState>, ctx: i32, pc: i64, reg: i32, v: i64| -> wasmtime::Result<()> { crate::host((|| -> anyhow::Result<()> {
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
            // PowerPC: an FPSCR rounding mode other than round-to-nearest,
            // which wasm arithmetic cannot honour, and `mffs` under
            // GEX_FP_STRICT=1 (docs/ppc.md §8). Both fail loudly.
            if gex_abi::PPC && reg == gex_kernel::ppc::SYSREG_FPSCR_RN && v != 0 {
                return Err(anyhow!(
                    "guest set a non-default FP rounding mode (FPSCR={v:#x}); gex computes only round-to-nearest"
                ));
            }
            if gex_abi::PPC && reg == gex_kernel::ppc::SYSREG_MFFS_STRICT {
                return Err(anyhow!(
                    "GEX_FP_STRICT: mffs reads FPSCR exception bits gex does not compute"
                ));
            }
            // An x87 transcendental or remainder, computed on the register
            // stack in the context.
            // `cmpxchg16b`, under the host's lock.
            if reg == gex_kernel::x86::SYSREG_CMPXCHG16B {
                if (v as u64) < gex_abi::GUEST_FLOOR {
                    return low_access(&mut caller, "a cmpxchg16b", v as u64, pc as u64);
                }
                let memory = caller
                    .data()
                    .memory
                    .clone()
                    .ok_or_else(|| anyhow!("guest memory unavailable"))?;
                let mut gm = GuestMem::new(memory.bytes());
                gex_kernel::x86::serve_cmpxchg16b(&mut gm, ctx as u32 as u64, v as u64);
            }
            if gex_abi::X86 && reg == gex_kernel::x86::SYSREG_TRACE {
                let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
                let gm = GuestMem::new(memory.bytes());
                lockstep::log_trace(&gm, ctx as u32 as u64, v as u64);
            }
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
        })()) },
    )?;

    linker.func_wrap(
        m,
        imports::CACHE_OP,
        |mut caller: Caller<'_, HostState>, _ctx: i32, pc: i64, op: i32, addr: i64| -> wasmtime::Result<()> { crate::host((|| -> anyhow::Result<()> {
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
            if op == gex_abi::cache_op::NEST_EXIT_SAMPLE {
                *EXIT_SITES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).entry((pc as u64, addr as u64)).or_default() += 1;
                // A normal return leaves PC at the word `ret` just popped
                // (still in memory below SP); anything else is a leave: the
                // function, or something it called, went elsewhere, and
                // every unit frame between it and that place is gone.
                if let Some(memory) = caller.data().memory.clone() {
                    let gm = GuestMem::new(memory.bytes());
                    let c = caller.data().thread.ctx_addr;
                    let sp = gm.read_u64(c + ctx::SP as u64).unwrap_or(0);
                    let popped = if gex_xlate::arch::compat() {
                        gm.read_u32(sp.wrapping_sub(4)).map(|v| v as u64).unwrap_or(1)
                    } else {
                        gm.read_u64(sp.wrapping_sub(8)).unwrap_or(1)
                    };
                    let leave = (addr as u64) != popped
                        && !gm.read(sp.wrapping_sub(64), 64).map(|w| w.chunks(4).any(|x| u32::from_le_bytes(x.try_into().unwrap()) as u64 == addr as u64)).unwrap_or(false);
                    // Bit 63: the target's page is marked patched (a unit
                    // leaves for a callee there instead of calling it).
                    let t = addr as u64;
                    let patched = gm.read(gex_abi::PATCHED_PAGES + (t >> 15), 1).map(|b| (b[0] >> ((t >> 12) & 7)) & 1 == 1).unwrap_or(false);
                    let addr = (t | ((patched as u64) << 63)) as i64;
                    if leave {
                        *LEAVE_SITES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).entry((pc as u64, addr as u64)).or_default() += 1;
                    }
                }
                return Ok(());
            }
            if op == gex_abi::cache_op::PAGE_STALE {
                // A block failed its page check. Translate its address again
                // only if its module still provides it; a newer translation
                // otherwise already sits in the slot dispatch reaches.
                let start = pc as u64;
                let again = match caller.data().jit.as_ref().map(|j| Arc::clone(&j.shared)) {
                    Some(shared) => {
                        let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                        let again = sh.provider.get(&start).map_or(true, |&m| m == addr as u64);
                        if again {
                            sh.translated.remove(&start);
                            sh.provider.remove(&start);
                        }
                        again
                    }
                    None => false,
                };
                if again {
                    jit_stats::STALE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    translate_and_install(&mut caller, start)?;
                } else {
                    // The newer translation is in the shared registry, but
                    // not necessarily in this thread's table yet: without
                    // the catch-up, dispatch reached the stale block again,
                    // which asked again, for ever (x64 mtx under load, all
                    // threads at one return address).
                    catch_up_in_caller(&mut caller)?;
                }
                return Ok(());
            }
            if op == gex_abi::cache_op::FTAB_TIER1 {
                if std::env::var_os("GEX_WASM_TRACE").is_some() {
                    let memory = caller.data().memory.clone();
                    let c = caller.data().thread.ctx_addr;
                    let sp = memory.map(|m| GuestMem::new(m.bytes()).read_u64(c + ctx::x(4) as u64).unwrap_or(0)).unwrap_or(0);
                    let w: Vec<String> = caller.data().callouts.iter().map(|c| format!("{:#x}@{:#x}", c.ret, c.sp_want)).collect();
                    eprintln!("gex: bridge hands {addr:#x} to tier-1 (sp {sp:#x}, call-outs [{}])", w.join(" "));
                }
                if let Some(shared) = caller.data().jit.as_ref().map(|j| Arc::clone(&j.shared)) {
                    let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                    sh.translated.remove(&(addr as u64));
                    sh.ftab.tier1_only.insert(addr as u64);
                }
                return Ok(());
            }
            if op == gex_abi::cache_op::SLOT_CONFLICT {
                if let Some(shared) = caller.data().jit.as_ref().map(|j| Arc::clone(&j.shared)) {
                    let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                    sh.translated.remove(&(addr as u64));
                }
                return Ok(());
            }
            if op == gex_abi::cache_op::LOW_ACCESS {
                return low_access(&mut caller, "a locked access", addr as u64, pc as u64);
            }
            if op == gex_abi::cache_op::SPLIT_LOCK {
                eprintln!(
                    "gex: a locked access at {:#x} (block {:#x}) spans two 8-byte words; such accesses run under one global lock, atomic against each other but not against plain accesses",
                    addr as u64, pc as u64
                );
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
        })()) },
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
        imports::X87,
        |caller: Caller<'_, HostState>,
         ctx: i32,
         op: i32,
         a_sig: i64,
         a_se: i32,
         b_sig: i64,
         b_se: i32,
         cw: i32|
         -> wasmtime::Result<i64> { crate::host((|| -> anyhow::Result<i64> {
            let (r, se, sw) = x87_op(op, a_sig as u64, a_se as u16, b_sig as u64, b_se as u16, cw as u16);
            let memory = caller
                .data()
                .memory
                .clone()
                .ok_or_else(|| anyhow!("guest memory unavailable"))?;
            let data = memory.bytes();
            let at = ctx as u32 as usize + ctx::X87_TMP + 8;
            data[at..at + 4].copy_from_slice(&se.to_le_bytes());
            data[at + 4..at + 8].copy_from_slice(&sw.to_le_bytes());
            Ok(r as i64)
        })()) },
    )?;

    linker.func_wrap(
        m,
        imports::SAFEPOINT,
        |mut caller: Caller<'_, HostState>, _ctx: i32, pc: i64| -> wasmtime::Result<()> { crate::host((|| -> anyhow::Result<()> {
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
            if nt::exc_escape(&mut caller, pc as u64) {
                return Err(StopExecution.into());
            }
            // A call-out whose caller's frame the stack has unwound past
            // (longjmp, an exception) is abandoned: its caller leaves.
            if let Some(co) = caller.data().callouts.last() {
                let ctx_addr = caller.data().thread.ctx_addr;
                let sp = GuestMem::new(memory.bytes()).read_u64(ctx_addr + ctx::x(sp_index()) as u64).unwrap_or(0);
                // Not at a function's own address under a native caller (the
                // bridge's poll there): leaving would hand the function back
                // with the PC its body left for, which the bridge takes as
                // "nothing served it" and gives the address to tier-1. The
                // entry about to run sees the guest left and leaves itself
                // (gex_nr::NATIVE_LEFT).
                let at_function = || {
                    co.ret == gex_kernel::nt::boot::tramp::NATIVE_RETURN_PAD
                        && caller.data().jit.as_ref().is_some_and(|j| j.shared.lock().unwrap_or_else(|e| e.into_inner()).ftab.map.get(pc as u64).is_some())
                };
                if sp > co.sp_want + co.unwound && !at_function() {
                    if std::env::var_os("GEX_WASM_TRACE").is_some() {
                        eprintln!("gex: safepoint at {pc:#x} leaves the call-out to {:#x} (sp {sp:#x}, want {:#x})", co.ret, co.sp_want);
                    }
                    let _ = GuestMem::new(memory.bytes()).write_u64(ctx_addr + ctx::PC as u64, pc as u64);
                    caller.data_mut().callout_signal = Some(CalloutSignal::Abort);
                    return Err(StopExecution.into());
                }
            }
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
            // An NT thread another has suspended (NtSuspendThread) stops
            // here, its registers written back, until it is resumed.
            if let Some(p) = state.nt.clone() {
                gex_kernel::nt::suspend_point(&p, ctx_addr);
            }
            // A stateful provider's collection stops guest threads here.
            if libprov::guest_stop().active() {
                libprov::guest_stop().park_if_stopping(state.thread.tid as u32, gm.bytes, false);
            }
            match state.with_kernel(|k| k.deliver_pending(&mut gm)) {
                gex_kernel::signal::Delivery::None => Ok(()),
                gex_kernel::signal::Delivery::Resume(target) => {
                    state.resume_cause = Some("signal at safepoint".into());
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
        })()) },
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
    // No translator in this build (gex-rt): nothing is ever translated.
    if cfg!(not(feature = "translate")) {
        return Ok(false);
    }
    if caller.data().nt.is_some() && profile::active() {
        profile::note_nt_entry(pc);
    }
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

    // A function the table holds, or the run-time translator can make,
    // serves this address: tier-1 reaches it through the bridge.
    if ftab::serve(caller, pc)? {
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
    remiss(caller, &shared, pc)?;
    // Not serialised: misses on several threads translate concurrently, each
    // from this snapshot, and two that both lack a page each allocate it a
    // base (the shared mark keeps the two distinct). The first to publish
    // keeps it (`PageTable::insert_entries`); the other's blocks there are
    // stale from the start and are not counted as translated.
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
    // What is already translated, asked of the shared state per address
    // rather than copied: a copy of the set (half a million entries for
    // Noita) per miss was 0.7% of tier-1's CPU in memcpy alone.
    let known = |a: u64| shared.lock().unwrap_or_else(|e| e.into_inner()).translated.contains(&a);
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
            &known,
            page_table,
            limits,
            // A cap keeps one miss from translating an entire library
            // synchronously; the next miss picks up where this stopped.
            4096,
            true,
            &Default::default(),
        )
    };
    jit_stats::note(pc);
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
    let t0 = std::time::Instant::now();
    // `GEX_DUMP_BAD_WASM=DIR`: keep a translation the engine rejects, as
    // DIR/PC.wasm, to take apart offline (wasm-tools validate/print).
    let keep_bad = |e: anyhow::Error| -> anyhow::Error {
        if let Some(dir) = std::env::var_os("GEX_DUMP_BAD_WASM") {
            let path = std::path::Path::new(&dir).join(format!("{pc:x}.wasm"));
            let _ = std::fs::create_dir_all(&dir);
            if std::fs::write(&path, &translated_module.wasm).is_ok() {
                return e.context(format!("the module is in {}", path.display()));
            }
        }
        e
    };
    let module = match cache {
        Some((dir, build)) => {
            let key = gex_cache::key(&build, [translated_module.wasm.as_slice()]);
            let wasm = translated_module.wasm.clone();
            let l = gex_cache::load_or_compile(&engine, &dir, &key, move || Ok((wasm, Vec::new())))
                .map_err(keep_bad)
                .context("compiling runtime-translated wasm")?;
            let c = if l.hit { &jit_stats::CACHE_HITS } else { &jit_stats::CACHE_MISSES };
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            l.module
        }
        None => gex_cache::compile(&engine, &translated_module.wasm)
            .map_err(keep_bad)
            .context("validating runtime-translated wasm")?,
    };
    jit_stats::COMPILE_US.fetch_add(t0.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);

    // Publish first, install second. The store's watermark only ever advances
    // through `replay_registry`, so installing here as well would double-count
    // whenever another thread appended between the catch-up above and this
    // push -- which is exactly what filled one store with ten thousand
    // instances of the same modules.
    publish_unit(&shared, &translated_module, module, stats.blocks)?;
    replay_registry(caller)?;

    // If the address that faulted did not end up with code, resuming would
    // fault again at the same place forever. Say so instead -- unless its
    // page was retired while this translation was being made (another
    // thread's block guard saw a JIT rewrite code there, or an munmap or
    // mprotect): `publish_unit` then counts the block stale, and translating
    // again from the current memory is the answer. Bounded, so a page that
    // keeps changing under its translator still reports.
    let (covered, retired) = {
        let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        let page = pc / gex_abi::DISPATCH_PAGE;
        (sh.translated.contains(&pc), sh.page_table.lookup(page) != translated_module.page_table.lookup(page))
    };
    if !covered {
        thread_local! {
            static RETRIES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        }
        const MAX_RETRIES: u32 = 16;
        if retired && RETRIES.with(|r| r.get()) < MAX_RETRIES {
            jit_stats::STALE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            RETRIES.with(|r| r.set(r.get() + 1));
            let again = translate_and_install(caller, pc);
            RETRIES.with(|r| r.set(r.get() - 1));
            return again;
        }
        bail!("translated blocks but none covered 0x{pc:x}{}", if retired { " (its page kept being retired)" } else { "" });
    }
    Ok(true)
}

/// How many times an address already translated may miss dispatch again
/// before the run stops. A miss there is legitimate a few times (a
/// neighbour's slot conflict, a page retired and translated anew), never
/// hundreds: translating it again each time ate 100 MB/s until MemoryMax.
const REMISS_LIMIT: u32 = 64;

/// Count a miss at `pc` when it is already translated, and past
/// [`REMISS_LIMIT`] stop with what dispatch sees there.
fn remiss(caller: &mut Caller<'_, HostState>, shared: &Arc<Mutex<JitShared>>, pc: u64) -> Result<()> {
    let ctx_addr = caller.data().thread.ctx_addr;
    let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let gm = GuestMem::new(memory.bytes());
    let source = gm.read_u64(ctx_addr + ctx::LAST_SOURCE as u64).unwrap_or(0);
    let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
    if !sh.translated.contains(&pc) {
        return Ok(());
    }
    let n = {
        let e = sh.remiss.entry(pc).or_insert((0, [0; 4]));
        e.1[(e.0 % 4) as usize] = source;
        e.0 += 1;
        e.0
    };
    if n <= REMISS_LIMIT {
        return Ok(());
    }
    let page = pc / gex_abi::DISPATCH_PAGE;
    let dir = gm.read_u32(gex_abi::DISPATCH_DIR + page * 4).unwrap_or(u32::MAX);
    let base = sh.page_table.lookup(page);
    let slot = base.map(|b| b as u64 + ((pc % gex_abi::DISPATCH_PAGE) >> gex_abi::DISPATCH_SHIFT));
    let owner = base.and_then(|b| sh.slot_owner.get(&b).copied());
    let units: Vec<usize> = sh
        .registry
        .iter()
        .enumerate()
        .filter(|(_, u)| u.exported_blocks.iter().any(|(a, _)| *a == pc))
        .map(|(i, _)| i)
        .collect();
    let retired = sh.retired_pages.get(&page).copied().unwrap_or(0);
    let sources = sh.remiss[&pc].1;
    let (n_units, stale) = (sh.registry.len(), sh.stale_blocks);
    drop(sh);
    let writable = caller.data_mut().with_kernel(|k| k.mem.check_access(pc, 1, prot::WRITE));
    eprintln!(
        "gex: BUG: {pc:#x} is translated but missed dispatch {n} times: page {page:#x} directory          entry {dir} (shared table: {base:?}, slot {slot:?}, base owned by page {owner:x?});          in units {:?} of {n_units} (first, last); page retired {retired} times; writable now:          {writable}; stale blocks so far {stale}; last miss sources {sources:x?}",
        (units.first(), units.last())
    );
    bail!("{pc:#x} keeps missing dispatch after translation (retranslation livelock)")
}

/// Replay anything this store has not seen, cheaply enough to call often: the
/// check is a length comparison, and the registry is append-only so a store
/// catches up by walking forward from its own watermark.
fn catch_up_in_caller(caller: &mut Caller<'_, HostState>) -> Result<()> {
    let behind = match caller.data().jit.as_ref() {
        // Nothing published past this store's watermark: no lock (every
        // syscall and safepoint comes through here).
        Some(jit) if REGISTRY_LEN.load(std::sync::atomic::Ordering::Acquire) <= jit.replayed => false,
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
    refresh_callouts(&mut *caller)?;
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
    aot_share::note(&module, aot_share::Kind::Tier1);
    {
        use std::sync::atomic::Ordering::Relaxed;
        jit_stats::PUBLISHED.fetch_add(1, Relaxed);
        jit_stats::BLOCKS.fetch_add(blocks as u64, Relaxed);
        jit_stats::WASM_BYTES.fetch_add(translated.wasm.len() as u64, Relaxed);
        jit_stats::SLOTS.fetch_max(translated.table_slots, Relaxed);
    }
    let unit = Arc::new(CompiledUnit {
        module: Some(module),
        exported_blocks: translated.exported_blocks.clone(),
        dir_entries: translated.dir_entries.clone(),
        table_slots: translated.table_slots,
        bridges: Vec::new(),
    });
    let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
    for &(page, base) in &translated.dir_entries {
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
    // Only the pages this translation allocated join the shared table. The
    // rest of its snapshot is what the table held when it started; a page
    // retired since then must stay retired (merging the snapshot brought it
    // back with its old base while its directory entry stayed zero, so every
    // translation in it reused that base, never wrote the directory, and
    // missed again: Noita's retranslation livelock).
    sh.page_table.insert_entries(&translated.dir_entries);
    // A block counts as translated only if its page still has the base it
    // was placed at; otherwise its slots are abandoned and the next miss
    // translates it again into the page's new slots.
    let mut stale = 0usize;
    for (a, _) in &translated.exported_blocks {
        let page = *a / gex_abi::DISPATCH_PAGE;
        if sh.page_table.lookup(page).is_some() && sh.page_table.lookup(page) == translated.page_table.lookup(page) {
            sh.translated.insert(*a);
            sh.provider.insert(*a, translated.module_id);
        } else {
            stale += 1;
        }
    }
    sh.stale_blocks += stale;
    {
        let mut by = TIER1_PAGES.lock().unwrap_or_else(|e| e.into_inner());
        let by = by.get_or_insert_with(Default::default);
        for (a, _) in &translated.exported_blocks {
            *by.entry(*a / gex_abi::DISPATCH_PAGE).or_default() += 1;
        }
    }
    sh.registry.push(Arc::clone(&unit));
    crate::REGISTRY_LEN.store(sh.registry.len(), std::sync::atomic::Ordering::Release);
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
            jit_stats::GROWN.fetch_add(want - have, std::sync::atomic::Ordering::Relaxed);
            t.grow(cx.as_context_mut(), want - have, miss.clone())
                .with_context(|| format!("growing dispatch table {i}"))?;
        }
    }

    if unit.module.is_none() {
        // Bridges only: tier-1's slots for addresses a function serves.
        ftab::install_bridges(cx.as_context_mut(), &unit.bridges)?;
        let current: Vec<(u64, u32)> = {
            let jit = cx.as_context().data().jit.as_ref().unwrap();
            let sh = jit.shared.lock().unwrap_or_else(|e| e.into_inner());
            unit.dir_entries.iter().copied().filter(|&(p, b)| sh.page_table.lookup(p) == Some(b)).collect()
        };
        return write_dispatch_dir(memory, &current);
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
    jit_stats::INSTALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if !cx.as_context().data().callouts.is_empty() {
        let slots: std::collections::HashSet<u64> =
            unit.exported_blocks.iter().filter_map(|(a, _)| dispatch_slot_of(&memory, *a)).collect();
        note_overwritten(&mut cx, &|s| slots.contains(&s));
    }
    let instance = linker
        .instantiate(cx.as_context_mut(), unit.module.as_ref().unwrap())
        .context("instantiating a runtime-translated module")?;
    profile::attach(cx.as_context_mut(), &instance)?;

    // The module carries element segments for the table it imports, so
    // instantiating it already filled its slots. Writing the directory is
    // idempotent: it lives in shared guest memory.
    // Only entries still current: replaying an old unit into a new store
    // must not point a page retired since back at its abandoned slots.
    let current: Vec<(u64, u32)> = {
        let jit = cx.as_context().data().jit.as_ref().unwrap();
        let sh = jit.shared.lock().unwrap_or_else(|e| e.into_inner());
        unit.dir_entries.iter().copied().filter(|&(p, b)| sh.page_table.lookup(p) == Some(b)).collect()
    };
    write_dispatch_dir(memory, &current)?;
    cx.as_context_mut()
        .data_mut()
        .jit
        .as_mut()
        .unwrap()
        .instances
        .push(instance);
    Ok(())
}

/// Blocks tier-1 has translated, by guest page, for GEX_NT_STATS's split
/// by image.
static TIER1_PAGES: Mutex<Option<std::collections::HashMap<u64, u64>>> = Mutex::new(None);

/// Tier-1's translated blocks so far, by page.
pub(crate) fn tier1_pages() -> Vec<(u64, u64)> {
    TIER1_PAGES.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|m| m.iter().map(|(p, n)| (*p, *n)).collect()).unwrap_or_default()
}

/// Sampled nest exits by (the root's function, where it went), one in 4096.
static EXIT_SITES: Mutex<Option<std::collections::HashMap<(u64, u64), u64>>> = Mutex::new(None);

/// The `n` most sampled nest exits since the last call, as
/// `function->target count`.
/// Sampled nest exits that were not a plain return (see NEST_EXIT_SAMPLE).
static LEAVE_SITES: Mutex<Option<std::collections::HashMap<(u64, u64), u64>>> = Mutex::new(None);

/// The `n` most sampled leaves since the last call, and their total.
pub(crate) fn take_leave_sites(n: usize) -> (u64, Vec<(u64, u64, u64)>) {
    let m = LEAVE_SITES.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    let total = m.values().sum();
    let mut v: Vec<_> = m.into_iter().map(|((f, t), c)| (f, t, c)).collect();
    v.sort_by(|a, b| b.2.cmp(&a.2));
    v.truncate(n);
    (total, v)
}

pub(crate) fn take_exit_sites(n: usize) -> Vec<(u64, u64, u64)> {
    let m = EXIT_SITES.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    let mut v: Vec<_> = m.into_iter().map(|((f, t), c)| (f, t, c)).collect();
    v.sort_by(|a, b| b.2.cmp(&a.2));
    v.truncate(n);
    v
}

/// A function-tier call-out in progress (`call_out`): the return it waits
/// for, and the dispatch slot it watches for it with the old content.
struct Callout {
    ret: u64,
    sp_want: u64,
    /// How far above `sp_want` a safepoint must find the stack pointer to
    /// call the caller unwound: `CALLOUT_UNWOUND`, or `NATIVE_UNWOUND` for a
    /// guest call from native Wine.
    unwound: u64,
    /// `(slot, what the slot held)` while watched.
    slot: Option<(u64, wasmtime::Ref)>,
    /// The slot holds the watch stub (an install may have written over it).
    live: bool,
}

/// What the watch stub found when a dispatch reached a watched slot.
pub(crate) enum CalloutSignal {
    /// The return call-out `i` waits for.
    Return(usize),
    /// Something else that shares the slot: run what it held.
    Bypass(wasmtime::Func),
    /// The stack unwound past the innermost call-out's caller.
    Abort,
    /// Control left nested exception dispatch `i` (`nt::raise_nested`) for
    /// the state in the context: every loop between passes it on.
    Escape(usize),
}

/// Call-outs nested deeper than this leave instead: each holds a host frame
/// and a nested wasm call.
const CALLOUT_MAX_DEPTH: usize = 32;

/// How far above `sp_want` a safepoint must find the stack pointer to call
/// a call-out's caller unwound (well past any argument popping), and how far
/// above it the awaited return may come back: what a stdcall callee pops,
/// or, for a call-out entered inside a function (a unit body resuming at a
/// `setjmp` or `catch` continuation, `gex_resume`), the rest of that
/// function's frame. `sp_want` is the stack at the call-out plus the
/// return address; a deeper activation returning to the same address
/// (recursion) is always below it.
const CALLOUT_UNWOUND: u64 = 64 << 10;

/// gex_nr::NATIVE_LEFT: whether the innermost call-out is a native caller's
/// and the guest, at `sp`, is above its callee's frame: then it left that
/// caller's frames. gexrt's mark is only a hint (its entry stack pointer
/// goes stale when a trap skips an entry's restore).
pub(crate) fn innermost_native_unwound(caller: &Caller<'_, HostState>, sp: u64) -> bool {
    caller
        .data()
        .callouts
        .last()
        .is_some_and(|co| co.ret == gex_kernel::nt::boot::tramp::NATIVE_RETURN_PAD && sp > co.sp_want + co.unwound)
}

/// The same for a guest call from native Wine (its return address is
/// `tramp::NATIVE_RETURN_PAD`): the callee's frame is built just below the
/// native caller's (gexrt `__gexw_guest_call`), so anything past its
/// arguments is the native frames' and the guest has left them -- a catch
/// block continuing in an outer frame. Its native callers then leave
/// (gexrt's `run_until` drops them), instead of the guest running on
/// nested inside them.
const NATIVE_UNWOUND: u64 = 512;

/// The table-0 slot dispatch reaches `pc` through, as the directory says now.
fn dispatch_slot_of(memory: &gex_mem::GuestMemory, pc: u64) -> Option<u64> {
    let page = pc / gex_abi::DISPATCH_PAGE;
    if page >= gex_abi::DISPATCH_MAX_PAGES {
        return None;
    }
    let entry = GuestMem::new(memory.bytes()).read_u32(gex_abi::DISPATCH_DIR + page * 4).ok()?;
    if gex_abi::dispatch_entry_table(entry) != 0 {
        return None;
    }
    Some(gex_abi::dispatch_index(pc, entry))
}

/// The per-store stub a watched slot holds (`call_out`), made once.
fn watch_stub(cx: &mut impl wasmtime::AsContextMut<Data = HostState>) -> wasmtime::Func {
    if let Some(f) = cx.as_context().data().watch_stub.clone() {
        return f;
    }
    let f = wasmtime::Func::wrap(cx.as_context_mut(), |mut caller: Caller<'_, HostState>, ctx: i32| -> wasmtime::Result<()> { crate::host((|| -> anyhow::Result<()> {
        let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
        let gm = GuestMem::new(memory.bytes());
        let c = ctx as u32 as u64;
        let target = if gex_abi::X86 { gm.read_u64(c + ctx::LAST_TARGET as u64) } else { gm.read_u64(c + ctx::PC as u64) }.unwrap_or(0);
        let sp = gm.read_u64(c + ctx::x(sp_index()) as u64).unwrap_or(0);
        let state = caller.data_mut();
        let hit = state.callouts.iter().rposition(|co| co.ret == target && sp >= co.sp_want && sp <= co.sp_want + CALLOUT_UNWOUND);
        let signal = match hit {
            Some(i) => CalloutSignal::Return(i),
            None => {
                // Another address in the slot, or the same return for a
                // deeper frame (recursion): what the slot held runs it.
                let slot = dispatch_slot_of(&memory, target);
                let old = state.callouts.iter().rev().find_map(|co| match &co.slot {
                    Some((s, r)) if Some(*s) == slot => r.as_func().flatten().cloned(),
                    _ => None,
                });
                match old {
                    Some(f) => CalloutSignal::Bypass(f),
                    None => bail!("a watched dispatch slot with nothing to run for {target:#x}"),
                }
            }
        };
        let _ = GuestMem::new(memory.bytes()).write_u64(c + ctx::PC as u64, target);
        state.callout_signal = Some(signal);
        Err(StopExecution.into())
    })()) });
    cx.as_context_mut().data_mut().watch_stub = Some(f.clone());
    f
}

/// The stack pointer's register index in the context.
fn sp_index() -> u8 {
    if gex_abi::X86 {
        4
    } else {
        31
    }
}

/// An install wrote dispatch slots `written`: a watched one no longer holds
/// the stub, and what it now holds is what the return should run when it is
/// not the awaited one.
fn note_overwritten(cx: &mut impl wasmtime::AsContextMut<Data = HostState>, written: &dyn Fn(u64) -> bool) {
    for co in cx.as_context_mut().data_mut().callouts.iter_mut() {
        if let Some((s, _)) = &co.slot {
            if written(*s) {
                co.live = false;
            }
        }
    }
}

/// Point every call-out's watched slot at the watch stub again, wherever the
/// directory now says its return is: a translation or a retirement may have
/// moved it, and an install may have written over it.
fn refresh_callouts(cx: &mut impl wasmtime::AsContextMut<Data = HostState>) -> Result<()> {
    if cx.as_context().data().callouts.is_empty() {
        return Ok(());
    }
    let Some(memory) = cx.as_context().data().memory.clone() else { return Ok(()) };
    let Some(table) = cx.as_context().data().jit.as_ref().map(|j| j.tables[0]) else { return Ok(()) };
    let stub = watch_stub(cx);
    let size = table.size(cx.as_context_mut());
    let n = cx.as_context().data().callouts.len();
    for i in 0..n {
        let ret = cx.as_context().data().callouts[i].ret;
        let want = dispatch_slot_of(&memory, ret).filter(|&s| s < size);
        let have = cx.as_context().data().callouts[i].slot.as_ref().map(|(s, _)| *s);
        if have.is_some() && have != want {
            // The return moved: put the old slot back, unless another
            // call-out still watches it or an install wrote over the stub.
            let (s, old, live) = {
                let mut c = cx.as_context_mut();
                let co = &mut c.data_mut().callouts[i];
                let (s, old) = co.slot.take().unwrap();
                (s, old, std::mem::replace(&mut co.live, false))
            };
            let shared = cx.as_context().data().callouts.iter().any(|c| matches!(&c.slot, Some((os, _)) if *os == s));
            if live && !shared {
                table.set(cx.as_context_mut(), s, old)?;
            }
        }
        let Some(s) = want else { continue };
        // Is the stub live in `s` for some call-out?
        let sharing = cx.as_context().data().callouts.iter().position(|co| co.live && matches!(&co.slot, Some((os, _)) if *os == s));
        if let Some(j) = sharing {
            let old = cx.as_context().data().callouts[j].slot.as_ref().unwrap().1.clone();
            let mut c = cx.as_context_mut();
            let co = &mut c.data_mut().callouts[i];
            co.slot = Some((s, old));
            co.live = true;
            continue;
        }
        // The slot holds real content: save it for every call-out watching
        // it, and arm the stub.
        let cur = table.get(cx.as_context_mut(), s).ok_or_else(|| anyhow!("dispatch slot {s} out of range"))?;
        for (k, co) in cx.as_context_mut().data_mut().callouts.iter_mut().enumerate() {
            if k == i || matches!(&co.slot, Some((os, _)) if *os == s) {
                co.slot = Some((s, cur.clone()));
                co.live = true;
            }
        }
        table.set(cx.as_context_mut(), s, wasmtime::Ref::Func(Some(stub.clone())))?;
    }
    Ok(())
}

/// An `enter` that reaches every slot: the calling module's, or -- when the
/// call came from code that has none (a function-tier unit, a function JIT
/// module) -- any tier-1 module's in this store, or the thread loop's root
/// module's. They all reach every slot through the same tables.
pub(crate) fn any_enter(caller: &mut Caller<'_, HostState>) -> Option<wasmtime::Func> {
    // The thread loop's, when there is one: the others are found by name,
    // one instance at a time, and a call-out from a unit (most of them)
    // would look through every tier-1 module before reaching it.
    if let Some(f) = caller.data().root_enter.clone() {
        return Some(f);
    }
    if let Some(f) = caller.get_export(exports::ENTER).and_then(Extern::into_func) {
        return Some(f);
    }
    let instances = caller.data().jit.as_ref().map(|j| j.instances.clone()).unwrap_or_default();
    instances
        .iter()
        .find_map(|i| i.get_func(&mut *caller, exports::ENTER))
        .or_else(|| caller.data().root_enter.clone())
}

/// `gex.call_out` (docs/function-tier.md §7.3): run the guest from `target`
/// until it returns to `ret` with the stack pointer at `sp_want` (or what a
/// callee popped above it), then let the C caller carry on (1); 0 when that
/// cannot be done, and the caller leaves as before.
fn call_out(caller: &mut Caller<'_, HostState>, ctx_i: i32, target: u64, ret: u64, sp_want: u64) -> Result<i32> {
    if caller.data().callouts.len() >= CALLOUT_MAX_DEPTH || caller.data().jit.is_none() {
        return Ok(0);
    }
    let Some(enter) = any_enter(caller) else { return Ok(0) };
    CALLOUTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let ctx_addr = ctx_i as u32 as u64;
    let unwound = if ret == gex_kernel::nt::boot::tramp::NATIVE_RETURN_PAD { NATIVE_UNWOUND } else { CALLOUT_UNWOUND };
    caller.data_mut().callouts.push(Callout { ret, sp_want, unwound, slot: None, live: false });
    let me = caller.data().callouts.len() - 1;
    refresh_callouts(&mut *caller)?;
    let mut next: Option<wasmtime::Func> = None;
    let mut pc = target;
    let result = loop {
        let r = match next.take() {
            Some(f) => f.call(&mut *caller, &[Val::I32(ctx_i)], &mut []),
            None => enter.call(&mut *caller, &[Val::I32(ctx_i), Val::I64(pc as i64)], &mut []),
        };
        if let Some(child) = caller.data_mut().pending_spawn.take() {
            let cx = caller.data().spawn.clone().ok_or_else(|| anyhow!("thread creation with no spawn context"))?;
            start_guest_thread(cx, child)?;
        }
        let e = match r {
            Ok(()) => break Err(anyhow!("guest code returned out of a call-out")),
            Err(e) => anyhow::Error::from(e),
        };
        if let Some(sig) = caller.data_mut().callout_signal.take() {
            match sig {
                CalloutSignal::Return(i) if i == me => break Ok(1),
                // An outer call-out's return: this one's caller is gone.
                CalloutSignal::Return(_) | CalloutSignal::Abort => break Ok(0),
                CalloutSignal::Bypass(f) => {
                    next = Some(f);
                    continue;
                }
                // Not a frame of this call-out's: down to the dispatch's
                // loop, every wasm frame between with it.
                sig @ CalloutSignal::Escape(_) => {
                    caller.data_mut().callout_signal = Some(sig);
                    break Err(anyhow::Error::from(e));
                }
            }
        }
        if caller.data().exit.is_some() {
            break Err(anyhow::Error::from(e));
        }
        if let Some(at) = caller.data_mut().resume_at.take() {
            crate::ftab::stats::RESUMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            note_resume(caller.data_mut(), at, "call-out");
            pc = at;
            refresh_callouts(&mut *caller)?;
            continue;
        }
        if is_stale_table_trap(&e) {
            catch_up_in_caller(caller)?;
            refresh_callouts(&mut *caller)?;
            pc = GuestMem::new(memory.bytes()).read_u64(ctx_addr + ctx::PC as u64).unwrap_or(pc);
            continue;
        }
        break Err(anyhow::Error::from(e));
    };
    // Unwatch: put the slot back unless an install has written over it.
    if let Some(co) = caller.data_mut().callouts.pop() {
        if let (Some((s, old)), Some(table)) = (co.slot, caller.data().jit.as_ref().map(|j| j.tables[0])) {
            let outer_watches = caller.data().callouts.iter().any(|c| matches!(&c.slot, Some((os, _)) if *os == s));
            if co.live && !outer_watches {
                table.set(&mut *caller, s, old)?;
            }
        }
    }
    if matches!(result, Ok(0)) {
        CALLOUT_ABORTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    result
}

/// Call-outs started and abandoned (GEX_NT_STATS).
pub(crate) static CALLOUTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) static CALLOUT_ABORTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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

    // Units' rewritten pages first: a lookup after the probe cache is
    // cleared below must already see them (`ftab::current`).
    nt_cgen::on_retire(&memory, ranges);
    nt_cgen::provided_rewrites(&memory, ranges);
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
                *sh.retired_pages.entry(page).or_default() += 1;
                jit_stats::RETIRED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let off = (gex_abi::DISPATCH_DIR + page * 4) as usize;
            let data = memory.bytes();
            if off + 4 <= data.len() {
                data[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
            }
        }
    }
    ftab::on_retire(&mut sh, memory.bytes(), ranges);
    drop(sh);
    refresh_callouts(caller)?;
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
    if caller.as_context().data().jit.is_none() || cfg!(not(feature = "translate")) {
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
    // What is already translated, asked of the shared state per address
    // rather than copied: a copy of the set (half a million entries for
    // Noita) per miss was 0.7% of tier-1's CPU in memcpy alone.
    let known = |a: u64| shared.lock().unwrap_or_else(|e| e.into_inner()).translated.contains(&a);
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
            &known,
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
            None => gex_cache::compile(&engine, &translated.wasm)
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
        // The wasm frames, when the engine kept them (GEX_WASM_BACKTRACE=1).
        let bt = e.downcast_ref::<wasmtime::WasmBacktrace>().map(|b| format!("\n{b:?}")).unwrap_or_default();
        return anyhow!("{detail} (pc≈0x{pc:x}){bt}");
    }
    e
}

/// Record a taken resume's cause (set with `resume_at`) and target, under
/// GEX_NT_STATS; `taker` names the loop that took it when no cause was set.
pub(crate) fn note_resume(state: &mut HostState, target: u64, taker: &str) {
    if state.nt.is_some() && profile::active() {
        profile::note_nt_entry(target);
    }
    let cause = state.resume_cause.take();
    if crate::ftab::stats_on() {
        crate::ftab::stats::note_resume_cause(cause.unwrap_or_else(|| format!("untagged ({taker})")), target);
    }
}

/// The wasm stack each guest thread may use (`GEX_WASM_STACK_MB`, default
/// 64 MiB; wasmtime's own default is 512 KiB): units are call-returning, so
/// guest call depth is wasm stack depth. Natively the units' depth guard is
/// what keeps them inside it.
pub fn wasm_stack_bytes() -> usize {
    std::env::var("GEX_WASM_STACK_MB").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(64) << 20
}

/// The host stack for a thread that runs guest code: the wasm stack plus
/// room for the host's own frames below and above it (reserved, touched
/// only as used).
pub fn guest_thread_stack() -> usize {
    wasm_stack_bytes() + (16 << 20)
}

/// `GEX_CHECK_I386_HIGH=1`: after a host function that writes guest
/// registers, an i386 process's GPR slots must have zero high halves
/// (`gex_kernel::gpr`), which compiled code may rely on. A slot that does
/// not is reported with where it was found (the first 32).
pub(crate) fn check_i386_high(cx: &impl wasmtime::AsContext<Data = HostState>, site: &str) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if !*ON.get_or_init(|| std::env::var("GEX_CHECK_I386_HIGH").as_deref() == Ok("1")) || !gex_kernel::compat32() {
        return;
    }
    let d = cx.as_context().data();
    let Some(memory) = d.memory.as_ref() else { return };
    let base = d.thread.ctx_addr as usize;
    for i in 0..8u8 {
        let v = read_u64(memory.bytes(), base + ctx::x(i));
        if v >> 32 != 0 && SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 32 {
            let pc = read_u64(memory.bytes(), base + ctx::PC);
            eprintln!("gex: GEX_CHECK_I386_HIGH: x{i} = {v:#x} after {site} (pc {pc:#x})");
        }
    }
}
