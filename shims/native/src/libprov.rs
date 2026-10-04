//! Stateful export providers (docs/eject-pe.md §5.1): a library of native
//! wasm in place of a guest DLL's exports, with data, a C stack and a heap of
//! its own -- what gex-cgen's built-in providers (no data, no stack) cannot
//! be. An ejected Noita's Lua (PUC Lua 5.1 for LuaJIT's lua51.dll) is one.
//!
//! The module is a position-independent wasm module (`wasm-ld
//! --experimental-pic -shared --shared-memory`), bound like any provider:
//! `GEX_PROVIDE(fn, "export")` puts an `F_body` in its element segment, and
//! when the library maps, each is installed in the function table at its
//! export's guest address. What differs:
//!
//! * **The entry point.** A `gexp__entry` replaces the image's DllMain.
//! * **Data.** Its `dylink.0` memory size is allocated from the NT
//!   allocator when the library maps, so the data sits at a guest address
//!   below 2 GiB and every pointer it hands the guest is a guest pointer.
//!   Every store instantiates the module with `__memory_base` there; the
//!   module's own `__wasm_init_memory` initialises the data once (wasm-ld
//!   guards it with an atomic flag in the data).
//! * **Stack.** Each guest thread gets one stack for all its stateful
//!   providers (`__stack_pointer` is a global of the store, shared by their
//!   instances, as a C thread has one stack), allocated at the first
//!   instantiation in that thread.
//! * **Heap.** `gex.provider_alloc(size)` hands out fresh read-write guest
//!   pages from the NT allocator; the module's malloc grows over them.
//! * **Non-local exits.** `gex.provider_try(fn, arg)` calls the module's
//!   `void fn(int)` (a table index) from the host and returns 0, or the code
//!   a `gex.provider_throw(code)` made inside it: every wasm frame between
//!   is discarded, the guest's included, as a C `longjmp` would discard
//!   them. (wasmtime 27 has no wasm exception handling; with it these become
//!   `try_table` and `throw`.) It is what Lua's `luaD_rawrunprotected` uses.

use super::*;
use std::path::PathBuf;
use std::sync::OnceLock;

/// One stateful provider.
#[derive(Clone, Debug)]
pub struct LibProvider {
    /// For messages.
    pub name: String,
    /// The libraries it serves, lower case (`lua51.dll`).
    pub dlls: Vec<String>,
    /// The linked module. Its export functions are found in its element
    /// segment by name: `gexp_<export>` serves `<export>`; every other
    /// export of the library is bound to `gexp__missing` (which calls
    /// `gex.provider_missing`), so no code of the replaced library runs.
    pub wasm: PathBuf,
}

/// Exports bound to `gexp__missing`, by guest address: for the message.
static MISSING: Mutex<Option<std::collections::HashMap<u64, String>>> = Mutex::new(None);

static LIBS: OnceLock<Vec<LibProvider>> = OnceLock::new();

/// Set this process's stateful providers, before the program starts.
pub fn set(v: Vec<LibProvider>) {
    let _ = LIBS.set(v);
}

pub(crate) fn for_dll(dll: &str) -> Vec<&'static LibProvider> {
    let dll = dll.to_ascii_lowercase();
    LIBS.get().map(|v| v.iter().filter(|p| p.dlls.contains(&dll)).collect()).unwrap_or_default()
}

/// What every store instantiates a stateful provider with.
#[derive(Clone, Copy, Debug)]
pub struct Extras {
    /// A guest address: above 4 GiB is possible in a 64-bit guest memory.
    pub memory_base: u64,
}

/// Bytes of C stack each guest thread gets for its stateful providers.
const STACK: u64 = 1 << 20;

/// The `dylink.0` memory size and alignment (log2) of a PIC module.
pub fn dylink_memory(wasm: &[u8]) -> Result<(u32, u32)> {
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let wasmparser::Payload::CustomSection(c) = payload? {
            if c.name() != "dylink.0" {
                continue;
            }
            let mut r = wasmparser::BinaryReader::new(c.data(), 0);
            while !r.eof() {
                let kind = r.read_u8()?;
                let len = r.read_var_u32()? as usize;
                let start = r.original_position();
                if kind == 1 {
                    // WASM_DYLINK_MEM_INFO: memsize, memalign, tablesize, tablealign.
                    let size = r.read_var_u32()?;
                    let align = r.read_var_u32()?;
                    return Ok((size, align));
                }
                r.read_bytes(len - (r.original_position() - start))?;
            }
        }
    }
    bail!("not a position-independent module (no dylink.0 memory info)")
}

/// This store's provider stack pointer, made (with its stack) on first use.
pub(crate) fn stack(cx: &mut wasmtime::StoreContextMut<'_, HostState>) -> Result<wasmtime::Global> {
    if let Some(g) = cx.data().jit.as_ref().and_then(|j| j.ftab.as_ref()).and_then(|s| s.provider_sp.clone()) {
        return Ok(g);
    }
    let tid = cx.data().thread.tid;
    // Top down when only built-in providers are stateful, so the first call
    // (a DllMain's malloc) does not move the DLLs mapped after it; an
    // ejected project's sessions were recorded with it bottom-up.
    let name = format!("[provider stack, thread {tid}]");
    let base = if LIBS.get().map_or(true, |v| v.is_empty()) { alloc_top(cx.data_mut(), STACK, &name)? } else { alloc(cx.data_mut(), STACK, &name)? };
    let top = base + STACK - 16;
    // Pointer-sized: i64 for a wasm64 provider beside a 64-bit guest memory.
    let g = if gex_abi::mem64() {
        wasmtime::Global::new(&mut *cx, wasmtime::GlobalType::new(wasmtime::ValType::I64, wasmtime::Mutability::Var), Val::I64(top as i64))?
    } else {
        wasmtime::Global::new(&mut *cx, wasmtime::GlobalType::new(wasmtime::ValType::I32, wasmtime::Mutability::Var), Val::I32(top as i32))?
    };
    if let Some(s) = cx.data_mut().jit.as_mut().and_then(|j| j.ftab.as_mut()) {
        s.provider_sp = Some(g.clone());
    }
    Ok(g)
}

/// Guest pages from the NT allocator (a Windows process; there is no other
/// kind of stateful provider yet).
pub(crate) fn alloc(state: &mut HostState, size: u64, name: &str) -> Result<u64> {
    let Some(p) = state.nt.clone() else { return alloc_linux(state, size, name) };
    state.with_kernel(|k| gex_kernel::nt::boot::alloc_private(&p, k, size, name)).map_err(|e| anyhow!(e))
}

/// Guest pages in a Linux process (a native wasm32 program, `wasmprog`):
/// an anonymous private mapping, as the program's own `mmap` would get.
fn alloc_linux(state: &mut HostState, size: u64, name: &str) -> Result<u64> {
    use gex_kernel::mem::prot;
    let len = (size + 0xfff) & !0xfff;
    state
        .with_kernel(|k| k.mem.mmap(0, len, prot::READ | prot::WRITE, gex_kernel::mem::flags::PRIVATE | gex_kernel::mem::flags::ANONYMOUS, name))
        .map_err(|e| anyhow!("mapping {name}: errno {}", e.0))
}

/// A page for a guest code address a provider binds (a thread entry): low,
/// where code lives in a 64-bit guest memory (docs/mem64.md), and read-only.
fn alloc_code(state: &mut HostState, name: &str) -> Result<u64> {
    if state.nt.is_some() {
        return alloc(state, 0x1000, name);
    }
    use gex_kernel::mem::prot;
    state
        .with_kernel(|k| k.mem.mmap_low(0, 0x1000, prot::READ, gex_kernel::mem::flags::PRIVATE | gex_kernel::mem::flags::ANONYMOUS, name))
        .map_err(|e| anyhow!("mapping {name}: errno {}", e.0))
}

/// [`alloc`] from the top down (`boot::alloc_private_top`): a built-in
/// provider's data and heap, which must not move the program's later DLLs.
pub(crate) fn alloc_top(state: &mut HostState, size: u64, name: &str) -> Result<u64> {
    let Some(p) = state.nt.clone() else { return alloc_linux(state, size, name) };
    state.with_kernel(|k| gex_kernel::nt::boot::alloc_private_top(&p, k, size, name)).map_err(|e| anyhow!(e))
}

/// Install `p`'s functions for an image whose exports are `exports` at
/// `base`: its data allocated, the module published with it.
pub(crate) fn install(
    p: &LibProvider,
    state: &mut HostState,
    shared: &Arc<Mutex<JitShared>>,
    engine: &Engine,
    memory: &gex_mem::GuestMemory,
    exports: &std::collections::HashMap<String, u32>,
    base: u64,
    entry_rva: u32,
) -> Result<usize> {
    let wasm = std::fs::read(&p.wasm).with_context(|| format!("reading provider {} ({})", p.name, p.wasm.display()))?;
    let slots = gex_cgen::provider::slot_names(&wasm)?;
    let missing = slots
        .iter()
        .position(|s| s.as_deref() == Some("gexp__missing"))
        .ok_or_else(|| anyhow!("provider {}: no gexp__missing in its module's table", p.name))? as u32;
    let mut provided: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for (k, s) in slots.iter().enumerate() {
        if let Some(e) = s.as_deref().and_then(|s| s.strip_prefix("gexp_")).filter(|e| !e.starts_with('_') && !e.starts_with("ptr_")) {
            provided.insert(e, k as u32);
        }
    }
    let mut pairs = Vec::new();
    let mut unprovided = Vec::new();
    for (e, &rva) in exports {
        let addr = base + rva as u64;
        match provided.get(e.as_str()) {
            Some(&k) => pairs.push((addr, k)),
            None => {
                pairs.push((addr, missing));
                unprovided.push((addr, e.clone()));
            }
        }
    }
    // `gexp__entry`, when there is one, replaces the image's entry point
    // (DllMain): the library it would set up does not run.
    if let (Some(k), true) = (slots.iter().position(|s| s.as_deref() == Some("gexp__entry")), entry_rva != 0) {
        pairs.push((base + entry_rva as u64, k as u32));
    }
    // A provider's own threads (musl's clone, wasm64): they start at a
    // guest address of their own, bound to `__gex_thread_start`.
    if let Some(k) = slots.iter().position(|s| s.as_deref() == Some("__gex_thread_start")) {
        // Below 4 GiB, like all code: the call cache keys on 32-bit targets.
        let at = alloc_code(state, &format!("[provider {} thread entry]", p.name))?;
        pairs.push((at, k as u32));
        THREAD_ENTRY.store(at, std::sync::atomic::Ordering::Release);
    }
    // An address several names share is served by whichever is provided.
    pairs.sort_by_key(|&(a, k)| (a, k == missing));
    pairs.dedup_by_key(|&mut (a, _)| a);
    MISSING.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).extend(unprovided);
    let (size, align) = dylink_memory(&wasm)?;
    let data = alloc(state, (size as u64).max(16) + (1u64 << align), &format!("[provider {} data]", p.name))?;
    let data = (data + (1u64 << align) - 1) & !((1u64 << align) - 1);
    let module = gex_cache::load_or_compile_at(engine, &cwasm_path(&p.wasm, &wasm), &wasm)
        .with_context(|| format!("compiling provider {}", p.name))?;
    crate::aot_share::note(&module, crate::aot_share::Kind::Provider);
    ftab::publish_stateful(shared, memory, module, slots.len() as u32, &pairs, Extras { memory_base: data })?;
    Ok(pairs.len())
}

/// Where a provider module's compiled form is kept: beside it, named by
/// the module's content (`lua51.0123456789abcdef.cwasm`), so a rebuilt
/// module is never run from a stale artifact. Anything that precompiles a
/// provider ahead of time (an ejected project) names it with this too.
pub fn cwasm_path(wasm_path: &std::path::Path, wasm: &[u8]) -> PathBuf {
    let key = gex_cache::key("libprov", [wasm]);
    let stem = wasm_path.file_stem().and_then(|s| s.to_str()).unwrap_or("provider");
    wasm_path.with_file_name(format!("{stem}.{}.cwasm", &key[..16]))
}

/// Data whose relocations have been applied, by its guest address.
static RELOCATED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// After an instantiation: the module's data relocations (`__wasm_apply_
/// data_relocs`: the absolute data and function addresses its initialised
/// data holds), once per process -- its data is the process's, and a later
/// thread's instance must not reset what has changed since. Its start
/// function has initialised the data itself, once, by then. Held under a
/// lock, so a thread instantiating meanwhile waits for it.
pub(crate) fn relocate_once(cx: &mut wasmtime::StoreContextMut<'_, HostState>, inst: &wasmtime::Instance, x: Extras) -> Result<()> {
    let mut done = RELOCATED.lock().unwrap_or_else(|e| e.into_inner());
    if done.contains(&x.memory_base) {
        return Ok(());
    }
    if let Ok(f) = inst.get_typed_func::<(), ()>(&mut *cx, "__wasm_apply_data_relocs") {
        f.call(&mut *cx, ()).context("applying a provider's data relocations")?;
    }
    done.push(x.memory_base);
    Ok(())
}

/// A `gex.provider_throw` unwinding to its `gex.provider_try`.
#[derive(Debug)]
pub(crate) struct Throw(pub i32);

impl std::fmt::Display for Throw {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "gex.provider_throw({}) with no gex.provider_try to catch it", self.0)
    }
}

impl std::error::Error for Throw {}

/// Where a provider's own threads start (`__gex_thread_start`, bound by
/// [`install`]): musl's `clone` sets the child's PC to it.
static THREAD_ENTRY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The host functions stateful providers import.
pub(crate) fn register(linker: &mut Linker<HostState>) -> Result<()> {
    // A wasm64 provider's pointers and table indices are i64 (docs/unity-
    // dotnet.md): one process is one flavour, so the same names take the
    // wider types.
    if gex_abi::mem64() {
        return register64(linker);
    }
    linker.func_wrap(IMPORT_MODULE, "provider_try", |mut caller: Caller<'_, HostState>, f: i32, arg: i32| -> Result<i32> {
        let func = ftab::table_func(&mut caller, f as u32)?;
        let typed = func.typed::<i32, ()>(&caller).context("gex.provider_try: the function is not void(int)")?;
        match typed.call(&mut caller, arg) {
            Ok(()) => Ok(0),
            Err(e) => match e.downcast_ref::<Throw>() {
                Some(t) => Ok(t.0),
                None => Err(e),
            },
        }
    })?;
    linker.func_wrap(IMPORT_MODULE, "provider_throw", |_caller: Caller<'_, HostState>, code: i32| -> Result<()> { Err(Throw(code).into()) })?;
    // The NT process clock as NtQueryPerformanceCounter reads it (100 ns
    // units): the `nt_qpc` provider's.
    linker.func_wrap(IMPORT_MODULE, "nt_qpc", |caller: Caller<'_, HostState>| -> i64 {
        caller.data().nt.as_ref().map_or(0, |p| ((p.clocks.mono_ns)() / 100) as i64)
    })?;
    linker.func_wrap(IMPORT_MODULE, "provider_ctx", |caller: Caller<'_, HostState>| -> i32 { caller.data().thread.ctx_addr as i32 })?;
    linker.func_wrap(IMPORT_MODULE, "provider_time", |_caller: Caller<'_, HostState>, which: i32| -> f64 {
        match which {
            0 => std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0),
            _ => {
                static START: OnceLock<std::time::Instant> = OnceLock::new();
                START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
            }
        }
    })?;
    linker.func_wrap(IMPORT_MODULE, "provider_missing", |mut caller: Caller<'_, HostState>, addr: i32| -> Result<()> {
        let addr = addr as u32 as u64;
        let name = MISSING.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&addr).cloned()).unwrap_or_else(|| "?".into());
        let what = match caller.data().nt.as_ref() {
            Some(p) => p.describe_addr(addr),
            None => format!("{addr:#x}"),
        };
        Err(crate::aot_only::stop(&mut caller, addr, &format!("gex-eject: the program called {name} ({what}), an export of a replaced library that its provider does not implement")))
    })?;
    linker.func_wrap(IMPORT_MODULE, "provider_alloc", |mut caller: Caller<'_, HostState>, size: i32| -> Result<i32> {
        Ok(alloc(caller.data_mut(), size as u32 as u64, "[provider heap]")? as i32)
    })?;
    // The same from the top down: a built-in provider's heap (`alloc_top`).
    linker.func_wrap(IMPORT_MODULE, "provider_alloc_top", |mut caller: Caller<'_, HostState>, size: i32| -> Result<i32> {
        Ok(alloc_top(caller.data_mut(), size as u32 as u64, "[provider heap]")? as i32)
    })?;
    // Diagnostics: text a provider writes to its stderr.
    linker.func_wrap(IMPORT_MODULE, "provider_log", |caller: Caller<'_, HostState>, ptr: i32, len: i32| -> Result<()> {
        if let Some(m) = caller.data().memory.as_ref() {
            if let Some(b) = m.bytes().get(ptr as u32 as usize..(ptr as u32 as usize).saturating_add(len as u32 as usize)) {
                use std::io::Write;
                let _ = std::io::stderr().write_all(b);
            }
        }
        Ok(())
    })?;
    Ok(())
}

/// [`register`] for a 64-bit guest memory: the same functions with i64
/// addresses, sizes and function-table indices (stream U, additive).
fn register64(linker: &mut Linker<HostState>) -> Result<()> {
    linker.func_wrap(IMPORT_MODULE, "provider_try", |mut caller: Caller<'_, HostState>, f: i64, arg: i64| -> Result<i32> {
        let func = ftab::table_func(&mut caller, f as u32)?;
        let typed = func.typed::<i64, ()>(&caller).context("gex.provider_try: the function is not void(void *)")?;
        match typed.call(&mut caller, arg) {
            Ok(()) => Ok(0),
            Err(e) => match e.downcast_ref::<Throw>() {
                Some(t) => Ok(t.0),
                None => Err(e),
            },
        }
    })?;
    linker.func_wrap(IMPORT_MODULE, "provider_throw", |_caller: Caller<'_, HostState>, code: i32| -> Result<()> { Err(Throw(code).into()) })?;
    linker.func_wrap(IMPORT_MODULE, "provider_ctx", |caller: Caller<'_, HostState>| -> i32 { caller.data().thread.ctx_addr as i32 })?;
    linker.func_wrap(IMPORT_MODULE, "provider_missing", |mut caller: Caller<'_, HostState>, addr: i64| -> Result<()> {
        let addr = addr as u64;
        let name = MISSING.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&addr).cloned()).unwrap_or_else(|| "?".into());
        let what = match caller.data().nt.as_ref() {
            Some(p) => p.describe_addr(addr),
            None => format!("{addr:#x}"),
        };
        Err(crate::aot_only::stop(&mut caller, addr, &format!("gex: the program called {name} ({what}), an export of a replaced library that its provider does not implement")))
    })?;
    linker.func_wrap(IMPORT_MODULE, "provider_alloc", |mut caller: Caller<'_, HostState>, size: i64| -> Result<i64> {
        Ok(alloc(caller.data_mut(), size as u64, "[provider heap]")? as i64)
    })?;
    linker.func_wrap(IMPORT_MODULE, "provider_log", |caller: Caller<'_, HostState>, ptr: i64, len: i64| -> Result<()> {
        if let Some(m) = caller.data().memory.as_ref() {
            if let Some(b) = m.bytes().get(ptr as usize..(ptr as usize).saturating_add(len as usize)) {
                use std::io::Write;
                let _ = std::io::stderr().write_all(b);
            }
        }
        Ok(())
    })?;
    // This thread's provider C stack (`stack`): its lowest address; it is
    // `STACK` bytes. musl reports it as the thread's stack.
    linker.func_wrap(IMPORT_MODULE, "provider_stack", |mut caller: Caller<'_, HostState>| -> Result<i64> {
        use wasmtime::AsContextMut;
        let mut cx = caller.as_context_mut();
        let g = stack(&mut cx)?;
        let top = match g.get(&mut cx) {
            Val::I64(v) => v as u64,
            Val::I32(v) => v as u32 as u64,
            _ => 0,
        };
        Ok((top + 16 - STACK) as i64)
    })?;
    linker.func_wrap(IMPORT_MODULE, "provider_thread_entry", |_caller: Caller<'_, HostState>| -> i64 {
        THREAD_ENTRY.load(std::sync::atomic::Ordering::Acquire) as i64
    })?;
    Ok(())
}

/// Linux libraries already given their provider: (path, load bias).
static ELF_INSTALLED: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());

/// After a Linux syscall that mapped executable code (`Kernel::new_code`):
/// a shared object one of this process's stateful providers serves gets
/// its exports bound to the provider, before anything can call them
/// (docs/unity-dotnet.md: Unity's `libmonobdwgc-2.0.so`). The library's
/// dynamic symbols say where each export is; its own code never runs.
pub(crate) fn on_linux_maps(caller: &mut Caller<'_, HostState>, mapped: &[gex_kernel::CodeMap]) -> Result<()> {
    if LIBS.get().is_none_or(|v| v.is_empty()) {
        return Ok(());
    }
    for m in mapped {
        let file = m.path.rsplit('/').next().unwrap_or(&m.path);
        let libs = for_dll(file);
        if libs.is_empty() {
            continue;
        }
        let Some(bytes) = caller.data_mut().with_kernel(|k| k.read_file(&m.path)) else {
            bail!("provider for {}: cannot read the library", m.path);
        };
        let image = gex_ld::elf::load(&bytes, 0).map_err(|e| anyhow!("provider for {}: {e:?}", m.path))?;
        let text = image.segments.iter().find(|s| s.executable).ok_or_else(|| anyhow!("provider for {}: no executable segment", m.path))?;
        let bias = m.start.wrapping_sub(text.vaddr & !0xfff);
        {
            let mut done = ELF_INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
            if done.iter().any(|(p, b)| *p == m.path && *b == bias) {
                continue;
            }
            done.push((m.path.clone(), bias));
        }
        let (lo, hi) = (text.vaddr, text.vaddr + text.bytes.len() as u64);
        let exports: std::collections::HashMap<String, u32> = image
            .dynsyms
            .iter()
            .filter(|y| !y.undefined && !y.tls && y.value >= lo && y.value < hi)
            .map(|y| (y.name.clone(), y.value as u32))
            .collect();
        let (shared, engine) = {
            let jit = caller.data().jit.as_ref().ok_or_else(|| anyhow!("a stateful provider needs the function table (--no-jit?)"))?;
            (Arc::clone(&jit.shared), jit.engine.clone())
        };
        let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
        for lp in libs {
            let n = install(lp, caller.data_mut(), &shared, &engine, &memory, &exports, bias, 0)?;
            if std::env::var_os("GEX_PROVIDER_TRACE").is_some() {
                eprintln!("gex: provider {} for {} at {bias:#x}: {n} exports bound", lp.name, m.path);
            }
        }
        crate::ftab::catch_up(&mut *caller)?;
    }
    Ok(())
}
