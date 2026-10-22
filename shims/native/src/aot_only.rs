//! The AOT-only runtime (docs/eject-pe.md §3.1): what an ejected project
//! runs by default. No guest code is translated at run time -- no tier-1, no
//! function JIT, no interpreter. Every guest function that runs is a wasm
//! function some module built ahead of time installed in the function table
//! (`ftab`): a unit, a region entry or a provider.
//!
//! The mode is made by construction, not by checks along the translator's
//! paths. With it on, a process:
//!
//! * has no tier-1 module. Its base module ([`base_module`]) is a few
//!   instructions: `enter(ctx, pc)` stores the pc and calls the function
//!   table runtime's `root` (§9.8.1), bound per store by [`bind_root`]; its
//!   dispatch table is empty and nothing ever fills it;
//! * installs no run-time translator (`gex_kernel::ftab::set_translator` is
//!   never called), so `gex.lookup` serves only what is installed and
//!   answers anything else with index 1, the fallback;
//! * has [`missing`] at index 1, and as `gex.dispatch_miss`: a call to code
//!   nothing built ahead of time stops the run there, naming the address;
//! * runs `gex.call_out` as every native wasm32 process does (native Wine
//!   starts its threads' guest code through it), translating nothing for
//!   it. Its dispatch tables being empty, no slot can watch a return: one
//!   that reaches the root is a miss and stops the run. A Windows
//!   process's units call it on their leave path only, and are answered
//!   "leave": the root then looks the address up, and a miss stops the run.
//!
//! E7 (a runtime without the translator in it) deletes the code this mode
//! never reaches.

use super::*;
use std::sync::OnceLock;
use wasm_encoder::{
    CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection, ImportSection, Instruction as I,
    MemArg, Module as WModule, RefType, TableSection, TableType, TypeSection, ValType,
};

static ON: OnceLock<()> = OnceLock::new();

/// Turn the mode on for this host process, before anything runs.
pub fn enable() {
    let _ = ON.set(());
}

/// Whether this process runs ahead-of-time code only.
pub fn enabled() -> bool {
    ON.get().is_some()
}

/// The private table `enter` reaches `root` through (one slot).
const ROOT_TABLE: &str = "gex_aot_root";

/// The base module of an AOT-only process: imports the guest memory,
/// exports an empty dispatch table 0 (the function table runtime imports
/// one), the one-slot table [`bind_root`] fills, and
/// `enter(ctx: i32, pc: i64)`: `ctx.PC = pc; root(ctx)`.
pub(crate) fn base_module() -> Vec<u8> {
    let mut m = WModule::new();
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32, ValType::I64], []); // 0: enter
    types.ty().function([ValType::I32], []); // 1: root
    m.section(&types);
    let mut imports = ImportSection::new();
    imports.import(IMPORT_MODULE, exports::MEMORY, EntityType::Memory(gex_cgen::runtime::memory_type()));
    m.section(&imports);
    let mut funcs = FunctionSection::new();
    funcs.function(0);
    m.section(&funcs);
    let mut tables = TableSection::new();
    let t = |min: u64| TableType { element_type: RefType::FUNCREF, minimum: min, maximum: None, table64: false, shared: false };
    tables.table(t(0)); // 0: dispatch, never filled
    tables.table(t(1)); // 1: root
    m.section(&tables);
    let mut ex = ExportSection::new();
    ex.export(exports::dispatch_table(0), ExportKind::Table, 0);
    ex.export(ROOT_TABLE, ExportKind::Table, 1);
    ex.export(exports::ENTER, ExportKind::Func, 0);
    m.section(&ex);
    let mut code = CodeSection::new();
    let mut f = Function::new([]);
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::I64Store(MemArg { offset: ctx::PC as u64, align: 3, memory_index: 0 }));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::I32Const(0));
    f.instruction(&I::CallIndirect { type_index: 1, table_index: 1 });
    f.instruction(&I::End);
    code.function(&f);
    m.section(&code);
    m.finish()
}

/// Compile the modules every AOT-only process makes at run time -- the
/// function table's runtime module and the base module -- into the
/// content-addressed precompiled directory (`gex_cache::precompiled_dir`), for
/// a runtime without a compiler (gex-rt).
#[cfg(feature = "compile")]
pub fn precompile(dir: &std::path::Path) -> Result<()> {
    let engine = crate::engine()?;
    gex_cache::precompile(&engine, &gex_cgen::runtime::module(), dir)?;
    gex_cache::precompile(&engine, &base_module(), dir)?;
    Ok(())
}

/// Point `instance`'s (the base module's) `enter` at this store's `root`,
/// making the store's function table first. Once per store, before the
/// first `enter`.
pub(crate) fn bind_root(store: &mut Store<HostState>, instance: &wasmtime::Instance) -> Result<()> {
    ftab::ensure(&mut *store)?;
    ftab::catch_up(&mut *store)?;
    let root = ftab::root_of(store).ok_or_else(|| anyhow!("the function table's runtime is not set up"))?;
    let t = instance
        .get_table(&mut *store, ROOT_TABLE)
        .ok_or_else(|| anyhow!("the AOT-only base module exports no {ROOT_TABLE}"))?;
    t.set(&mut *store, 0, Ref::Func(Some(root)))?;
    Ok(())
}

/// `gex.dispatch_miss` and `gex.call_out` as an AOT-only process has them.
pub(crate) fn override_imports(linker: &mut Linker<HostState>) -> Result<()> {
    linker.allow_shadowing(true);
    linker.func_wrap(
        IMPORT_MODULE,
        imports::DISPATCH_MISS,
        |mut caller: Caller<'_, HostState>, _ctx: i32, pc: i64| -> wasmtime::Result<i32> { crate::host((|| -> anyhow::Result<i32> { Err(missing(&mut caller, pc as u64, "a dispatch")) })()) },
    )?;
    linker.func_wrap(
        IMPORT_MODULE,
        imports::CALL_OUT,
        |mut caller: Caller<'_, HostState>, ctx: i32, target: i64, ret: i64, sp: i64| -> wasmtime::Result<i32> {
            if caller.data().nt.is_some() {
                return crate::host((|| -> anyhow::Result<i32> { Ok(0) })());
            }
            crate::host(super::call_out(&mut caller, ctx, target as u64, ret as u64, sp as u64, gex_abi::ftab::NEED_ALL))
        },
    )?;
    Ok(())
}

/// Index 1 of an AOT-only process's function table: the call that reached
/// it named an address no installed function serves.
pub(crate) fn fallback(caller: &mut Caller<'_, HostState>, ctx: i32, depth: i32, need: i32, ret: u64) -> Result<i32> {
    let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let target = GuestMem::new(memory.bytes()).read_u64(ctx as u32 as u64 + ctx::LAST_TARGET as u64).unwrap_or(0);
    // The awaited return itself, reached with the stack moved: leave, and
    // the caller's return check resumes there (as `ftab::fallback` does).
    if target == ret {
        return Ok(gex_abi::ftab::RET_LEFT);
    }
    // A call-out's return (native Wine's return pad, which no function
    // holds), reached through the table rather than the dispatch slot that
    // watches it: that call-out ends here, as the slot would end it.
    let sp = GuestMem::new(memory.bytes()).read_u64(ctx as u32 as u64 + ctx::x(sp_index()) as u64).unwrap_or(0);
    if let Some(i) = caller.data().callouts.iter().rposition(|co| co.ret == target && sp >= co.sp_want && sp <= co.sp_want + co.span) {
        let _ = GuestMem::new(memory.bytes()).write_u64(ctx as u32 as u64 + ctx::PC as u64, target);
        caller.data_mut().callout_signal = Some(CalloutSignal::Return(i));
        return Err(StopExecution.into());
    }
    // An exception dispatched nested whose handler jumps back above it:
    // not a call into code, the frames below resume there.
    if nt::exc_escape(caller, target) {
        return Err(StopExecution.into());
    }
    // A branch to memory that is not executable is the guest's own fault
    // (DEP: an access violation, ExceptionInformation[0] = 8), delivered as
    // the translator's miss path delivers it. Not a gap in the build.
    if !caller.data_mut().with_kernel(|k| k.mem.lock().check_access(target, 1, prot::EXEC)) {
        if caller.data().nt.is_some() {
            return nt::fault(caller, nt::Fault::NotExecutable, target).map(|_| 0);
        }
        return crate::branch_not_executable(caller, target);
    }
    // A window-procedure thunk made at run time (gex_kernel::win::thunk):
    // its store done here, then the call goes on to where it jumps.
    if gex_kernel::compat32() {
        let next = gex_kernel::win::thunk::run(&mut GuestMem::new(memory.bytes()), ctx as u32 as u64, target);
        if let Some(next) = next {
            let _ = GuestMem::new(memory.bytes()).write_u64(ctx as u32 as u64 + ctx::LAST_TARGET as u64, next);
            let idx = crate::ftab::lookup(caller, next)?;
            let f = crate::ftab::table_func(caller, idx as u32)?;
            let f = f.typed::<(i32, i32, i32, i64), i32>(&*caller).context("a function table entry of another type")?;
            return Ok(f.call(&mut *caller, (ctx, depth, need, ret as i64))?);
        }
    }
    Err(missing(caller, target, "a call or jump"))
}

/// Stop the run: `addr` has no ahead-of-time code.
pub(crate) fn missing(caller: &mut Caller<'_, HostState>, addr: u64, how: &str) -> anyhow::Error {
    let what = match caller.data().nt.as_ref() {
        Some(p) => p.describe_addr(addr),
        None => describe(caller, addr),
    };
    let exec = caller.data_mut().with_kernel(|k| k.mem.lock().check_access(addr, 1, prot::EXEC));
    let why = if !exec {
        "which is not executable"
    } else if !what.contains('(') {
        "outside every image: code generated at run time, which an ejected project cannot run (replace its generator)"
    } else {
        "which nothing built ahead of time holds (the recorded session never ran it and recovery did not find it, \
         or it was rewritten at run time)"
    };
    let detail = format!(
        "gex-eject: {how} reached {what}, {why}. Record a session that reaches it and re-eject, add it to \
         \"seeds\" in user/eject.json, or run with --jit-fallback"
    );
    stop(caller, addr, &detail)
}

/// `addr` as `name+offset` in the mapping (file) it is in, from the
/// kernel's map: the offset from the lowest of the adjacent mappings with
/// that name, which for an image is its base.
fn describe(caller: &mut Caller<'_, HostState>, addr: u64) -> String {
    let named = caller.data_mut().with_kernel(|k| {
        let mem = k.mem.lock();
        let vmas = mem.vmas();
        let i = vmas.iter().position(|v| v.start <= addr && addr < v.end)?;
        let name = vmas[i].name.clone();
        if name.is_empty() {
            return None;
        }
        let mut base = vmas[i].start;
        for v in vmas[..i].iter().rev() {
            if v.end != base || v.name != name {
                break;
            }
            base = v.start;
        }
        Some((name, base))
    });
    match named {
        Some((name, base)) => {
            let short = name.rsplit(['/', '\\']).next().unwrap_or(&name);
            format!("{addr:#x} ({short}+{:#x})", addr - base)
        }
        None => format!("{addr:#x}"),
    }
}

/// Stop the run with `detail`, from any thread (an ejected project's loud
/// failure).
pub(crate) fn stop(caller: &mut Caller<'_, HostState>, addr: u64, detail: &str) -> anyhow::Error {
    let detail = detail.to_string();
    eprintln!("{detail}");
    // Another thread's end would go unnoticed (its store's exit is its
    // own): the whole process stops, loudly, as for any failed thread.
    if caller.data().thread.ctx_addr != gex_abi::ctx_addr(0) {
        eprintln!("gexc: gex failed at guest pc={addr:#x} on thread {}: {detail}", caller.data().thread.tid);
        std::process::exit(134);
    }
    let state = caller.data_mut();
    if state.exit.is_none() {
        state.exit = Some(Exit::Signal { signal: signal::SIGSEGV, pc: addr, detail: detail.clone(), internal: true });
    }
    StopExecution.into()
}
