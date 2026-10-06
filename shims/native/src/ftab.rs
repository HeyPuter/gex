//! The function table, native half (docs/merge-plan.md §9.8.1).
//!
//! A wasmtime table belongs to one store, so each guest thread has its own
//! `gex.ftab`, numbered alike from the process's [`gex_kernel::ftab::FuncMap`]
//! (in [`JitShared`]) and kept current by replaying its log: an install is
//! the module instantiated into this store with `__table_base` at its range
//! (its element segment fills it), a retirement points the index back at
//! `resolve`. A store behind the log is never wrong: every slot it has not
//! filled holds `resolve`, which asks `gex.lookup`, which catches up first.
//!
//! While tier-1 is the fallback (§9.4 step 3), it and the table meet in two
//! places: index 1 runs tier-1 under the `F_body` contract (the call-out),
//! and the dispatch slot of every address a function serves holds the
//! runtime module's `bridge`, which calls it and dispatches on what it left
//! in `ctx::PC`. Translations publish their bridges through the ordinary
//! registry ([`CompiledUnit::bridges`]), so every store's dispatch table
//! gets them as it replays.

use super::*;
use gex_kernel::ftab::{Event, FuncMap, Tier};
use std::collections::HashMap;
use wasmtime::{AsContext, AsContextMut};

/// The process's half, inside [`JitShared`].
#[derive(Default)]
pub(crate) struct Shared {
    pub map: FuncMap,
    /// Every installed module, by `Install::module`.
    pub modules: Vec<Module>,
    /// The runtime module (`gex_cgen::runtime`), compiled once.
    runtime: Option<Module>,
    /// The page checks of each run-time translation, by install: the
    /// `PAGE_GEN` value of every page it covers, as it was translated.
    checks: HashMap<u32, Vec<(u64, u32)>>,
    /// Addresses the bridge handed back to tier-1 (`cache_op::FTAB_TIER1`).
    pub tier1_only: std::collections::HashSet<u64>,
    /// Bumped by every retirement: a background translation made across
    /// one may have read code since rewritten, and is dropped.
    epoch: u64,
    /// Addresses handed to the background translator (`queue`), until a
    /// retirement covers them.
    queued: std::collections::HashSet<u64>,
    /// Stateful providers' installs (`libprov`), by module: their data's
    /// guest address.
    pub stateful: HashMap<u32, crate::libprov::Extras>,
    /// Installs that are export providers (`publish_pairs`): native code in
    /// place of a library's, so a rewrite of the library's bytes there does
    /// not retire them (the bytes are not what runs). Noita's mod sandbox
    /// overwrites ~120 lua51.dll entry points; pewasm, whose lua51 is
    /// native too, ignores those writes the same way.
    pub providers: std::collections::HashSet<u32>,
    /// Which guest process this is, for the run-time translator
    /// (`ftab::Request::process`): one per process image, shared with a
    /// forked child until it `execve`s.
    pub process: Process,
}

/// A guest process's identity for the run-time translator, unique for the
/// run; the translator forgets the process when the last reference goes.
pub(crate) struct Process(pub u64);

impl Default for Process {
    fn default() -> Process {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Process(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(t) = gex_kernel::ftab::translator() {
            t.forget(self.0);
        }
    }
}

/// One store's half, inside [`Jit`].
pub(crate) struct Store {
    pub table: Table,
    pub resolve: wasmtime::Func,
    pub bridge: wasmtime::Func,
    /// The run loop (`gex_cgen::runtime::ROOT`), which an AOT-only process
    /// enters through (`aot_only::bind_root`).
    pub root: wasmtime::Func,
    /// The stack pointer this thread's stateful providers share
    /// (`libprov::stack`), made at the first one's instantiation.
    pub provider_sp: Option<wasmtime::Global>,
    /// The lowest address of that stack (`libprov::STACK` bytes).
    pub provider_stack_lo: u64,
    /// The wasm exception tag of `gex.provider_throw` (wasm64 providers; [`crate::libprov::tag`]).
    pub provider_tag: Option<wasmtime::Tag>,
    /// How far through the map's log this table is.
    replayed: usize,
}

/// `GEX_NT_STATS` counters: host lookups, run-time translations, lookups
/// answered with the fallback, stale functions found on a lookup.
/// Whether `GEX_NT_STATS` is set (read once).
pub(crate) fn stats_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("GEX_NT_STATS").is_some())
}

pub(crate) mod stats {
    use std::sync::atomic::AtomicU64;
    pub static LOOKUPS: AtomicU64 = AtomicU64::new(0);
    pub static TRANSLATED: AtomicU64 = AtomicU64::new(0);
    pub static FALLBACKS: AtomicU64 = AtomicU64::new(0);
    pub static STALE: AtomicU64 = AtomicU64::new(0);
    /// Guest execution redirected by trapping out of every wasm frame
    /// (`HostState::resume_at`: signals, NtContinue, exception dispatch),
    /// which discards the whole nest.
    pub static RESUMES: AtomicU64 = AtomicU64::new(0);
    /// Why each resume happened (`note_resume_cause`), by cause and target.
    pub static RESUME_CAUSES: std::sync::Mutex<Option<std::collections::HashMap<(String, u64), u64>>> = std::sync::Mutex::new(None);
    /// Record a trap-out resume's cause (a syscall's name, an exception
    /// code, a signal) and where the guest went.
    pub fn note_resume_cause(cause: String, target: u64) {
        *RESUME_CAUSES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).entry((cause, target)).or_default() += 1;
    }
    /// The causes since the last call, totals first, then the `n` busiest
    /// (cause, target) pairs.
    pub fn take_resume_causes(n: usize) -> (Vec<(String, u64)>, Vec<((String, u64), u64)>) {
        let m = RESUME_CAUSES.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
        let mut by: std::collections::HashMap<String, u64> = Default::default();
        for ((c, _), k) in &m {
            *by.entry(c.clone()).or_default() += k;
        }
        let mut tot: Vec<_> = by.into_iter().collect();
        tot.sort_by(|a, b| b.1.cmp(&a.1));
        let mut v: Vec<_> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.truncate(n);
        (tot, v)
    }
    /// Calls the fallback served, and 1 in 1024 of their targets.
    pub static FALLBACK_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static FALLBACK_SITES: std::sync::Mutex<Option<std::collections::HashMap<u64, u64>>> = std::sync::Mutex::new(None);
    /// The `n` most sampled fallback targets since the last call.
    pub fn take_fallback_sites(n: usize) -> Vec<(u64, u64)> {
        let m = FALLBACK_SITES.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
        let mut v: Vec<_> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.truncate(n);
        v
    }
    /// Microseconds in the translator, in compiling its modules, and the
    /// longest single lookup miss (translate + compile + install).
    pub static TRANSLATE_US: AtomicU64 = AtomicU64::new(0);
    pub static COMPILE_US: AtomicU64 = AtomicU64::new(0);
    pub static WORST_US: AtomicU64 = AtomicU64::new(0);
    /// Background translations: queued, dropped (retired meanwhile), and
    /// microseconds from queueing to publication.
    pub static QUEUED: AtomicU64 = AtomicU64::new(0);
    pub static DROPPED: AtomicU64 = AtomicU64::new(0);
    pub static LATENCY_US: AtomicU64 = AtomicU64::new(0);
    pub fn line() -> String {
        use std::sync::atomic::Ordering::Relaxed;
        format!(
            "ftab lookups={} translated={} fallback={} stale={} resumes={} exc_nested={} exc_escaped={} translate_ms={} compile_ms={} worst_ms={} queued={} dropped={} latency_ms={}",
            LOOKUPS.load(Relaxed),
            TRANSLATED.load(Relaxed),
            FALLBACKS.load(Relaxed),
            STALE.load(Relaxed),
            RESUMES.load(Relaxed),
            crate::nt::EXC_NESTED.load(Relaxed),
            crate::nt::EXC_ESCAPED.load(Relaxed),
            TRANSLATE_US.load(Relaxed) / 1000,
            COMPILE_US.load(Relaxed) / 1000,
            WORST_US.load(Relaxed) / 1000,
            QUEUED.load(Relaxed),
            DROPPED.load(Relaxed),
            LATENCY_US.load(Relaxed) / 1000
        )
    }
}

/// Room a table keeps past the highest index handed out, so a slot another
/// thread allocated and published to the probe cache is in range here
/// (holding `resolve`) before this store has replayed it.
const HEADROOM: u32 = 1 << 16;

fn shared(cx: &impl AsContext<Data = HostState>) -> Result<Arc<Mutex<JitShared>>> {
    let jit = cx.as_context().data().jit.as_ref().ok_or_else(|| anyhow!("runtime translation is not set up"))?;
    Ok(Arc::clone(&jit.shared))
}

/// Make this store's table and runtime instance, if it has none yet.
pub(crate) fn ensure(mut cx: impl AsContextMut<Data = HostState>) -> Result<()> {
    let Some(jit) = cx.as_context().data().jit.as_ref() else { return Ok(()) };
    if jit.ftab.is_some() {
        return Ok(());
    }
    let (sh, linker, tables) = (Arc::clone(&jit.shared), jit.linker.clone(), jit.tables.clone());
    let memory = cx.as_context().data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let rm = {
        let mut s = sh.lock().unwrap_or_else(|e| e.into_inner());
        match &s.ftab.runtime {
            Some(m) => m.clone(),
            None => {
                let m = gex_cache::compile(cx.as_context().engine(), &gex_cgen::runtime::module())
                    .context("compiling the function table's runtime module")?;
                crate::aot_share::note(&m, crate::aot_share::Kind::Runtime);
                s.ftab.runtime = Some(m.clone());
                m
            }
        }
    };
    // A table64 beside a 64-bit guest memory, as wasm64 units import it.
    let ftab_type = if gex_abi::mem64() {
        wasmtime::TableType::new64(wasmtime::RefType::FUNCREF, 0, None)
    } else {
        wasmtime::TableType::new(wasmtime::RefType::FUNCREF, 0, None)
    };
    let table = Table::new(cx.as_context_mut(), ftab_type, wasmtime::Ref::Func(None))?;
    let mut l = linker;
    l.allow_shadowing(true);
    l.define(cx.as_context_mut(), gex_abi::IMPORT_MODULE, exports::MEMORY, memory.shared().clone())?;
    l.define(cx.as_context_mut(), gex_abi::IMPORT_MODULE, gex_abi::ftab::TABLE, table)?;
    for i in 0..gex_abi::DISPATCH_TABLES {
        // Natively only the tables the image exports exist; the rest are
        // never dispatched to but must be there to import.
        let t = match tables.get(i) {
            Some(t) => *t,
            None => Table::new(cx.as_context_mut(), wasmtime::TableType::new(wasmtime::RefType::FUNCREF, 0, None), wasmtime::Ref::Func(None))?,
        };
        l.define(cx.as_context_mut(), gex_abi::IMPORT_MODULE, exports::dispatch_table(i), t)?;
    }
    let inst = l.instantiate(cx.as_context_mut(), &rm).context("instantiating the function table's runtime module")?;
    let get = |cx: &mut wasmtime::StoreContextMut<'_, HostState>, n: &str| {
        inst.get_func(&mut *cx, n).ok_or_else(|| anyhow!("the runtime module exports no {n}"))
    };
    let mut c = cx.as_context_mut();
    let resolve = get(&mut c, gex_cgen::runtime::RESOLVE)?;
    let bridge = get(&mut c, gex_cgen::runtime::BRIDGE)?;
    let root = get(&mut c, gex_cgen::runtime::ROOT)?;
    let fallback = if crate::aot_only::enabled() {
        wasmtime::Func::wrap(
            &mut c,
            |mut caller: Caller<'_, HostState>, ctx: i32, _depth: i32, _need: i32, _ret: i64| -> wasmtime::Result<i32> { crate::host((|| -> anyhow::Result<i32> {
                crate::aot_only::fallback(&mut caller, ctx)
            })()) },
        )
    } else {
        wasmtime::Func::wrap(
            &mut c,
            |mut caller: Caller<'_, HostState>, ctx: i32, _depth: i32, _need: i32, ret: i64| -> wasmtime::Result<i32> { crate::host((|| -> anyhow::Result<i32> { fallback(&mut caller, ctx, ret as u64) })()) },
        )
    };
    let want = sh.lock().unwrap_or_else(|e| e.into_inner()).ftab.map.size() + HEADROOM;
    table.grow(&mut c, want as u64, wasmtime::Ref::Func(Some(resolve.clone())))?;
    table.set(&mut c, gex_abi::ftab::FALLBACK as u64, wasmtime::Ref::Func(Some(fallback)))?;
    let jit = c.data_mut().jit.as_mut().unwrap();
    jit.instances.push(inst);
    jit.ftab = Some(Store { table, resolve, bridge, root, provider_sp: None, provider_stack_lo: 0, provider_tag: None, replayed: 0 });
    Ok(())
}

/// Replay what this store's table has not seen of the map's log.
pub(crate) fn catch_up(mut cx: impl AsContextMut<Data = HostState>) -> Result<()> {
    ensure(cx.as_context_mut())?;
    let Some(jit) = cx.as_context().data().jit.as_ref() else { return Ok(()) };
    let Some(st) = jit.ftab.as_ref() else { return Ok(()) };
    let (sh, from, table, resolve) = (Arc::clone(&jit.shared), st.replayed, st.table, st.resolve.clone());
    #[allow(clippy::type_complexity)]
    let (events, size, installs, modules, stateful): (Vec<Event>, u32, Vec<gex_kernel::ftab::Install>, Vec<Module>, HashMap<u32, crate::libprov::Extras>) = {
        let s = sh.lock().unwrap_or_else(|e| e.into_inner());
        let ev = s.ftab.map.log_from(from).to_vec();
        if ev.is_empty() {
            return Ok(());
        }
        (ev, s.ftab.map.size(), s.ftab.map.installs().to_vec(), s.ftab.modules.clone(), s.ftab.stateful.clone())
    };
    let have = table.size(cx.as_context_mut());
    if (size as u64) > have {
        table.grow(cx.as_context_mut(), size as u64 + HEADROOM as u64 - have, wasmtime::Ref::Func(Some(resolve.clone())))?;
    }
    for ev in &events {
        match *ev {
            Event::Install(i) => {
                let inst = &installs[i as usize];
                let module = &modules[inst.module as usize];
                instantiate(cx.as_context_mut(), table, inst.base, module, stateful.get(&inst.module).copied())?;
            }
            Event::Retire(idx) => {
                table.set(cx.as_context_mut(), idx as u64, wasmtime::Ref::Func(Some(resolve.clone())))?;
            }
        }
    }
    if let Some(st) = cx.as_context_mut().data_mut().jit.as_mut().and_then(|j| j.ftab.as_mut()) {
        st.replayed = from + events.len();
    }
    Ok(())
}

/// Instantiate a module whose element segment fills `table` from `base`.
fn instantiate(
    mut cx: impl AsContextMut<Data = HostState>,
    table: Table,
    base: u32,
    module: &Module,
    stateful: Option<crate::libprov::Extras>,
) -> Result<wasmtime::Instance> {
    use gex_abi::ftab as f;
    let (linker, memory) = {
        let d = cx.as_context().data();
        (d.jit.as_ref().unwrap().linker.clone(), d.memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?)
    };
    let mut l = linker;
    l.allow_shadowing(true);
    // The PIC globals are pointer-sized: i64 for a wasm64 module beside a
    // 64-bit guest memory (docs/mem64.md).
    let i32c = |cx: &mut wasmtime::StoreContextMut<'_, HostState>, v: u64, m: wasmtime::Mutability| {
        if gex_abi::mem64() {
            wasmtime::Global::new(&mut *cx, wasmtime::GlobalType::new(wasmtime::ValType::I64, m), Val::I64(v as i64))
        } else {
            wasmtime::Global::new(&mut *cx, wasmtime::GlobalType::new(wasmtime::ValType::I32, m), Val::I32(v as u32 as i32))
        }
    };
    let mut c = cx.as_context_mut();
    l.define(&mut c, gex_abi::IMPORT_MODULE, exports::MEMORY, memory.shared().clone())?;
    l.define(&mut c, f::PIC_MODULE, "memory", memory.shared().clone())?;
    l.define(&mut c, gex_abi::IMPORT_MODULE, f::TABLE, table)?;
    l.define(&mut c, f::PIC_MODULE, f::PIC_TABLE, table)?;
    let g = i32c(&mut c, base as u64, wasmtime::Mutability::Const)?;
    l.define(&mut c, f::PIC_MODULE, f::PIC_TABLE_BASE, g)?;
    match stateful {
        // A stateful provider: its data where the host put it, and this
        // thread's own stack (`libprov`).
        Some(x) => {
            let g = i32c(&mut c, x.memory_base, wasmtime::Mutability::Const)?;
            l.define(&mut c, f::PIC_MODULE, f::PIC_MEMORY_BASE, g)?;
            let sp = crate::libprov::stack(&mut c)?;
            l.define(&mut c, f::PIC_MODULE, f::PIC_STACK_POINTER, sp)?;
        }
        None => {
            let g = i32c(&mut c, 0u64, wasmtime::Mutability::Const)?;
            l.define(&mut c, f::PIC_MODULE, f::PIC_MEMORY_BASE, g)?;
            // A unit's functions never touch clang's shadow stack (those are
            // stubs), but a position-independent module imports its pointer
            // all the same.
            let g = i32c(&mut c, 0u64, wasmtime::Mutability::Var)?;
            l.define(&mut c, f::PIC_MODULE, f::PIC_STACK_POINTER, g)?;
        }
    }
    jit_stats::INSTALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let inst = l.instantiate(&mut c, module).context("instantiating a function-table module")?;
    if let Some(x) = stateful {
        crate::libprov::relocate_once(&mut c, &inst, x)?;
    }
    c.data_mut().jit.as_mut().unwrap().instances.push(inst);
    Ok(inst)
}

/// Publish a module whose element segment holds `funcs` (guest address per
/// slot, 0 for none) as the process's newest functions for those
/// addresses, with the page checks it was translated under. Every store
/// instantiates it as it replays; tier-1 reaches the functions through
/// bridges (`publish_bridges`). Returns the install's number.
pub(crate) fn publish(
    sh: &Arc<Mutex<JitShared>>,
    memory: &gex_mem::GuestMemory,
    module: Module,
    funcs: &[u64],
    tier: Tier,
    checks: Vec<(u64, u32)>,
) -> Result<u32> {
    Ok(publish_at(sh, memory, module, funcs, tier, checks, None)?.expect("an unconditional publish"))
}

/// [`publish`], unless `epoch` is given and a retirement happened since
/// (then nothing, and `None`).
fn publish_at(
    sh: &Arc<Mutex<JitShared>>,
    memory: &gex_mem::GuestMemory,
    module: Module,
    funcs: &[u64],
    tier: Tier,
    checks: Vec<(u64, u32)>,
    epoch: Option<u64>,
) -> Result<Option<u32>> {
    let pairs: Vec<(u64, u32)> = funcs.iter().enumerate().map(|(k, &a)| (a, k as u32)).collect();
    publish_pairs_at(sh, memory, module, funcs.len() as u32, &pairs, tier, checks, epoch)
}

/// [`publish`] for a module whose slots are bound by explicit (guest
/// address, slot) pairs: several addresses may share a slot (an export
/// provider's function serving several exports).
pub(crate) fn publish_pairs(
    sh: &Arc<Mutex<JitShared>>,
    memory: &gex_mem::GuestMemory,
    module: Module,
    len: u32,
    pairs: &[(u64, u32)],
    tier: Tier,
) -> Result<u32> {
    let n = publish_pairs_at(sh, memory, module, len, pairs, tier, Vec::new(), None)?.expect("an unconditional publish");
    sh.lock().unwrap_or_else(|e| e.into_inner()).ftab.providers.insert(n);
    Ok(n)
}

/// [`publish_pairs`] for a stateful provider (`libprov`): every store
/// instantiates it with `extras`.
pub(crate) fn publish_stateful(
    sh: &Arc<Mutex<JitShared>>,
    memory: &gex_mem::GuestMemory,
    module: Module,
    len: u32,
    pairs: &[(u64, u32)],
    extras: crate::libprov::Extras,
) -> Result<u32> {
    {
        let mut s = sh.lock().unwrap_or_else(|e| e.into_inner());
        let m = s.ftab.modules.len() as u32;
        s.ftab.stateful.insert(m, extras);
    }
    publish_pairs(sh, memory, module, len, pairs, Tier::Unit)
}

/// Bind more addresses to slots of an existing install (`FuncMap::bind`):
/// one module serving several images.
pub(crate) fn bind_pairs(sh: &Arc<Mutex<JitShared>>, memory: &gex_mem::GuestMemory, install: u32, pairs: &[(u64, u32)]) -> Result<()> {
    let mut entries: Vec<u64> = pairs.iter().map(|&(a, _)| a).filter(|&a| a != 0).collect();
    entries.sort_unstable();
    entries.dedup();
    sh.lock().unwrap_or_else(|e| e.into_inner()).ftab.map.bind(install, pairs);
    let data = memory.bytes();
    for &a in &entries {
        gex_kernel::ftab::fcache_clear(data, a);
    }
    publish_bridges(sh, &entries, &[])
}

/// The function at `idx` of this store's table (a C function pointer of a
/// provider module: its table index).
pub(crate) fn table_func(cx: &mut Caller<'_, HostState>, idx: u32) -> Result<wasmtime::Func> {
    let t = cx.data().jit.as_ref().and_then(|j| j.ftab.as_ref()).map(|s| s.table).ok_or_else(|| anyhow!("no function table"))?;
    match t.get(&mut *cx, idx as u64) {
        Some(wasmtime::Ref::Func(Some(f))) => Ok(f),
        _ => bail!("function table index {idx} holds no function"),
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_pairs_at(
    sh: &Arc<Mutex<JitShared>>,
    memory: &gex_mem::GuestMemory,
    module: Module,
    len: u32,
    pairs: &[(u64, u32)],
    tier: Tier,
    checks: Vec<(u64, u32)>,
    epoch: Option<u64>,
) -> Result<Option<u32>> {
    let mut entries: Vec<u64> = pairs.iter().map(|&(a, _)| a).filter(|&a| a != 0).collect();
    entries.sort_unstable();
    entries.dedup();
    let n = {
        let mut s = sh.lock().unwrap_or_else(|e| e.into_inner());
        if epoch.is_some_and(|e| e != s.ftab.epoch) {
            return Ok(None);
        }
        let m = s.ftab.modules.len() as u32;
        crate::aot_share::note(
            &module,
            match tier {
                Tier::Unit => crate::aot_share::Kind::Unit,
                Tier::Jit => crate::aot_share::Kind::Jit,
                Tier::Region => crate::aot_share::Kind::Tier1,
            },
        );
        s.ftab.modules.push(module);
        let n = s.ftab.map.publish(len, m, tier, pairs);
        if !checks.is_empty() {
            s.ftab.checks.insert(n, checks.clone());
        }
        n
    };
    // An address the cache sent to the fallback (or to an older function)
    // looks again.
    let data = memory.bytes();
    for &a in &entries {
        gex_kernel::ftab::fcache_clear(data, a);
    }
    publish_bridges(sh, &entries, &checks)?;
    Ok(Some(n))
}

/// Point tier-1's dispatch slot for each of `entries` at the bridge, in
/// every store (a registry entry), numbering their pages first. `pages`
/// are pages a translation was numbered for (`Request::page_gen`), whose
/// directory entries must be written even with no entry on them.
pub(crate) fn publish_bridges(sh: &Arc<Mutex<JitShared>>, entries: &[u64], pages: &[(u64, u32)]) -> Result<()> {
    // No tier-1 dispatch to bridge from.
    if crate::aot_only::enabled() {
        return Ok(());
    }
    let mut s = sh.lock().unwrap_or_else(|e| e.into_inner());
    let mut dirs: Vec<(u64, u32)> = pages.to_vec();
    let mut slots = Vec::with_capacity(entries.len());
    for &a in entries {
        let page = a / gex_abi::DISPATCH_PAGE;
        if page >= gex_abi::DISPATCH_MAX_PAGES {
            continue;
        }
        let base = match s.page_table.lookup(page) {
            Some(b) => b,
            None => {
                let b = s.page_table.base(page);
                s.slot_owner.insert(b, page);
                b
            }
        };
        if gex_abi::dispatch_entry_table(base) != 0 {
            continue;
        }
        if !dirs.iter().any(|&(p, _)| p == page) {
            dirs.push((page, base));
        }
        slots.push(gex_abi::dispatch_index(a, base) as u32);
    }
    s.translated.extend(entries.iter().copied());
    let unit = Arc::new(CompiledUnit {
        module: None,
        exported_blocks: Vec::new(),
        dir_entries: dirs,
        table_slots: s.page_table.slots(),
        bridges: slots,
    });
    s.registry.push(unit);
    crate::REGISTRY_LEN.store(s.registry.len(), std::sync::atomic::Ordering::Release);
    Ok(())
}

/// The value `PAGE_GEN` holds for `page`, numbering the page if it has no
/// translation yet (returned to the caller in `fresh` so the directory is
/// written when the translation is published).
fn page_gen(sh: &Arc<Mutex<JitShared>>, memory: &gex_mem::GuestMemory, page: u64) -> u32 {
    if page >= gex_abi::DISPATCH_MAX_PAGES {
        return u32::MAX;
    }
    let off = (gex_abi::PAGE_GEN + 4 * page) as usize;
    let have = memory.bytes().get(off..off + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).unwrap_or(0);
    if have != 0 {
        return have;
    }
    let mut s = sh.lock().unwrap_or_else(|e| e.into_inner());
    match s.page_table.lookup(page) {
        Some(b) => b,
        None => {
            let b = s.page_table.base(page);
            s.slot_owner.insert(b, page);
            b
        }
    }
}

/// Whether the function `f` at `addr` may still run: a run-time
/// translation's pages hold the values it was made under; a unit's entry
/// page was not rewritten since it was installed.
fn current(s: &JitShared, memory: &[u8], addr: u64, f: gex_kernel::ftab::Func) -> bool {
    let inst = s.ftab.map.install(f.install);
    let word = |off: u64| memory.get(off as usize..off as usize + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).unwrap_or(0);
    if s.ftab.providers.contains(&f.install) {
        return true;
    }
    match inst.tier {
        Tier::Unit => {
            let page = addr / gex_abi::DISPATCH_PAGE;
            let byte = memory.get((gex_abi::PATCHED_PAGES + page / 8) as usize).copied().unwrap_or(0);
            byte & (1 << (page % 8)) == 0
        }
        _ => s
            .ftab
            .checks
            .get(&f.install)
            .is_none_or(|c| c.iter().all(|&(p, g)| word(gex_abi::PAGE_GEN + 4 * p) == g)),
    }
}

/// `GEX_FTAB_TRACE=1`: every host lookup that finds a function, said.
fn trace_lookups() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("GEX_FTAB_TRACE").is_some())
}

/// `gex.lookup(ctx, addr)`.
pub(crate) fn lookup(caller: &mut Caller<'_, HostState>, addr: u64) -> Result<i32> {
    use std::sync::atomic::Ordering::Relaxed;
    stats::LOOKUPS.fetch_add(1, Relaxed);
    catch_up(&mut *caller)?;
    let sh = shared(caller)?;
    let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    {
        let mut s = sh.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = s.ftab.map.get(addr) {
            if current(&s, memory.bytes(), addr, f) {
                if trace_lookups() {
                    let inst = s.ftab.map.install(f.install).clone();
                    drop(s);
                    let t = caller.data().jit.as_ref().and_then(|j| j.ftab.as_ref()).map(|st| st.table);
                    let ty = t.and_then(|t| t.get(&mut *caller, f.idx as u64)).and_then(|r| r.as_func().flatten().map(|f| format!("{:?}", f.ty(&*caller))));
                    // x86-64: the guest's return address, at [rsp] (register 4) on a call.
                    let ctx_addr = caller.data().thread.ctx_addr as usize;
                    let rd = |a: usize| memory.bytes().get(a..a + 8).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()));
                    let rsp = rd(ctx_addr + gex_abi::ctx::X + 4 * 8);
                    let ret = rd(rsp as usize);
                    eprintln!("[ftab] lookup {addr:#x} -> index {} (install {}, module {}, base {}, {:?}) ret {ret:#x} holds {ty:?}", f.idx, f.install, inst.module, inst.base, inst.tier);
                    gex_kernel::ftab::fcache_write(memory.bytes(), addr, f.idx);
                    return Ok(f.idx as i32);
                }
                drop(s);
                gex_kernel::ftab::fcache_write(memory.bytes(), addr, f.idx);
                return Ok(f.idx as i32);
            }
            stats::STALE.fetch_add(1, Relaxed);
            s.ftab.map.retire(addr);
            s.ftab.queued.remove(&addr);
            drop(s);
            gex_kernel::ftab::fcache_clear(memory.bytes(), addr);
            catch_up(&mut *caller)?;
        }
    }
    if background() {
        // Tier-1 (the fallback) runs it meanwhile; the publication clears
        // this address's cache entry, so the next call finds the function.
        // The cache entry first: a publication racing ahead clears it.
        stats::FALLBACKS.fetch_add(1, Relaxed);
        gex_kernel::ftab::fcache_write(memory.bytes(), addr, gex_abi::ftab::FALLBACK);
        let fresh = sh.lock().unwrap_or_else(|e| e.into_inner()).ftab.queued.insert(addr);
        if fresh {
            if let Some(j) = job(caller, &sh, &memory, addr) {
                queue(j);
            }
        }
        return Ok(gex_abi::ftab::FALLBACK as i32);
    } else if let Some(idx) = translate(caller, &sh, &memory, addr)? {
        gex_kernel::ftab::fcache_write(memory.bytes(), addr, idx);
        return Ok(idx as i32);
    }
    stats::FALLBACKS.fetch_add(1, Relaxed);
    gex_kernel::ftab::fcache_write(memory.bytes(), addr, gex_abi::ftab::FALLBACK);
    Ok(gex_abi::ftab::FALLBACK as i32)
}

/// Whether misses are translated on worker threads while the fallback
/// runs the code (`GEX_FJIT_BACKGROUND=1`), or on the missing thread before
/// it carries on (the default). Natively the fallback is tier-1, whose
/// translation of the same code costs about as much as the JIT's (it is
/// compiled by the same Cranelift, a block at a time): tests/nt sync runs
/// 6.0 s (x86) and 9.9 s (x64) in the background against 4.0 s and 3.9 s
/// inline. Worth it only with a cheap fallback (the browser's, or an
/// interpreter).
pub(crate) fn background() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("GEX_FJIT_BACKGROUND").is_some())
}

/// One translation's inputs, taken on the guest thread so that any thread
/// can do it: the mappings as they are now, and the retirement epoch they
/// belong to.
struct Job {
    process: u64,
    sh: Arc<Mutex<JitShared>>,
    memory: gex_mem::GuestMemory,
    engine: wasmtime::Engine,
    addr: u64,
    exec: Vec<(u64, u64)>,
    ro: Vec<(u64, u64)>,
    /// Writable-and-executable ranges (a JIT's code), which the function
    /// JIT does not translate.
    wx: Vec<(u64, u64)>,
    /// Addresses ELF providers hold (`elfprov`): not code to translate.
    provided: Arc<std::collections::HashSet<u64>>,
    class32: bool,
    epoch: u64,
    queued_at: std::time::Instant,
    /// The module cache (directory, build), when it is on.
    cache: Option<(std::path::PathBuf, String)>,
}

/// `GEX_FJIT_RANGE=LO-HI[,LO-HI...]` (hex guest addresses): the function JIT
/// takes only functions entered in those ranges; `GEX_FJIT_SKIP=` the same
/// form, never those. For bisecting a translation bug down to one function.
fn bisect_allows(addr: u64) -> bool {
    type Ranges = Option<Vec<(u64, u64)>>;
    fn parse(var: &str) -> Ranges {
        let v = std::env::var(var).ok()?;
        let hex = |s: &str| u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
        Some(v.split(',').filter_map(|r| { let (a, b) = r.split_once('-')?; Some((hex(a)?, hex(b)?)) }).collect())
    }
    static R: std::sync::OnceLock<(Ranges, Ranges)> = std::sync::OnceLock::new();
    let (only, skip) = R.get_or_init(|| (parse("GEX_FJIT_RANGE"), parse("GEX_FJIT_SKIP")));
    let inr = |rs: &Vec<(u64, u64)>| rs.iter().any(|&(s, e)| addr >= s && addr < e);
    only.as_ref().is_none_or(inr) && !skip.as_ref().is_some_and(inr)
}

fn job(caller: &mut Caller<'_, HostState>, sh: &Arc<Mutex<JitShared>>, memory: &gex_mem::GuestMemory, addr: u64) -> Option<Job> {
    gex_kernel::ftab::translator()?;
    if !bisect_allows(addr) {
        return None;
    }
    let (exec, ro, wx): (Vec<(u64, u64)>, Vec<(u64, u64)>, Vec<(u64, u64)>) = caller.data_mut().with_kernel(|k| {
        let v = k.mem.vmas();
        (
            // A library a stateful provider replaced is not code to translate (`libprov::replaced`).
            v.iter().filter(|v| v.prot & prot::EXEC != 0 && !crate::libprov::replaced(v.start, v.end)).map(|v| (v.start, v.end)).collect(),
            v.iter().filter(|v| v.prot & prot::WRITE == 0 && v.prot & prot::READ != 0).map(|v| (v.start, v.end)).collect(),
            v.iter().filter(|v| v.prot & prot::EXEC != 0 && v.prot & prot::WRITE != 0).map(|v| (v.start, v.end)).collect(),
        )
    });
    if !exec.iter().any(|&(s, e)| addr >= s && addr < e) {
        return None;
    }
    let (epoch, process) = {
        let s = sh.lock().unwrap_or_else(|e| e.into_inner());
        (s.ftab.epoch, s.ftab.process.0)
    };
    Some(Job {
        process,
        sh: Arc::clone(sh),
        memory: memory.clone(),
        engine: caller.engine().clone(),
        addr,
        exec,
        ro,
        wx,
        provided: crate::elfprov::provided(sh),
        class32: gex_abi::X86 && gex_xlate::arch::compat(),
        epoch,
        queued_at: std::time::Instant::now(),
        cache: caller.data().jit.as_ref().and_then(|jit| jit.cache.clone()),
    })
}

/// Translate, compile and publish `j` (into no store: each catches up as it
/// replays). False when the translator declined or a retirement since the
/// job was taken made it stale.
fn run_job(j: &Job) -> Result<bool> {
    if cfg!(not(feature = "translate")) {
        return Ok(false);
    }
    use std::sync::atomic::Ordering::Relaxed;
    let Some(tr) = gex_kernel::ftab::translator() else { return Ok(false) };
    let (sh, memory, addr) = (&j.sh, &j.memory, j.addr);
    let executable = |a: u64| !j.provided.contains(&a) && j.exec.iter().any(|&(s, e)| a >= s && a < e);
    let read_only = |a: u64, n: u64| j.ro.iter().any(|&(s, e)| a >= s && a.saturating_add(n) <= e);
    let volatile = |a: u64| j.wx.iter().any(|&(s, e)| a >= s && a < e);
    let pages = Mutex::new(Vec::<(u64, u32)>::new());
    let pg = |page: u64| {
        let g = page_gen(sh, memory, page);
        let mut p = pages.lock().unwrap_or_else(|e| e.into_inner());
        if !p.iter().any(|&(q, _)| q == page) {
            p.push((page, g));
        }
        g
    };
    let req = gex_kernel::ftab::Request {
        process: j.process,
        mem: memory.bytes(),
        addr,
        executable: &executable,
        read_only: &read_only,
        volatile: &volatile,
        page_gen: &pg,
        class32: j.class32,
    };
    let t0 = std::time::Instant::now();
    let t = tr.translate(&req);
    stats::TRANSLATE_US.fetch_add(t0.elapsed().as_micros() as u64, Relaxed);
    let Some(t) = t else { return Ok(false) };
    let funcs = gex_kernel::ftab::module_funcs(&t.wasm).ok_or_else(|| anyhow!("a translation for {addr:#x} has no gex.funcs section"))?;
    if !funcs.contains(&addr) {
        bail!("the translation for {addr:#x} does not hold it");
    }
    let t1 = std::time::Instant::now();
    let module = compile_translation(j, &t.wasm).with_context(|| format!("compiling the translation for {addr:#x}"))?;
    let tc = t1.elapsed();
    stats::COMPILE_US.fetch_add(tc.as_micros() as u64, Relaxed);
    let checks = pages.into_inner().unwrap_or_else(|e| e.into_inner());
    if publish_at(sh, memory, module, &funcs, Tier::Jit, checks, Some(j.epoch))?.is_none() {
        stats::DROPPED.fetch_add(1, Relaxed);
        return Ok(false);
    }
    stats::TRANSLATED.fetch_add(1, Relaxed);
    stats::LATENCY_US.fetch_add(j.queued_at.elapsed().as_micros() as u64, Relaxed);
    stats::WORST_US.fetch_max(t0.elapsed().as_micros() as u64, Relaxed);
    if std::env::var("GEX_FTAB_SLOW").ok().and_then(|v| v.parse::<u128>().ok()).is_some_and(|ms| t0.elapsed().as_millis() >= ms) {
        eprintln!("[ftab] slow translation {addr:#x}: {} ms (translate {} ms, compile {} ms, {} bytes)", t0.elapsed().as_millis(), (t1 - t0).as_millis(), tc.as_millis(), t.wasm.len());
    }
    Ok(true)
}

/// Hand `j` to the background translators (`GEX_FJIT_WORKERS`, default 2),
/// started at the first need.
fn queue(j: Job) {
    static TX: std::sync::OnceLock<Mutex<std::sync::mpsc::Sender<Job>>> = std::sync::OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let n = std::env::var("GEX_FJIT_WORKERS").ok().and_then(|v| v.parse().ok()).unwrap_or(2usize).max(1);
        for k in 0..n {
            let rx = Arc::clone(&rx);
            let _ = std::thread::Builder::new().name(format!("gex-fjit-{k}")).spawn(move || loop {
                let next = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                let Ok(j) = next else { return };
                if let Err(e) = run_job(&j) {
                    eprintln!("[fjit] {:#x}: {e:#}; left to the fallback", j.addr);
                }
            });
        }
        Mutex::new(tx)
    });
    stats::QUEUED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _ = tx.lock().unwrap_or_else(|e| e.into_inner()).send(j);
}

/// Translate `addr` on this thread, install what it makes into this store
/// and say which index serves `addr`.
/// Compile a run-time translation. With `GEX_FJIT_CACHE=1` (and the
/// module cache on), the compiled module is kept on disk by the wasm it came
/// from, so a program's next run loads instead of compiling: a large
/// program's start-up under the function JIT is mostly Cranelift (Unity's:
/// about 60% of its CPU). Off by default, since a large program leaves
/// gigabytes of them.
fn compile_translation(j: &Job, wasm: &[u8]) -> Result<Module> {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let on = *ON.get_or_init(|| std::env::var("GEX_FJIT_CACHE").is_ok_and(|v| !v.is_empty() && v != "0"));
    match j.cache.as_ref().filter(|_| on) {
        Some((dir, build)) => {
            let key = gex_cache::key(build, [b"fjit".as_slice(), wasm]);
            gex_cache::load_or_compile_at(&j.engine, &dir.join("fjit").join(format!("{key}.cwasm")), wasm)
        }
        None => gex_cache::compile(&j.engine, wasm),
    }
}

fn translate(caller: &mut Caller<'_, HostState>, sh: &Arc<Mutex<JitShared>>, memory: &gex_mem::GuestMemory, addr: u64) -> Result<Option<u32>> {
    let Some(j) = job(caller, sh, memory, addr) else { return Ok(None) };
    if !run_job(&j)? {
        return Ok(None);
    }
    catch_up(&mut *caller)?;
    replay_registry(caller)?;
    Ok(sh.lock().unwrap_or_else(|e| e.into_inner()).ftab.map.get(addr).map(|f| f.idx))
}

/// A dispatch miss at `pc` (tier-1's miss handler asks first): if a
/// function serves it, or the run-time translator makes one, bridge its
/// slot and say so. Without a translator only addresses already in the
/// table are served (their bridges were published with them).
pub(crate) fn serve(caller: &mut Caller<'_, HostState>, pc: u64) -> Result<bool> {
    let known = {
        let sh = shared(caller)?;
        let s = sh.lock().unwrap_or_else(|e| e.into_inner());
        if s.ftab.tier1_only.contains(&pc) {
            return Ok(false);
        }
        s.ftab.map.get(pc).is_some()
    };
    if !known && gex_kernel::ftab::translator().is_none() {
        return Ok(false);
    }
    if lookup(caller, pc)? == gex_abi::ftab::FALLBACK as i32 {
        return Ok(false);
    }
    let sh = shared(caller)?;
    publish_bridges(&sh, &[pc], &[])?;
    replay_registry(caller)?;
    Ok(true)
}

/// Index 1: code no function holds, under the `F_body` contract -- tier-1
/// run nested until the guest returns to `ret` (the call-out).
fn fallback(caller: &mut Caller<'_, HostState>, ctx: i32, ret: u64) -> Result<i32> {
    let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let gm = GuestMem::new(memory.bytes());
    let c = ctx as u32 as u64;
    let target = gm.read_u64(c + ctx::LAST_TARGET as u64).unwrap_or(0);
    let sp = gm.read_u64(c + ctx::x(sp_index()) as u64).unwrap_or(0);
    let delta = match (gex_abi::X86, gex_xlate::arch::compat()) {
        (false, _) => 0,
        (true, false) => 8,
        (true, true) => 4,
    };
    let _ = GuestMem::new(memory.bytes()).write_u64(c + ctx::PC as u64, target);
    if caller.data().nt.is_some() && crate::profile::active() {
        crate::profile::note_nt_entry(target);
    }
    if stats::FALLBACK_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & 1023 == 0 {
        *stats::FALLBACK_SITES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).entry(target).or_default() += 1;
    }
    // A jump out of an exception being dispatched nested (a handler's
    // longjmp to its `setjmp`, a `catch` continuation): back to the frames
    // it was raised under.
    if crate::nt::exc_escape(caller, target) {
        return Err(StopExecution.into());
    }
    // No caller waits for a return (a root, or the bridge): leave, and the
    // root dispatches there flat.
    if ret == gex_abi::ftab::NO_RETURN {
        return Ok(gex_abi::ftab::RET_LEFT);
    }
    call_out(caller, ctx, target, ret, sp + delta)
}

/// Write the bridge into this store's dispatch table at `slots`.
pub(crate) fn install_bridges(mut cx: impl AsContextMut<Data = HostState>, slots: &[u32]) -> Result<()> {
    ensure(cx.as_context_mut())?;
    let jit = cx.as_context().data().jit.as_ref().unwrap();
    let (t0, bridge) = (jit.tables[0], jit.ftab.as_ref().unwrap().bridge.clone());
    for &s in slots {
        t0.set(cx.as_context_mut(), s as u64, wasmtime::Ref::Func(Some(bridge.clone())))?;
    }
    Ok(())
}

/// Retire the run-time translations whose entries lie in the retired
/// ranges, and clear their probe cache entries (`retire_translations`).
/// Their page checks would catch them anyway; this keeps the cache from
/// sending callers there first.
pub(crate) fn on_retire(sh: &mut JitShared, memory: &mut [u8], ranges: &[(u64, u64)]) {
    sh.ftab.epoch += 1;
    sh.ftab.queued.retain(|&a| !ranges.iter().any(|&(s, e)| a >= s && a < e));
    for &(start, end) in ranges {
        let hit: Vec<(u64, gex_kernel::ftab::Func)> = sh.ftab.map.range(start, end).collect();
        for (a, f) in hit {
            gex_kernel::ftab::fcache_clear(memory, a);
            if sh.ftab.map.install(f.install).tier != Tier::Unit {
                sh.ftab.map.retire(a);
            }
        }
    }
}

/// This store's run loop, once its table exists.
pub(crate) fn root_of(cx: &impl AsContext<Data = HostState>) -> Option<wasmtime::Func> {
    cx.as_context().data().jit.as_ref()?.ftab.as_ref().map(|s| s.root.clone())
}

/// The resolve stub's funcref, for a host that fills a table of its own.
#[allow(dead_code)]
pub(crate) fn resolve_of(cx: &impl AsContext<Data = HostState>) -> Option<wasmtime::Func> {
    cx.as_context().data().jit.as_ref()?.ftab.as_ref().map(|s| s.resolve.clone())
}

/// `GEX_FTAB_SELFTEST=1`: a run-time "translator" that serves every address
/// a dispatch misses on with `gex_cgen::runtime::forwarder_module` -- a
/// function that only re-enters the fallback. Every path of the table's host
/// half runs (lookup, translation, install in every store, bridges, the
/// call-out behind index 1), and the program must behave exactly as without.
struct Forwarder;

impl gex_kernel::ftab::Translator for Forwarder {
    fn translate(&self, req: &gex_kernel::ftab::Request<'_>) -> Option<gex_kernel::ftab::Translated> {
        let _ = (req.page_gen)(req.addr / gex_abi::DISPATCH_PAGE);
        Some(gex_kernel::ftab::Translated { wasm: gex_cgen::runtime::forwarder_module(&[req.addr]) })
    }
}

pub(crate) fn selftest_init() {
    if cfg!(not(feature = "translate")) {
        return;
    }
    if std::env::var_os("GEX_FTAB_SELFTEST").is_some() {
        gex_kernel::ftab::set_translator(Box::new(Forwarder));
    }
}
