//! Running an ejected project (`gex-eject`): prebuilt modules instead of
//! translation, and library replacements.
//!
//! An ejected project ships its program's tier-1 modules, built ahead of time
//! by `gexc eject build`, and optionally some user-supplied wasm functions that
//! replace guest functions. [`Aot`] is what the runtime is told about them:
//!
//! - **Images.** A program's image module is looked up by the image's
//!   identity ([`image_identity`]) instead of being translated. A program the
//!   project has no module for fails loudly: it was not part of the recorded
//!   session.
//! - **Replacements.** The build emits a guest function the user replaced as
//!   one `sysreg_write(pc, REPLACE_SYSREG, id)` (see
//!   `gex_xlate::Intrinsic::Replace`). The host runs replacement `id` here,
//!   against the calling thread's context.
//!
//! # Where a replacement module lives
//!
//! A replacement is ordinary wasm32 code (clang, or any producer) that
//! imports the guest's memory as `env.memory`. Because a guest address *is* a
//! linear-memory offset, a guest pointer is a C pointer to it: no translation.
//! Its own data and stacks need somewhere nothing else uses, natively *and* in
//! the browser (where the shim keeps its stack and data in
//! `[STACK_TOP, MMAP_BOTTOM)`). Above [`gex_abi::MMAP_TOP`] is that: the guest
//! is never given an address there and the browser's arena never claims one.
//! The first [`REPL_DATA_BYTES`] of [`REPL_BASE`] hold the module's data and
//! heap (link it with `--global-base=` [`REPL_BASE`]); after them come
//! [`REPL_THREADS`] stacks of [`REPL_STACK_BYTES`], one per guest thread by
//! context slot. The region is recorded as a mapping so `fork` copies it.

use super::{read_u64, write_u64, HostState};
use anyhow::{anyhow, bail, Context, Result};
use gex_abi::ctx;
use gex_ld::Image;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use wasmtime::{Caller, Engine, ExternType, Func, Linker, Module, Mutability, Val, ValType};

/// Where replacement modules' data begins: link them with this as
/// `--global-base`.
pub const REPL_BASE: u64 = 0xF000_0000;
/// Bytes for a replacement module's data and heap.
pub const REPL_DATA_BYTES: u64 = 8 << 20;
/// One guest thread's stack for replacement code.
pub const REPL_STACK_BYTES: u64 = 128 << 10;
/// How many guest threads can run replacement code.
pub const REPL_THREADS: u64 = 64;
/// End of the whole replacement region.
pub const REPL_END: u64 = REPL_BASE + REPL_DATA_BYTES + REPL_THREADS * REPL_STACK_BYTES;
const _: () = assert!(REPL_BASE >= gex_abi::MMAP_TOP && REPL_END <= gex_abi::GUEST_MEM_BYTES);

/// How a replacement takes its arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Abi {
    /// `void f(uint32_t ctx)`: the function reads the guest's registers from
    /// the context and writes its result there itself, like a pewasm import
    /// binding. Full control: stack arguments, structs, several results.
    Ctx,
    /// A plain C function, `r f(a, b, ...)`, typed by its wasm signature: the
    /// host passes the guest's integer and floating-point argument registers
    /// in order and puts the result in the guest's return register.
    C,
    /// `void f(uint32_t ctx)` that *is* the function, return included: on
    /// return the context's PC says where the guest goes next (the return
    /// address, or wherever it left for). What `gexc eject decompile` writes:
    /// the function tier's C for the function (`gex_cgen::standalone`).
    Entry,
}

impl Abi {
    pub fn parse(s: &str) -> Option<Abi> {
        match s {
            "ctx" => Some(Abi::Ctx),
            "c" => Some(Abi::C),
            "entry" => Some(Abi::Entry),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Abi::Ctx => "ctx",
            Abi::C => "c",
            Abi::Entry => "entry",
        }
    }
}

/// One guest function served by a user wasm function.
#[derive(Clone, Debug)]
pub struct Replacement {
    /// Index into [`Aot::modules`].
    pub module: usize,
    pub export: String,
    pub abi: Abi,
    /// For messages: what it replaces.
    pub symbol: String,
    pub addr: u64,
    /// How many times it ran, over every thread and process of the run.
    pub calls: Arc<std::sync::atomic::AtomicU64>,
    /// A function of a replaced library the module does not provide: calling
    /// it stops the run, naming it (pewasm's aborting placeholder).
    pub missing: bool,
}

/// What an ejected project gives the runtime. See the module comment.
#[derive(Default)]
pub struct Aot {
    /// Image identity ([`image_identity`]) to the prebuilt module's wasm.
    pub images: HashMap<String, PathBuf>,
    /// For a dynamically linked program (by its image identity), the modules
    /// of the rest of its closure: the interpreter and the libraries, each
    /// built for the base `RunOptions::pins` places it at. Installed before
    /// the program starts.
    pub shared: HashMap<String, Vec<PathBuf>>,
    /// Replacement modules' wasm files.
    pub modules: Vec<PathBuf>,
    /// By id, the number the build put in each `Replace` block.
    pub replacements: Vec<Replacement>,
    /// The project, for messages.
    pub label: String,
    /// The system calls the program may make, by canonical number (as the
    /// session manifest counts them). Any other stops the run, naming it.
    /// `None` allows everything.
    pub syscalls: Option<std::collections::HashSet<u64>>,
    /// The recorded session never forked: a fork stops the run.
    pub single_process: bool,
    /// Replacement modules compiled for an engine. An `execve` builds a new
    /// engine, and a module belongs to the engine that compiled it.
    compiled: Mutex<Vec<(Engine, Arc<Vec<Module>>)>>,
}

impl std::fmt::Debug for Aot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Aot")
            .field("images", &self.images)
            .field("shared", &self.shared)
            .field("modules", &self.modules)
            .field("replacements", &self.replacements)
            .field("label", &self.label)
            .field("syscalls", &self.syscalls.as_ref().map(|s| s.len()))
            .field("single_process", &self.single_process)
            .finish()
    }
}

impl Aot {
    pub fn new(label: String) -> Aot {
        Aot {
            label,
            ..Aot::default()
        }
    }

    fn modules_for(&self, engine: &Engine) -> Result<Arc<Vec<Module>>> {
        let mut c = self.compiled.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, m)) = c.iter().find(|(e, _)| Engine::same(e, engine)) {
            return Ok(Arc::clone(m));
        }
        let mut out = Vec::new();
        for p in &self.modules {
            let bytes = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
            out.push(
                gex_cache::compile(engine, &bytes)
                    .with_context(|| format!("compiling replacement module {}", p.display()))?,
            );
        }
        let out = Arc::new(out);
        c.push((engine.clone(), Arc::clone(&out)));
        Ok(out)
    }
}

/// A content identity for a loaded image: its entry, mode and placed
/// segments. The same file loaded at the same base gives the same identity,
/// which is what a prebuilt module is only valid for.
pub fn image_identity(image: &Image) -> String {
    gex_key::image_identity(image)
}

/// The translation metadata a module carries in its `gex.meta` custom
/// section.
pub fn read_metadata(wasm: &[u8]) -> Option<gex_xlate::Metadata> {
    fn leb(b: &[u8], p: &mut usize) -> Option<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let byte = *b.get(*p)?;
            *p += 1;
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
    }
    if wasm.get(..8)? != b"\0asm\x01\0\0\0" {
        return None;
    }
    let mut p = 8;
    while p < wasm.len() {
        let id = wasm[p];
        p += 1;
        let size = leb(wasm, &mut p)? as usize;
        let end = p.checked_add(size)?;
        let body = wasm.get(p..end)?;
        if id == 0 {
            let mut q = 0;
            let n = leb(body, &mut q)? as usize;
            let name = body.get(q..q + n)?;
            if name == gex_emit::module::METADATA_SECTION.as_bytes() {
                return gex_xlate::Metadata::decode(&body[q + n..]);
            }
        }
        p = end;
    }
    None
}

/// The image module an ejected project built for `image`, compiled.
pub(crate) fn image_module(
    aot: &Aot,
    engine: &Engine,
    cache: Option<(&std::path::Path, &str)>,
    image: &Image,
) -> Result<(Module, gex_xlate::Metadata)> {
    let id = image_identity(image);
    let path = aot.images.get(&id).ok_or_else(|| {
        anyhow!(
            "gex-eject: {} has no translation of this program (entry {:#x}, {id}). \
             Only programs the recorded session ran are built into an ejected \
             project; record a session that runs it and re-eject",
            aot.label,
            image.entry
        )
    })?;
    load_module(engine, cache, path)
}

/// A prebuilt module and its metadata, compiled through the cache.
fn load_module(
    engine: &Engine,
    cache: Option<(&std::path::Path, &str)>,
    path: &std::path::Path,
) -> Result<(Module, gex_xlate::Metadata)> {
    let wasm = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let meta = read_metadata(&wasm)
        .ok_or_else(|| anyhow!("{} carries no gex metadata; rebuild the project", path.display()))?;
    let module = match cache {
        Some((dir, build)) => {
            let key = gex_cache::key(build, [b"eject-image".as_slice(), wasm.as_slice()]);
            let enc = meta.encode();
            gex_cache::load_or_compile(engine, dir, &key, move || Ok((wasm, enc)))
                .with_context(|| format!("compiling {}", path.display()))?
                .module
        }
        None => gex_cache::compile(engine, &wasm).with_context(|| format!("compiling {}", path.display()))?,
    };
    Ok((module, meta))
}

/// The rest of a dynamically linked program's closure, compiled; empty for
/// a static one.
pub(crate) fn shared_modules(
    aot: &Aot,
    engine: &Engine,
    cache: Option<(&std::path::Path, &str)>,
    image: &Image,
) -> Result<Vec<(Module, gex_xlate::Metadata)>> {
    let Some(paths) = aot.shared.get(&image_identity(image)) else {
        return Ok(Vec::new());
    };
    paths.iter().map(|p| load_module(engine, cache, p)).collect()
}

/// Install a program's prebuilt closure modules into the runtime's registry
/// and this store, as if runtime translation had produced them: threads and
/// forked children replay the registry, so they get them too.
pub(crate) fn install_shared(
    store: &mut wasmtime::Store<HostState>,
    units: Vec<(Module, gex_xlate::Metadata)>,
) -> Result<()> {
    let shared = std::sync::Arc::clone(
        &store
            .data()
            .jit
            .as_ref()
            .ok_or_else(|| anyhow!("an ejected closure needs the runtime's module registry"))?
            .shared,
    );
    for (module, meta) in units {
        let unit = Arc::new(super::CompiledUnit {
            module: Some(module),
            exported_blocks: Vec::new(),
            dir_entries: meta.dir_entries.clone(),
            table_slots: meta.table_slots,
            bridges: Vec::new(),
        });
        {
            let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
            sh.page_table.insert_entries(&meta.dir_entries);
            sh.registry.push(Arc::clone(&unit));
            crate::REGISTRY_LEN.store(sh.registry.len(), std::sync::atomic::Ordering::Release);
        }
        super::install_unit(&mut *store, &unit)?;
        store.data_mut().jit.as_mut().unwrap().replayed += 1;
    }
    Ok(())
}

/// Record the replacement region in the process's memory map, so the guest's
/// `mmap` never lands in it and `fork` copies it like any other mapping.
pub(crate) fn reserve_region(kernel: &mut gex_kernel::Kernel, aot: &Aot) {
    if aot.replacements.is_empty() {
        return;
    }
    kernel.mem.insert(
        REPL_BASE,
        REPL_END,
        gex_kernel::mem::prot::READ | gex_kernel::mem::prot::WRITE,
        "[gex-replace]",
    );
}

/// The host functions a replacement module may import from `gex`.
pub const HOST_IMPORTS: &[&str] = &[
    gex_abi::imports::SYSCALL,
    gex_abi::imports::SAFEPOINT,
    gex_abi::imports::SYSREG_READ,
    gex_abi::imports::SYSREG_WRITE,
    gex_abi::imports::CACHE_OP,
    gex_abi::imports::FMA,
    gex_abi::imports::ILLEGAL,
    gex_abi::imports::BREAKPOINT,
    gex_abi::imports::DISPATCH_MISS,
];

/// The guest's integer argument registers, as `x` indices in the context.
const INT_ARGS: &[u8] = if gex_abi::X86 {
    // rdi, rsi, rdx, rcx, r8, r9
    &[7, 6, 2, 1, 8, 9]
} else {
    &[0, 1, 2, 3, 4, 5, 6, 7]
};
/// Floating-point arguments: v0-v7, or xmm0-xmm7.
const FP_ARGS: u8 = 8;

/// Check that a replacement module can run here, and that it exports `name`
/// with a type `abi` can call. Returns the export's signature, for the build
/// report. Everything wrong is an error: a replacement that half-works is the
/// silent misbehaviour this exists to avoid.
pub fn check_replacement(wasm: &[u8], name: &str, abi: Abi) -> Result<String> {
    ReplacementModule::inspect(wasm)?.check(name, abi)
}

/// A replacement module, checked once: that it can run here, and what it
/// exports. [`ReplacementModule::check`] then checks each export a build
/// binds, without compiling the module again for each.
pub struct ReplacementModule {
    module: Module,
}

impl ReplacementModule {
    pub fn inspect(wasm: &[u8]) -> Result<ReplacementModule> {
        Self::inspect_for(wasm, true)
    }

    /// As [`Self::inspect`], for a host whose guest memory is `shared` or
    /// not: the browser's single-Worker shim has a plain one, and a module
    /// importing a shared memory will not link to it (nor the reverse).
    pub fn inspect_for(wasm: &[u8], shared: bool) -> Result<ReplacementModule> {
    let engine = Engine::new(&engine_config()).context("creating a wasm engine")?;
    let module = gex_cache::compile(&engine, wasm).context("not a valid wasm module for this runtime")?;
    let mut imports_memory = false;
    for imp in module.imports() {
        match (imp.module(), imp.name(), imp.ty()) {
            ("env", "memory", ExternType::Memory(m)) => {
                if m.is_shared() != shared {
                    bail!(
                        "it imports env.memory {}, and this host's guest memory is {} (the \
                         native runtime's is shared: link with --shared-memory; the browser's \
                         single-Worker shim's is not: link without it)",
                        if m.is_shared() { "shared" } else { "unshared" },
                        if shared { "shared" } else { "not" }
                    );
                }
                if m.maximum().unwrap_or(0) < gex_abi::GUEST_MEM_PAGES {
                    bail!(
                        "its env.memory maximum ({:?} pages) is below the guest's {} \
                         (link with --max-memory={})",
                        m.maximum(),
                        gex_abi::GUEST_MEM_PAGES,
                        gex_abi::GUEST_MEM_BYTES
                    );
                }
                imports_memory = true;
            }
            // The host functions translated code calls (`gex_abi.h`): what
            // the function tier's C, and so a decompiled function, imports.
            // Bound to the very ones the guest's own modules get, the syscall
            // allow-list included.
            ("gex", n, ExternType::Func(_)) if HOST_IMPORTS.contains(&n) => {}
            (m, n, _) => bail!(
                "it imports {m}.{n}, which gex does not provide: a replacement module may \
                 import env.memory (the guest's memory) and the gex.* host functions \
                 ({})",
                HOST_IMPORTS.join(", ")
            ),
        }
    }
    let global = |g: &str| {
        module.exports().find_map(|e| match e.ty() {
            ExternType::Global(t) if e.name() == g => Some(t),
            _ => None,
        })
    };
    if imports_memory {
        match global("__stack_pointer") {
            Some(t) if t.mutability() == Mutability::Var && matches!(t.content(), ValType::I32) => {}
            _ => bail!(
                "it uses the guest's memory but does not export a mutable i32 __stack_pointer \
                 (link with --export=__stack_pointer): each guest thread needs a stack of its own"
            ),
        }
    }
        Ok(ReplacementModule { module })
    }

    /// Whether the module exports a function called `name`.
    pub fn has(&self, name: &str) -> bool {
        self.module.exports().any(|e| e.name() == name && matches!(e.ty(), ExternType::Func(_)))
    }

    /// Check that `name` is an export `abi` can call; its signature.
    pub fn check(&self, name: &str, abi: Abi) -> Result<String> {
    let module = &self.module;
    let ty = module
        .exports()
        .find(|e| e.name() == name)
        .ok_or_else(|| anyhow!("it exports no `{name}`"))?
        .ty();
    let ExternType::Func(ft) = ty else {
        bail!("its export `{name}` is not a function");
    };
    let show = |t: &ValType| match t {
        ValType::I32 => "i32",
        ValType::I64 => "i64",
        ValType::F32 => "f32",
        ValType::F64 => "f64",
        _ => "?",
    };
    let sig = format!(
        "({}) -> ({})",
        ft.params().map(|t| show(&t)).collect::<Vec<_>>().join(", "),
        ft.results().map(|t| show(&t)).collect::<Vec<_>>().join(", ")
    );
    match abi {
        Abi::Ctx | Abi::Entry => {
            if !(ft.params().len() == 1
                && matches!(ft.params().next(), Some(ValType::I32))
                && ft.results().len() == 0)
            {
                bail!("`{name}` is {sig}; a \"{}\" replacement is (i32 ctx) -> ()", abi.name());
            }
        }
        Abi::C => {
            let (mut ints, mut fps) = (0usize, 0u8);
            for p in ft.params() {
                match p {
                    ValType::I32 | ValType::I64 => ints += 1,
                    ValType::F32 | ValType::F64 => fps += 1,
                    _ => bail!("`{name}` is {sig}; a \"c\" replacement takes i32/i64/f32/f64"),
                }
            }
            if ints > INT_ARGS.len() || fps > FP_ARGS {
                bail!(
                    "`{name}` is {sig}: more arguments than the guest passes in registers \
                     ({} integer, {FP_ARGS} floating-point); use the \"ctx\" ABI and read the \
                     rest from the guest stack",
                    INT_ARGS.len()
                );
            }
            if ft.results().len() > 1
                || ft.results().any(|t| !matches!(t, ValType::I32 | ValType::I64 | ValType::F32 | ValType::F64))
            {
                bail!("`{name}` is {sig}; a \"c\" replacement returns at most one i32/i64/f32/f64");
            }
        }
    }
    Ok(sig)
    }
}

/// The engine features translated code and replacements both need.
fn engine_config() -> wasmtime::Config {
    let mut config = wasmtime::Config::new();
    config.wasm_tail_call(true);
    config.wasm_threads(true);
    config.wasm_bulk_memory(true);
    config.wasm_multi_memory(true);
    config
}

/// Replacement instances in one store (one guest thread).
pub(crate) struct StoreRepl {
    /// By replacement id; `None` for a missing one.
    funcs: Vec<Option<Func>>,
}

/// The stack top for replacement code on the thread whose context is at
/// `ctx_addr`.
fn stack_top(ctx_addr: u64) -> Result<u64> {
    let stride = (ctx::SIZE as u64 + 63) & !63;
    let slot = ctx_addr.saturating_sub(gex_abi::RESERVED_BASE) / stride;
    if slot >= REPL_THREADS {
        bail!("guest thread slot {slot} has no replacement stack: the region holds {REPL_THREADS} threads");
    }
    Ok(REPL_BASE + REPL_DATA_BYTES + (slot + 1) * REPL_STACK_BYTES)
}

/// Instantiate the project's replacement modules in this store.
fn instantiate(caller: &mut Caller<'_, HostState>, aot: &Aot) -> Result<StoreRepl> {
    let engine = caller.engine().clone();
    let modules = aot.modules_for(&engine)?;
    let memory = caller
        .data()
        .memory
        .clone()
        .ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let top = stack_top(caller.data().thread.ctx_addr)?;
    let mut instances = Vec::new();
    for (i, m) in modules.iter().enumerate() {
        let mut linker: Linker<HostState> = Linker::new(&engine);
        // The gex.* host functions, the same ones translated code imports.
        super::register_imports(&mut linker)?;
        linker.define(&mut *caller, "env", "memory", memory.shared().clone())?;
        let inst = linker
            .instantiate(&mut *caller, m)
            .with_context(|| format!("instantiating {}", aot.modules[i].display()))?;
        if let Some(sp) = inst.get_global(&mut *caller, "__stack_pointer") {
            sp.set(&mut *caller, Val::I32(top as u32 as i32))
                .context("setting a replacement module's stack pointer")?;
        }
        instances.push(inst);
    }
    let mut funcs = Vec::new();
    for r in &aot.replacements {
        if r.missing {
            funcs.push(None);
            continue;
        }
        let f = instances[r.module]
            .get_func(&mut *caller, &r.export)
            .ok_or_else(|| anyhow!("{} exports no `{}`", aot.modules[r.module].display(), r.export))?;
        funcs.push(Some(f));
    }
    Ok(StoreRepl { funcs })
}

/// `sysreg_write(pc, REPLACE_SYSREG, id)`: run replacement `id` for the
/// thread whose context is at `ctx`.
pub(crate) fn call(caller: &mut Caller<'_, HostState>, ctx_addr: i32, pc: u64, id: u64) -> Result<()> {
    let aot = caller
        .data()
        .spawn
        .as_ref()
        .and_then(|s| s.opts.aot.clone())
        .ok_or_else(|| {
            anyhow!("replacement #{id} at {pc:#x} reached, but this run has no ejected project")
        })?;
    let r = aot
        .replacements
        .get(id as usize)
        .ok_or_else(|| anyhow!("replacement #{id} at {pc:#x} is not in {}", aot.label))?;
    r.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if r.missing {
        caller.data_mut().exit = Some(super::Exit::Signal {
            signal: super::signal::SIGSEGV,
            pc,
            detail: format!(
                "gex-eject: {} (at {pc:#x}) belongs to a library {} replaced, and the \
                 replacement module has no `{}`: add it and rebuild",
                r.symbol, aot.label, r.export
            ),
            internal: true,
        });
        return Err(super::StopExecution.into());
    }
    if caller.data().repl.is_none() {
        let s = instantiate(caller, &aot)?;
        caller.data_mut().repl = Some(s);
    }
    let func = caller.data().repl.as_ref().unwrap().funcs[id as usize]
        .ok_or_else(|| anyhow!("internal: replacement #{id} has no function"))?;
    let memory = caller
        .data()
        .memory
        .clone()
        .ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let base = ctx_addr as u32 as usize;
    match r.abi {
        // A host call in it that stops the guest (an exit, a signal, a
        // fork) unwinds through it as a `StopExecution`, which must reach the
        // run loop as itself: the replacement left the context architectural
        // before the call, as translated code does.
        Abi::Ctx | Abi::Entry => {
            if let Err(e) = func.call(&mut *caller, &[Val::I32(ctx_addr)], &mut []) {
                if e.is::<super::StopExecution>() {
                    return Err(e);
                }
                return Err(e.context(format!("replacement `{}` for {}", r.export, r.symbol)));
            }
        }
        Abi::C => {
            let ty = func.ty(&*caller);
            let (mut ni, mut nf) = (0usize, 0u8);
            let mut args = Vec::new();
            {
                let data = memory.bytes();
                for p in ty.params() {
                    let v = match p {
                        ValType::I32 | ValType::I64 => {
                            let x = read_u64(data, base + ctx::x(INT_ARGS[ni]));
                            ni += 1;
                            if matches!(p, ValType::I32) {
                                Val::I32(x as u32 as i32)
                            } else {
                                Val::I64(x as i64)
                            }
                        }
                        _ => {
                            let x = read_u64(data, base + ctx::v(nf));
                            nf += 1;
                            if matches!(p, ValType::F32) {
                                Val::F32(x as u32)
                            } else {
                                Val::F64(x)
                            }
                        }
                    };
                    args.push(v);
                }
            }
            let mut results = vec![Val::I32(0); ty.results().len()];
            func.call(&mut *caller, &args, &mut results)
                .with_context(|| format!("replacement `{}` for {}", r.export, r.symbol))?;
            let data = memory.bytes();
            match results.first() {
                // A wasm32 `int` or pointer: zero-extended, which is what a
                // pointer needs and all a caller of an `int` function reads.
                Some(Val::I32(v)) => write_u64(data, base + ctx::x(0), *v as u32 as u64),
                Some(Val::I64(v)) => write_u64(data, base + ctx::x(0), *v as u64),
                Some(Val::F32(v)) => {
                    write_u64(data, base + ctx::v(0), u64::from(*v));
                    write_u64(data, base + ctx::v(0) + 8, 0);
                }
                Some(Val::F64(v)) => {
                    write_u64(data, base + ctx::v(0), *v);
                    write_u64(data, base + ctx::v(0) + 8, 0);
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// The system-call allow-list: `Some` when the call about to be made is not
/// on it, having stopped the run.
pub(crate) fn check_syscall(caller: &mut Caller<'_, HostState>, pc: u64) -> Option<Result<()>> {
    let aot = caller.data().spawn.as_ref().and_then(|s| s.opts.aot.clone())?;
    let allowed = aot.syscalls.as_ref()?;
    let memory = caller.data().memory.clone()?;
    let ctx_addr = caller.data().thread.ctx_addr as usize;
    let raw = read_u64(memory.bytes(), ctx_addr + ctx::SYSCALL_NR);
    let nr = if gex_abi::X86 { gex_kernel::x86::canonical_number(raw) } else { raw };
    if allowed.contains(&nr) {
        return None;
    }
    super::profile::use_kernel_syscall_names();
    caller.data_mut().exit = Some(super::Exit::Signal {
        signal: super::signal::SIGSYS,
        pc,
        detail: format!(
            "gex-eject: system call {} ({nr}) was never made by the session {} was ejected \
             from, and its runtime allows only those. Record a session that makes it, or add \
             it to \"allow_syscalls\" in user/eject.json",
            gex_xlate::profile::syscall_name(nr),
            aot.label
        ),
        internal: true,
    });
    Some(Err(super::StopExecution.into()))
}

/// The guest's own system-call numbers that stand for the canonical ones in
/// `allowed`: what a host that sees raw numbers (the browser's) checks.
pub fn raw_syscalls(allowed: &std::collections::HashSet<u64>) -> Vec<u64> {
    if !gex_abi::X86 {
        let mut v: Vec<u64> = allowed.iter().copied().collect();
        v.sort_unstable();
        return v;
    }
    (0..1024u64).filter(|raw| allowed.contains(&gex_kernel::x86::canonical_number(*raw))).collect()
}

/// Why a dispatch miss happened, when an ejected project without a JIT
/// fallback is running and the address is code: nobody saw it when the
/// project was built. `None` otherwise (a wild branch stays a SIGSEGV).
pub(crate) fn unseen_code(caller: &mut Caller<'_, HostState>, pc: u64) -> Option<String> {
    let aot = caller.data().spawn.as_ref().and_then(|s| {
        (!s.opts.jit).then(|| s.opts.aot.clone()).flatten()
    })?;
    let name = caller.data_mut().with_kernel(|k| {
        k.mem
            .vmas()
            .iter()
            .find(|v| v.start <= pc && pc < v.end && v.prot & gex_kernel::mem::prot::EXEC != 0)
            .map(|v| (v.name.clone(), pc - v.start))
    })?;
    let place = match name {
        // gex's loader names the program's (and interpreter's) segments so.
        (n, _) if n == "[image]" => Some("in the program's image".to_string()),
        (n, off) if !n.is_empty() && !n.starts_with('[') => Some(format!("{n} + {off:#x}")),
        _ => None,
    };
    Some(match place {
        Some(place) => format!(
            "gex-eject: code at {pc:#x} ({place}) was not translated when {} was \
             built: the recorded session never reached it and discovery did not find it. \
             Record a session that reaches it and re-eject, set \"coverage\" to \"static\" or \"exhaustive\" \
             in user/eject.json, or run with --jit-fallback",
            aot.label
        ),
        None => format!(
            "gex-eject: code at {pc:#x} is in an anonymous mapping: code the program made \
             at run time, which {} cannot have built. It runs only with --jit-fallback",
            aot.label
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(func (export "add") (param i64 i64) (result i64) ...)`
    const ADD: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x07, 0x01, 0x60, 0x02, 0x7e, 0x7e,
        0x01, 0x7e, 0x03, 0x02, 0x01, 0x00, 0x07, 0x07, 0x01, 0x03, b'a', b'd', b'd', 0x00, 0x00,
        0x0a, 0x09, 0x01, 0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x7c, 0x0b,
    ];
    /// `(import "wasi" "x" (func))`: something gex does not provide.
    const IMPORTS_WASI: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x04, 0x01, 0x60, 0x00, 0x00, 0x02,
        0x0a, 0x01, 0x04, b'w', b'a', b's', b'i', 0x01, b'x', 0x00, 0x00,
    ];

    #[test]
    fn checks_a_replacement_modules_shape() {
        assert_eq!(check_replacement(ADD, "add", Abi::C).unwrap(), "(i64, i64) -> (i64)");
        assert!(check_replacement(ADD, "add", Abi::Ctx).is_err());
        assert!(check_replacement(ADD, "sub", Abi::C).is_err());
        let e = format!("{:#}", check_replacement(IMPORTS_WASI, "x", Abi::C).unwrap_err());
        assert!(e.contains("wasi.x"), "{e}");
    }

    #[test]
    fn reads_the_metadata_a_translation_carries() {
        let m = gex_xlate::seed_module(true);
        let meta = read_metadata(&m.wasm).expect("a gex module carries gex.meta");
        assert_eq!(meta.next_slot, m.metadata().next_slot);
        assert!(read_metadata(ADD).is_none());
    }
}
