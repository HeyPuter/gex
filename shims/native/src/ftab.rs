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
}

/// One store's half, inside [`Jit`].
pub(crate) struct Store {
    pub table: Table,
    pub resolve: wasmtime::Func,
    pub bridge: wasmtime::Func,
    /// How far through the map's log this table is.
    replayed: usize,
}

/// `GEX_NT_STATS` counters: host lookups, run-time translations, lookups
/// answered with the fallback, stale functions found on a lookup.
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
    pub fn line() -> String {
        use std::sync::atomic::Ordering::Relaxed;
        format!(
            "ftab lookups={} translated={} fallback={} stale={} resumes={}",
            LOOKUPS.load(Relaxed),
            TRANSLATED.load(Relaxed),
            FALLBACKS.load(Relaxed),
            STALE.load(Relaxed),
            RESUMES.load(Relaxed)
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
                let m = Module::new(cx.as_context().engine(), gex_cgen::runtime::module())
                    .context("compiling the function table's runtime module")?;
                s.ftab.runtime = Some(m.clone());
                m
            }
        }
    };
    let table = Table::new(cx.as_context_mut(), wasmtime::TableType::new(wasmtime::RefType::FUNCREF, 0, None), wasmtime::Ref::Func(None))?;
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
    let fallback = wasmtime::Func::wrap(
        &mut c,
        |mut caller: Caller<'_, HostState>, ctx: i32, _depth: i32, _need: i32, ret: i64| -> Result<i32> { fallback(&mut caller, ctx, ret as u64) },
    );
    let want = sh.lock().unwrap_or_else(|e| e.into_inner()).ftab.map.size() + HEADROOM;
    table.grow(&mut c, want as u64, wasmtime::Ref::Func(Some(resolve.clone())))?;
    table.set(&mut c, gex_abi::ftab::FALLBACK as u64, wasmtime::Ref::Func(Some(fallback)))?;
    let jit = c.data_mut().jit.as_mut().unwrap();
    jit.instances.push(inst);
    jit.ftab = Some(Store { table, resolve, bridge, replayed: 0 });
    Ok(())
}

/// Replay what this store's table has not seen of the map's log.
pub(crate) fn catch_up(mut cx: impl AsContextMut<Data = HostState>) -> Result<()> {
    ensure(cx.as_context_mut())?;
    let Some(jit) = cx.as_context().data().jit.as_ref() else { return Ok(()) };
    let Some(st) = jit.ftab.as_ref() else { return Ok(()) };
    let (sh, from, table, resolve) = (Arc::clone(&jit.shared), st.replayed, st.table, st.resolve.clone());
    let (events, size, installs, modules): (Vec<Event>, u32, Vec<gex_kernel::ftab::Install>, Vec<Module>) = {
        let s = sh.lock().unwrap_or_else(|e| e.into_inner());
        let ev = s.ftab.map.log_from(from).to_vec();
        if ev.is_empty() {
            return Ok(());
        }
        (ev, s.ftab.map.size(), s.ftab.map.installs().to_vec(), s.ftab.modules.clone())
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
                instantiate(cx.as_context_mut(), table, inst.base, module)?;
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
fn instantiate(mut cx: impl AsContextMut<Data = HostState>, table: Table, base: u32, module: &Module) -> Result<wasmtime::Instance> {
    use gex_abi::ftab as f;
    let (linker, memory) = {
        let d = cx.as_context().data();
        (d.jit.as_ref().unwrap().linker.clone(), d.memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?)
    };
    let mut l = linker;
    l.allow_shadowing(true);
    let i32c = |cx: &mut wasmtime::StoreContextMut<'_, HostState>, v: i32, m: wasmtime::Mutability| {
        wasmtime::Global::new(&mut *cx, wasmtime::GlobalType::new(wasmtime::ValType::I32, m), Val::I32(v))
    };
    let mut c = cx.as_context_mut();
    l.define(&mut c, gex_abi::IMPORT_MODULE, exports::MEMORY, memory.shared().clone())?;
    l.define(&mut c, f::PIC_MODULE, "memory", memory.shared().clone())?;
    l.define(&mut c, gex_abi::IMPORT_MODULE, f::TABLE, table)?;
    l.define(&mut c, f::PIC_MODULE, f::PIC_TABLE, table)?;
    let g = i32c(&mut c, base as i32, wasmtime::Mutability::Const)?;
    l.define(&mut c, f::PIC_MODULE, f::PIC_TABLE_BASE, g)?;
    let g = i32c(&mut c, 0, wasmtime::Mutability::Const)?;
    l.define(&mut c, f::PIC_MODULE, f::PIC_MEMORY_BASE, g)?;
    // A unit's functions never touch clang's shadow stack (those are stubs),
    // but a position-independent module imports its pointer all the same.
    let g = i32c(&mut c, 0, wasmtime::Mutability::Var)?;
    l.define(&mut c, f::PIC_MODULE, f::PIC_STACK_POINTER, g)?;
    jit_stats::INSTALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let inst = l.instantiate(&mut c, module).context("instantiating a function-table module")?;
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
    let entries: Vec<u64> = funcs.iter().copied().filter(|&a| a != 0).collect();
    let n = {
        let mut s = sh.lock().unwrap_or_else(|e| e.into_inner());
        let m = s.ftab.modules.len() as u32;
        s.ftab.modules.push(module);
        let pairs: Vec<(u64, u32)> = funcs.iter().enumerate().map(|(k, &a)| (a, k as u32)).collect();
        let n = s.ftab.map.publish(funcs.len() as u32, m, tier, &pairs);
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
    Ok(n)
}

/// Point tier-1's dispatch slot for each of `entries` at the bridge, in
/// every store (a registry entry), numbering their pages first. `pages`
/// are pages a translation was numbered for (`Request::page_gen`), whose
/// directory entries must be written even with no entry on them.
pub(crate) fn publish_bridges(sh: &Arc<Mutex<JitShared>>, entries: &[u64], pages: &[(u64, u32)]) -> Result<()> {
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
                drop(s);
                gex_kernel::ftab::fcache_write(memory.bytes(), addr, f.idx);
                return Ok(f.idx as i32);
            }
            stats::STALE.fetch_add(1, Relaxed);
            s.ftab.map.retire(addr);
            drop(s);
            gex_kernel::ftab::fcache_clear(memory.bytes(), addr);
            catch_up(&mut *caller)?;
        }
    }
    if let Some(idx) = translate(caller, &sh, &memory, addr)? {
        gex_kernel::ftab::fcache_write(memory.bytes(), addr, idx);
        return Ok(idx as i32);
    }
    stats::FALLBACKS.fetch_add(1, Relaxed);
    gex_kernel::ftab::fcache_write(memory.bytes(), addr, gex_abi::ftab::FALLBACK);
    Ok(gex_abi::ftab::FALLBACK as i32)
}

/// Ask the run-time translator for `addr`, install what it makes into this
/// store and say which index serves `addr`.
fn translate(caller: &mut Caller<'_, HostState>, sh: &Arc<Mutex<JitShared>>, memory: &gex_mem::GuestMemory, addr: u64) -> Result<Option<u32>> {
    let Some(tr) = gex_kernel::ftab::translator() else { return Ok(None) };
    let exec: Vec<(u64, u64)> = caller.data_mut().with_kernel(|k| {
        k.mem.vmas().iter().filter(|v| v.prot & prot::EXEC != 0).map(|v| (v.start, v.end)).collect()
    });
    let executable = |a: u64| exec.iter().any(|&(s, e)| a >= s && a < e);
    if !executable(addr) {
        return Ok(None);
    }
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
        mem: memory.bytes(),
        addr,
        executable: &executable,
        page_gen: &pg,
        class32: gex_abi::X86 && gex_xlate::arch::compat(),
    };
    let Some(t) = tr.translate(&req) else { return Ok(None) };
    let funcs = gex_kernel::ftab::module_funcs(&t.wasm).ok_or_else(|| anyhow!("a translation for {addr:#x} has no gex.funcs section"))?;
    if !funcs.contains(&addr) {
        bail!("the translation for {addr:#x} does not hold it");
    }
    let engine = caller.engine().clone();
    let module = Module::new(&engine, &t.wasm).with_context(|| format!("compiling the translation for {addr:#x}"))?;
    stats::TRANSLATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let checks = pages.into_inner().unwrap_or_else(|e| e.into_inner());
    publish(sh, memory, module, &funcs, Tier::Jit, checks)?;
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
    if stats::FALLBACK_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & 1023 == 0 {
        *stats::FALLBACK_SITES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).entry(target).or_default() += 1;
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
    if std::env::var_os("GEX_FTAB_SELFTEST").is_some() {
        gex_kernel::ftab::set_translator(Box::new(Forwarder));
    }
}
