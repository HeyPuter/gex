//! Shared libraries for native wasm32 programs, native half (the
//! shim-independent half is the `gex-dylink` crate).
//!
//! A program whose module names libraries (`dylink.0` NEEDED) is installed
//! with them: each library's data is mapped like `ld.so` maps it, every
//! module (program included) is published as a stateful install with a
//! [`Binds`] saying what its imports are, in initialisation order (each
//! after what it needs, the program last), and every thread's store
//! instantiates them in that order as it replays the function table's log.
//! The entries gex starts threads at (`__gex_start`, `__gex_thread_start`)
//! are libc's. libc learns of the libraries through its `__gex_dsos`
//! registry (musl's src/env/wasm32/__gex_dso.c), which runs their
//! constructors and lays out their thread-local blocks.
//!
//! A fork's child gets a copy of the process's registry and function map
//! natively (`JitShared::fork_copy`), so it instantiates the same modules
//! at the same table bases; the data, relocated, is the parent's copy. A
//! library either then dlopens is its own: mapped, initialised and
//! published in that process alone.

use super::*;
use gex_dylink::{Bind, Info, Kind, Link, Loaded};
use std::collections::{HashMap, HashSet};
use wasmtime::{AsContextMut, Val};

/// How one module binds its imports, the same in every store.
pub(crate) struct Binds {
    pub path: String,
    /// Per import, in the module's order: (module, name, binding).
    pub imports: Vec<(String, String, NBind)>,
    /// Slots after the element segment: (table index, export) to fill.
    pub extra: Vec<(u32, String)>,
    /// A `dlopen`ed module's thread-local block (size, alignment), which
    /// each store gives it as it instantiates the module ([`thread_tls`]).
    /// None for the modules a program starts with: libc's registry lays
    /// theirs out in every thread's TLS area.
    pub tls: Option<(u32, u32)>,
}

#[derive(Clone, Debug)]
pub(crate) enum NBind {
    Shim,
    Global(u32),
    /// An export of the module with this ftab module number.
    Export {
        module: u32,
        name: String,
    },
    Late {
        slot: u32,
    },
    Missing(String),
}

/// Whether a program module needs the loader (it names libraries).
pub(crate) fn wanted(wasm: &[u8]) -> Option<Info> {
    let info = Info::parse(wasm).ok()?;
    (!info.needed.is_empty()).then_some(info)
}

/// The process's environment and the program's path, from the initial
/// stack (argc, argv..., 0, envp..., 0) at the context's stack pointer.
fn initial_env(store: &Store<HostState>, memory: &gex_mem::GuestMemory) -> Vec<String> {
    let b = memory.bytes();
    let ctx_addr = store.data().thread.ctx_addr as usize;
    let sp = read_u64(b, ctx_addr + ctx::sp_slot(true)) as usize;
    let word = |a: usize| {
        b.get(a..a + 4)
            .map(|w| u32::from_le_bytes(w.try_into().unwrap()) as usize)
            .unwrap_or(0)
    };
    let cstr = |a: usize| {
        let s = b.get(a..).unwrap_or(&[]);
        let n = s.iter().position(|&c| c == 0).unwrap_or(0);
        String::from_utf8_lossy(&s[..n]).into_owned()
    };
    let argc = word(sp);
    let mut p = sp + 4 * (argc + 2);
    let mut env = Vec::new();
    while word(p) != 0 && env.len() < 4096 {
        env.push(cstr(word(p)));
        p += 4;
    }
    env
}

/// A process's link map, kept with its function table (`ftab::Shared`)
/// for `dlopen` and `dlsym`.
#[derive(Clone)]
pub(crate) struct DyProcess {
    pub link: Link,
    /// The ftab module of each link-map entry.
    pub module_of: Vec<u32>,
    pub ldpath: Vec<String>,
    pub origin: String,
}

fn shared_of(store: &impl wasmtime::AsContext<Data = HostState>) -> Result<Arc<Mutex<JitShared>>> {
    store
        .as_context()
        .data()
        .jit
        .as_ref()
        .map(|j| Arc::clone(&j.shared))
        .ok_or_else(|| anyhow!("wasm32 program: the function table is off (--no-jit?)"))
}

/// Map `p.link.mods[from..]` (whose bytes are `bytes`), publish them in
/// initialisation order with `pairs_of`'s entry bindings, and return that
/// order. Nothing is instantiated yet.
fn publish(
    state: &mut HostState,
    engine: &wasmtime::Engine,
    shared: &Arc<Mutex<JitShared>>,
    p: &mut DyProcess,
    from: usize,
    bytes: Vec<Vec<u8>>,
    pairs_of: impl Fn(usize, &Loaded, &HashMap<u32, u32>) -> Vec<(u64, u32)>,
) -> Result<Vec<usize>> {
    // data, where its own mmap would put it (the program's is its image's)
    for m in p.link.mods[from..].iter_mut() {
        if m.memory_base != 0 {
            continue;
        }
        let align = 1u64 << m.info.mem_align.min(16);
        let size = (m.info.mem_size as u64).max(1) + if align > 0x1000 { align } else { 0 };
        let at = crate::libprov::alloc(state, size, &m.path)?;
        let base = (at + align - 1) & !(align - 1);
        if base > u32::MAX as u64 {
            bail!("{}: data mapped above 4 GiB", m.path);
        }
        m.memory_base = base as u32;
    }
    // compiled before the table is touched (nothing to undo on failure)
    let modules: Vec<Module> = bytes
        .iter()
        .zip(&p.link.mods[from..])
        .map(|(b, m)| gex_cache::compile_cached(engine, b).with_context(|| format!("compiling {}", m.path)))
        .collect::<Result<_>>()?;
    let order = p.link.init_order(from);
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    s.wasm_program = true;
    // table bases: consecutive ranges from the map's end, in order
    let mut next = s.ftab.map.size();
    for &i in &order {
        let (len, _) = p.link.mods[i].info.slots();
        p.link.mods[i].table_base = next;
        next += len.max(1);
    }
    let first_module = s.ftab.modules.len() as u32;
    p.module_of.resize(p.link.mods.len(), u32::MAX);
    for (k, &i) in order.iter().enumerate() {
        p.module_of[i] = first_module + k as u32;
    }
    let mut done: HashSet<usize> = (0..from).collect();
    for &i in &order {
        let m = &p.link.mods[i];
        let tls = dl_tls(m, from)?;
        let binds = p.link.bind(i, &done)?;
        done.insert(i);
        let imports = m
            .info
            .imports
            .iter()
            .zip(binds)
            .map(|(imp, b)| {
                let nb = match b {
                    Bind::Shim => NBind::Shim,
                    Bind::GotMem(v) | Bind::GotFunc(v) => NBind::Global(v),
                    Bind::Export { module, name } => NBind::Export { module: p.module_of[module], name },
                    Bind::Late { slot } => NBind::Late { slot },
                    Bind::Missing(msg) => NBind::Missing(msg),
                };
                (imp.module.clone(), imp.name.clone(), nb)
            })
            .collect();
        let extra = m.info.extra_slots().into_iter().map(|(s, n)| (m.table_base + s, n)).collect();
        let (len, at) = m.info.slots();
        let pairs = pairs_of(i, m, &at);
        let module = p.module_of[i];
        s.ftab.stateful.insert(module, crate::libprov::Extras { memory_base: m.memory_base as u64 });
        s.ftab.dylink.insert(module, Arc::new(Binds { path: m.path.clone(), imports, extra, tls }));
        s.ftab.modules.push(modules[i - from].clone());
        let (n, ok) = s.ftab.map.publish_at(m.table_base, len, module, gex_kernel::ftab::Tier::Unit, &pairs);
        if !ok {
            bail!("{}: its table range moved while loading", m.path);
        }
        s.ftab.providers.insert(n);
    }
    s.ftab.dyproc = Some(p.clone());
    Ok(order)
}

/// Install a program that names libraries: load them, publish every
/// module, instantiate them in this store, fill libc's registry.
#[allow(clippy::too_many_arguments)]
pub(crate) fn install_program(
    store: &mut Store<HostState>,
    memory: &gex_mem::GuestMemory,
    path: &str,
    wasm: Vec<u8>,
    info: Info,
    mem_base: u64,
    entry: u64,
    thread_entry: u64,
) -> Result<()> {
    let t0 = std::time::Instant::now();
    let (wasm, info) = if checked_calls() {
        let w = checked(wasm);
        let i = Info::parse(&w)?;
        (w, i)
    } else {
        (wasm, info)
    };
    let env = initial_env(store, memory);
    let ldpath: Vec<String> = env
        .iter()
        .find_map(|e| e.strip_prefix("LD_LIBRARY_PATH="))
        .map(|v| v.split(':').filter(|s| !s.is_empty()).map(str::to_string).collect())
        .unwrap_or_default();
    let origin = path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_else(|| ".".into());
    let mut p = DyProcess {
        link: Link { mods: vec![Loaded { name: path.to_string(), path: path.to_string(), info, memory_base: mem_base as u32, table_base: 0 }] },
        module_of: Vec::new(),
        ldpath,
        origin,
    };
    let libs = {
        let state = store.data_mut();
        let (o, lp) = (p.origin.clone(), p.ldpath.clone());
        p.link.needed_of(0, |name, rp| gex_dylink::search(name, rp, &o, &lp, |f| state.with_kernel(|k| k.read_file(f)).map(checked)))?
    };
    let mut bytes = vec![wasm];
    for (name, path, b, info) in libs {
        p.link.mods.push(Loaded { name, path, info, memory_base: 0, table_base: 0 });
        bytes.push(b);
    }
    // libc: where gex enters threads, and the registry
    let libc = p.link.lookup("__gex_start", Kind::Func).ok_or_else(|| anyhow!("{path}: no library defines __gex_start (libc.so)"))?;
    let shared = shared_of(store)?;
    let engine = store.engine().clone();
    let order = publish(store.data_mut(), &engine, &shared, &mut p, 0, bytes, |i, m, at| {
        if i == libc {
            let slot = |n: &str| at[&m.info.exports[n].index];
            vec![(entry, slot("__gex_start")), (thread_entry, slot("__gex_thread_start"))]
        } else {
            Vec::new()
        }
    })?;
    {
        let data = memory.bytes();
        for a in [entry, thread_entry] {
            gex_kernel::ftab::fcache_clear(data, a);
        }
    }
    crate::ftab::publish_bridges(&shared, &[entry, thread_entry], &[])?;
    store.data_mut().with_kernel(|k| k.wasm32 = true);
    crate::ftab::catch_up(&mut *store)?;
    write_registry(memory, &p.link, &order, libc)?;
    trace(&p, &order, path, t0);
    Ok(())
}

/// A module's thread-local block when [`publish`]ed by `dlopen` (`from`
/// past the program's own modules): libc's registry does not have it, so
/// every store gives it one as it instantiates it.
fn dl_tls(m: &Loaded, from: usize) -> Result<Option<(u32, u32)>> {
    let size = m.info.value("__tls_size");
    if from == 0 || size == 0 {
        return Ok(None);
    }
    for f in ["__wasm_init_tls", "__gex_set_tls_base"] {
        if m.info.exports.get(f).is_none_or(|e| e.kind != Kind::Func) {
            bail!("{}: has thread-local data but does not export {f} (link it with a newer gexcc)", m.path);
        }
    }
    Ok(Some((size, m.info.value("__tls_align").max(1))))
}

fn trace(p: &DyProcess, order: &[usize], what: &str, t0: std::time::Instant) {
    if std::env::var_os("GEX_WASM_TRACE").is_none() {
        return;
    }
    for &i in order {
        let m = &p.link.mods[i];
        eprintln!("gex: wasm32 module {}: data {:#x}+{:#x}, table {}", m.path, m.memory_base, m.info.mem_size, m.table_base);
    }
    eprintln!("gex: wasm32 {what}: {} modules loaded in {:.3}s", order.len(), t0.elapsed().as_secs_f64());
}

use gex_dylink::HANDLE_BASE;

/// Bumped by every `dlopen` once its modules are published: a thread that
/// has not seen the latest number catches up at its next system call's
/// return ([`sync`]), so a function pointer another thread got from
/// `dlsym` and handed over (a futex, a pipe, a socket) is in its table by
/// the time it can call it.
///
/// A known gap: a pointer handed over lock-free, with no system call on the
/// receiving side before the call (gexcc's loop polls come only every so
/// many back-edges), can reach a slot this thread has not caught up with.
/// That call traps and the trap says so ([`behind`]); a trap cannot be
/// retried, a wasm32 program's frames being gone by then.
/// `GEX_DL_CHECKED_CALLS=1` ([`checked_calls`]) closes it at the cost of a
/// compare per indirect call.
static DL_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// After a system call: catch up with the function table when a `dlopen`
/// has published modules since this store last did.
pub(crate) fn sync(caller: &mut Caller<'_, HostState>) -> Result<()> {
    use std::sync::atomic::Ordering;
    let gen = DL_GEN.load(Ordering::Acquire);
    let Some(st) = caller.data_mut().jit.as_mut().and_then(|j| j.ftab.as_mut()) else { return Ok(()) };
    if st.dl_seen == gen {
        return Ok(());
    }
    st.dl_seen = gen;
    crate::ftab::catch_up(&mut *caller)
}

/// `GEX_DL_CHECKED_CALLS=1`: every wasm32 module of a program linked
/// against shared libraries gets its indirect calls guarded
/// ([`gex_dylink::checked_calls`]), so even a function pointer from a
/// `dlopen` that reached a thread with no system call between is caught up
/// with before the call. Off, nothing is rewritten and nothing costs.
pub(crate) fn checked_calls() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("GEX_DL_CHECKED_CALLS").is_ok_and(|v| v == "1"))
}

/// A module's bytes as loaded: guarded when [`checked_calls`] is on.
fn checked(b: Vec<u8>) -> Vec<u8> {
    if !checked_calls() || !b.starts_with(b"\0asm") {
        return b;
    }
    match gex_dylink::checked_calls(&b) {
        Ok(out) => out,
        Err(e) => {
            eprintln!("gex: GEX_DL_CHECKED_CALLS: {e:#}; that module is left unguarded");
            b
        }
    }
}

/// The imports of a module [`checked`] guarded, in this store: the
/// catch-up its guard calls, and the slot count it compares with.
pub(crate) fn define_checked(c: &mut wasmtime::StoreContextMut<'_, HostState>, l: &mut wasmtime::Linker<HostState>) -> Result<()> {
    let size = match c.data().jit.as_ref().and_then(|j| j.ftab.as_ref()).and_then(|st| st.dl_size) {
        Some(g) => g,
        None => {
            let sh = shared_of(&*c)?;
            let n = sh.lock().unwrap_or_else(|e| e.into_inner()).ftab.map.size();
            let g = wasmtime::Global::new(&mut *c, wasmtime::GlobalType::new(wasmtime::ValType::I32, wasmtime::Mutability::Var), Val::I32(n as i32))?;
            if let Some(st) = c.data_mut().jit.as_mut().and_then(|j| j.ftab.as_mut()) {
                st.dl_size = Some(g);
            }
            g
        }
    };
    l.define(&mut *c, gex_abi::IMPORT_MODULE, gex_dylink::DL_SIZE, size)?;
    let stale = wasmtime::Func::wrap(&mut *c, |mut caller: Caller<'_, HostState>, _index: i32| -> wasmtime::Result<()> {
        crate::host((|| -> Result<()> {
            let gen = DL_GEN.load(std::sync::atomic::Ordering::Acquire);
            crate::ftab::catch_up(&mut caller)?;
            if let Some(st) = caller.data_mut().jit.as_mut().and_then(|j| j.ftab.as_mut()) {
                st.dl_seen = gen;
            }
            Ok(())
        })())
    });
    l.define(&mut *c, gex_abi::IMPORT_MODULE, gex_dylink::DL_STALE, stale)?;
    Ok(())
}

/// Whether this store's table is behind a `dlopen` it has not caught up
/// with: a call through a slot that is not there yet (out of the table's
/// bounds, or holding `resolve`) is then a function pointer that reached
/// this thread with no system call between the `dlopen` and the call.
pub(crate) fn behind(state: &HostState) -> bool {
    let gen = DL_GEN.load(std::sync::atomic::Ordering::Acquire);
    state.jit.as_ref().and_then(|j| j.ftab.as_ref()).is_some_and(|st| st.dl_seen != gen)
}

/// `gex.dlopen(path)`: load a library and what it needs into the process,
/// run their constructors; its handle. Other threads see the new modules
/// as they next catch up with the function table's log.
fn dlopen(caller: &mut Caller<'_, HostState>, path: &str) -> Result<u32> {
    let t0 = std::time::Instant::now();
    let shared = shared_of(caller)?;
    let Some(mut p) = shared.lock().unwrap_or_else(|e| e.into_inner()).ftab.dyproc.clone() else {
        bail!("{path}: dlopen in a program not linked against shared libraries");
    };
    if p.link.mods[0].info.needed.is_empty() {
        bail!("{path}: dlopen in a static program (link it with GEXCC_SHARED=1)");
    }
    if let Some(i) = p.link.loaded(path) {
        return Ok(HANDLE_BASE + i as u32 * 16);
    }
    let (found, bytes) = {
        let state = caller.data_mut();
        let (o, lp) = (p.origin.clone(), p.ldpath.clone());
        gex_dylink::search(path, &[], &o, &lp, |f| state.with_kernel(|k| k.read_file(f)).map(checked))?
    };
    let info = Info::parse(&bytes).with_context(|| format!("{found}: not a wasm shared library"))?;
    if !info.dylink {
        bail!("{found}: not a wasm shared library");
    }
    let from = p.link.mods.len();
    p.link.mods.push(Loaded { name: path.to_string(), path: found, info, memory_base: 0, table_base: 0 });
    let mut all = vec![bytes];
    let libs = {
        let state = caller.data_mut();
        let (o, lp) = (p.origin.clone(), p.ldpath.clone());
        p.link.needed_of(from, |name, rp| gex_dylink::search(name, rp, &o, &lp, |f| state.with_kernel(|k| k.read_file(f)).map(checked)))?
    };
    for (name, path, b, info) in libs {
        p.link.mods.push(Loaded { name, path, info, memory_base: 0, table_base: 0 });
        all.push(b);
    }
    let engine = caller.engine().clone();
    let order = publish(caller.data_mut(), &engine, &shared, &mut p, from, all, |_, _, _| Vec::new())?;
    DL_GEN.fetch_add(1, std::sync::atomic::Ordering::Release);
    let gen = DL_GEN.load(std::sync::atomic::Ordering::Acquire);
    crate::ftab::catch_up(&mut *caller)?;
    if let Some(st) = caller.data_mut().jit.as_mut().and_then(|j| j.ftab.as_mut()) {
        st.dl_seen = gen;
    }
    // constructors, in order, in this thread
    for &i in &order {
        let module = p.module_of[i];
        let inst = caller.data().jit.as_ref().and_then(|j| j.ftab.as_ref()).and_then(|s| s.dyl.get(&module).copied());
        if let Some(inst) = inst {
            if let Some(f) = inst.get_func(&mut *caller, "__wasm_call_ctors") {
                f.typed::<(), ()>(&*caller)?.call(&mut *caller, ())?;
            }
        }
    }
    trace(&p, &order, path, t0);
    Ok(HANDLE_BASE + from as u32 * 16)
}

/// `gex.dlsym(handle, name)`: a function's table index or a datum's
/// address; 0 when not found. Handle 0: the global scope.
fn dlsym(caller: &mut Caller<'_, HostState>, handle: u32, name: &str) -> Result<u32> {
    let shared = shared_of(caller)?;
    let p = shared.lock().unwrap_or_else(|e| e.into_inner()).ftab.dyproc.clone();
    Ok(p.map_or(0, |p| p.link.dlsym(handle, name)))
}

fn guest_str(caller: &mut Caller<'_, HostState>, ptr: u32) -> Result<String> {
    let mem = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let b = mem.bytes();
    let s = b.get(ptr as usize..).ok_or_else(|| anyhow!("string outside memory"))?;
    let n = s.iter().position(|&c| c == 0).ok_or_else(|| anyhow!("unterminated string"))?;
    Ok(String::from_utf8_lossy(&s[..n]).into_owned())
}

/// `gex.dlopen(path, err, errlen) -> handle` and `gex.dlsym(handle, name)
/// -> address`, libgex's dl.c's way to the loader.
pub(crate) fn register(linker: &mut Linker<HostState>) -> Result<()> {
    linker.func_wrap(
        gex_abi::IMPORT_MODULE,
        "dlopen",
        |mut caller: Caller<'_, HostState>, path: i32, err: i32, errlen: i32| -> wasmtime::Result<i32> {
            crate::host((|| -> anyhow::Result<i32> {
                let path = guest_str(&mut caller, path as u32)?;
                match dlopen(&mut caller, &path) {
                    Ok(h) => Ok(h as i32),
                    Err(e) => {
                        let msg = format!("{e:#}");
                        let mem = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
                        let b = mem.bytes();
                        let n = msg.len().min((errlen as usize).saturating_sub(1));
                        if let Some(d) = b.get_mut(err as u32 as usize..err as u32 as usize + n + 1) {
                            d[..n].copy_from_slice(&msg.as_bytes()[..n]);
                            d[n] = 0;
                        }
                        Ok(0)
                    }
                }
            })())
        },
    )?;
    linker.func_wrap(
        gex_abi::IMPORT_MODULE,
        "dlsym",
        |mut caller: Caller<'_, HostState>, handle: i32, name: i32| -> wasmtime::Result<i32> {
            crate::host((|| -> anyhow::Result<i32> {
                let name = guest_str(&mut caller, name as u32)?;
                Ok(dlsym(&mut caller, handle as u32, &name)? as i32)
            })())
        },
    )?;
    Ok(())
}

/// libc's `__gex_dsos` (gex-dylink's `Link::write_registry`).
fn write_registry(memory: &gex_mem::GuestMemory, link: &Link, order: &[usize], libc: usize) -> Result<()> {
    link.write_registry(memory.bytes(), order, libc)
}

/// Define module `module`'s imports per its [`Binds`] in `l`.
pub(crate) fn define_imports(
    c: &mut wasmtime::StoreContextMut<'_, HostState>,
    l: &mut Linker<HostState>,
    table: Table,
    module: &Module,
    dy: &Binds,
) -> Result<()> {
    let types: HashMap<(String, String), wasmtime::ExternType> = module
        .imports()
        .map(|i| ((i.module().to_string(), i.name().to_string()), i.ty()))
        .collect();
    for (m, name, b) in &dy.imports {
        let Some(ty) = types.get(&(m.clone(), name.clone())) else {
            continue;
        };
        // already given (ftab's stand-ins for clang's three names for main)
        if l.get(&mut *c, m, name).is_ok() {
            continue;
        }
        match (b, ty) {
            (NBind::Shim, _) if m == "env" && (name == "__asyncify_state" || name == "__asyncify_data") => {
                let g = asyncify_globals(c)?;
                l.define(&mut *c, m, name, if name == "__asyncify_state" { g.0 } else { g.1 })?;
            }
            (NBind::Shim, _) => {}
            (NBind::Global(v), wasmtime::ExternType::Global(g)) => {
                let g = wasmtime::Global::new(&mut *c, g.clone(), Val::I32(*v as i32))?;
                l.define(&mut *c, m, name, g)?;
            }
            (NBind::Missing(_), wasmtime::ExternType::Global(g)) => {
                let g = wasmtime::Global::new(&mut *c, g.clone(), Val::I32(0))?;
                l.define(&mut *c, m, name, g)?;
            }
            (NBind::Export { module: d, name: en }, _) => {
                let inst = c
                    .data()
                    .jit
                    .as_ref()
                    .and_then(|j| j.ftab.as_ref())
                    .and_then(|s| s.dyl.get(d).copied())
                    .ok_or_else(|| anyhow!("{}: {name}'s module is not instantiated in this thread", dy.path))?;
                let e = inst
                    .get_export(&mut *c, en)
                    .ok_or_else(|| anyhow!("{}: {en} not exported", dy.path))?;
                l.define(&mut *c, m, name, e)?;
            }
            (NBind::Late { slot }, wasmtime::ExternType::Func(ft)) => {
                let slot = *slot;
                let f = wasmtime::Func::new(&mut *c, ft.clone(), move |mut caller, args, rets| {
                    let f = match table.get(&mut caller, slot as u64) {
                        Some(wasmtime::Ref::Func(Some(f))) => f,
                        _ => return Err(wasmtime::Error::msg(format!("table slot {slot} is empty"))),
                    };
                    f.call(&mut caller, args, rets)
                });
                l.define(&mut *c, m, name, f)?;
            }
            (NBind::Missing(msg), wasmtime::ExternType::Func(ft)) => {
                let msg = if msg.is_empty() {
                    format!("{}: undefined weak symbol {name} called", dy.path)
                } else {
                    msg.clone()
                };
                let f = wasmtime::Func::new(&mut *c, ft.clone(), move |_, _, _| {
                    Err(wasmtime::Error::msg(format!("symbol lookup error: {msg}")))
                });
                l.define(&mut *c, m, name, f)?;
            }
            (b, t) => bail!("{}: cannot bind {m}.{name} ({t:?}) to {b:?}", dy.path),
        }
    }
    Ok(())
}

/// After a module's instantiation in a store: remember the instance, fill
/// its exported functions' extra slots.
pub(crate) fn after_instantiate(
    mut cx: impl AsContextMut<Data = HostState>,
    table: Table,
    module: u32,
    inst: &wasmtime::Instance,
    dy: &Binds,
) -> Result<()> {
    let mut c = cx.as_context_mut();
    for (slot, name) in &dy.extra {
        let f = inst
            .get_func(&mut c, name)
            .ok_or_else(|| anyhow!("{}: {name} not exported", dy.path))?;
        table.set(&mut c, *slot as u64, wasmtime::Ref::Func(Some(f)))?;
    }
    if let Some(s) = c.data_mut().jit.as_mut().and_then(|j| j.ftab.as_mut()) {
        s.dyl.insert(module, *inst);
    }
    if let Some((size, align)) = dy.tls {
        thread_tls(&mut c, module, inst, size, align).with_context(|| format!("{}: its thread-local block", dy.path))?;
    }
    Ok(())
}

/// A `dlopen`ed module's thread-local block in this store's thread: a fork
/// child's first thread keeps the one its parent's thread had (the copy of
/// it is in the child's memory, as libc's own blocks are); any other thread
/// gets a fresh one with the module's initial image (`__wasm_init_tls`).
fn thread_tls(c: &mut wasmtime::StoreContextMut<'_, HostState>, module: u32, inst: &wasmtime::Instance, size: u32, align: u32) -> Result<()> {
    let kept = c.data().spawn.as_ref().and_then(|cx| cx.fork_dl_tls.lock().unwrap_or_else(|e| e.into_inner()).remove(&module));
    let (block, init) = match kept {
        Some(b) => (b, "__gex_set_tls_base"),
        None => (tls_block(c.data_mut(), size, align)?, "__wasm_init_tls"),
    };
    let f = inst.get_typed_func::<i32, ()>(&mut *c, init)?;
    f.call(&mut *c, block as i32)?;
    if let Some(s) = c.data_mut().jit.as_mut().and_then(|j| j.ftab.as_mut()) {
        s.dl_tls.insert(module, block);
    }
    Ok(())
}

/// TLS_CHUNK bytes at a time, per store, freed when its thread ends
/// ([`free_thread_tls`]).
const TLS_CHUNK: u64 = 0x10000;

/// `size` bytes at `align` for a thread-local block, from this store's chunks.
fn tls_block(state: &mut HostState, size: u32, align: u32) -> Result<u32> {
    let (size, align) = (size as u64, (align as u64).max(8));
    let fits = |a: &(u64, u64)| ((a.0 + align - 1) & !(align - 1)) + size <= a.1;
    let arena = state.jit.as_ref().and_then(|j| j.ftab.as_ref()).map(|s| s.dl_tls_arena).unwrap_or((0, 0));
    let arena = if fits(&arena) {
        arena
    } else {
        let len = (size + align).max(TLS_CHUNK);
        let at = crate::libprov::alloc(state, len, "[dlopen tls]")?;
        if let Some(s) = state.jit.as_mut().and_then(|j| j.ftab.as_mut()) {
            s.dl_tls_chunks.push((at, (len + 0xfff) & !0xfff));
        }
        (at, at + len)
    };
    let block = (arena.0 + align - 1) & !(align - 1);
    if block + size > u32::MAX as u64 {
        bail!("mapped above 4 GiB");
    }
    if let Some(s) = state.jit.as_mut().and_then(|j| j.ftab.as_mut()) {
        s.dl_tls_arena = (block + size, arena.1);
    }
    Ok(block as u32)
}

/// A thread is ending (not its process): unmap the thread-local blocks its
/// store made for `dlopen`ed modules.
pub(crate) fn free_thread_tls(state: &mut HostState) {
    let chunks = state.jit.as_mut().and_then(|j| j.ftab.as_mut()).map(|s| std::mem::take(&mut s.dl_tls_chunks)).unwrap_or_default();
    for (at, len) in chunks {
        let _ = state.with_kernel(|k| k.mem.lock().unmap(at, len));
    }
}

/// This store's `dlopen`ed modules' thread-local blocks, for a fork's child
/// (its `SpawnContext::fork_dl_tls`).
pub(crate) fn fork_tls(state: &HostState) -> HashMap<u32, u32> {
    state.jit.as_ref().and_then(|j| j.ftab.as_ref()).map(|s| s.dl_tls.clone()).unwrap_or_default()
}

/// This store's asyncify state and data globals, made on first use.
fn asyncify_globals(c: &mut wasmtime::StoreContextMut<'_, HostState>) -> Result<(wasmtime::Global, wasmtime::Global)> {
    if let Some(g) = c.data().jit.as_ref().and_then(|j| j.ftab.as_ref()).and_then(|s| s.asyncify) {
        return Ok(g);
    }
    let ty = wasmtime::GlobalType::new(wasmtime::ValType::I32, wasmtime::Mutability::Var);
    let g = (
        wasmtime::Global::new(&mut *c, ty.clone(), Val::I32(0))?,
        wasmtime::Global::new(&mut *c, ty, Val::I32(0))?,
    );
    if let Some(s) = c.data_mut().jit.as_mut().and_then(|j| j.ftab.as_mut()) {
        s.asyncify = Some(g);
    }
    Ok(g)
}
