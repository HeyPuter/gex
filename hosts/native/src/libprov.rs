//! Stateful export providers: a library of native
//! wasm in place of a guest DLL's exports, with data, a C stack and a heap of
//! its own -- what gex-cgen's built-in providers (no data, no stack) cannot
//! be. A Lua (PUC Lua 5.1 in place of LuaJIT's lua51.dll) is one.
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
    let base = new_stack(cx.data_mut(), &format!("[provider stack, thread {tid}]"))?;
    let top = base + STACK - 16;
    if std::env::var_os("GEX_PROVIDER_STACKS").is_some() {
        eprintln!("gex: provider stack for thread {tid}: [{base:#x}, {:#x})", base + STACK);
    }
    // Pointer-sized: i64 for a wasm64 provider beside a 64-bit guest memory.
    let g = if gex_abi::mem64() {
        wasmtime::Global::new(&mut *cx, wasmtime::GlobalType::new(wasmtime::ValType::I64, wasmtime::Mutability::Var), Val::I64(top as i64))?
    } else {
        wasmtime::Global::new(&mut *cx, wasmtime::GlobalType::new(wasmtime::ValType::I32, wasmtime::Mutability::Var), Val::I32(top as i32))?
    };
    if let Some(s) = cx.data_mut().jit.as_mut().and_then(|j| j.ftab.as_mut()) {
        s.provider_sp = Some(g.clone());
        s.provider_stack_lo = base;
    }
    Ok(g)
}

/// This store's i64 stack pointer for lowered LP64 modules (64-bit Wine: built for
/// wasm64, run in a 32-bit memory; `ftab::instantiate`), with a stack of its
/// own beside [`stack`]'s, made on first use. A 64-bit Wine thread's loader and
/// Wine's PE module share it, as 32-bit Wine's share [`stack`].
pub(crate) fn stack64(cx: &mut wasmtime::StoreContextMut<'_, HostState>) -> Result<wasmtime::Global> {
    if let Some(g) = cx.data().jit.as_ref().and_then(|j| j.ftab.as_ref()).and_then(|s| s.provider_sp64.clone()) {
        return Ok(g);
    }
    let tid = cx.data().thread.tid;
    let base = new_stack(cx.data_mut(), &format!("[provider stack (lp64), thread {tid}]"))?;
    let top = base + STACK - 16;
    let g = wasmtime::Global::new(&mut *cx, wasmtime::GlobalType::new(wasmtime::ValType::I64, wasmtime::Mutability::Var), Val::I64(top as i64))?;
    if let Some(s) = cx.data_mut().jit.as_mut().and_then(|j| j.ftab.as_mut()) {
        s.provider_sp64 = Some(g.clone());
        s.provider_stack64_lo = base;
    }
    Ok(g)
}

/// A provider stack for a thread starting: one an ended thread of this
/// process left ([`free_stacks`]), or a new one. Top down when only
/// built-in providers are stateful, so the first call (a DllMain's malloc)
/// does not move the DLLs mapped after it; an ejected project's sessions
/// were recorded with it bottom-up.
fn new_stack(state: &mut HostState, name: &str) -> Result<u64> {
    if let Some(base) = state.spawn.as_ref().and_then(|s| s.provider_stacks.lock().unwrap_or_else(|e| e.into_inner()).pop()) {
        return Ok(base);
    }
    if LIBS.get().map_or(true, |v| v.is_empty()) { alloc_top(state, STACK, name) } else { alloc(state, STACK, name) }
}

/// An ending thread's provider stacks, back to its process for the next
/// threads. Kept mapped rather than unmapped: Wine has a view of each
/// (`note_host_data`) that it never drops.
pub(crate) fn free_stacks(state: &mut HostState) {
    let Some(s) = state.jit.as_mut().and_then(|j| j.ftab.as_mut()) else { return };
    let lo = [std::mem::take(&mut s.provider_stack_lo), std::mem::take(&mut s.provider_stack64_lo)];
    (s.provider_sp, s.provider_sp64) = (None, None);
    if let Some(spawn) = state.spawn.as_ref() {
        spawn.provider_stacks.lock().unwrap_or_else(|e| e.into_inner()).extend(lo.into_iter().filter(|&b| b != 0));
    }
}

/// Guest pages for a provider's own use.
pub(crate) fn alloc(state: &mut HostState, size: u64, name: &str) -> Result<u64> {
    alloc_linux(state, size, name)
}

/// Guest pages in a Linux process (a native wasm32 program, `wasmprog`):
/// an anonymous private mapping, as the program's own `mmap` would get.
fn alloc_linux(state: &mut HostState, size: u64, name: &str) -> Result<u64> {
    use gex_kernel::mem::prot;
    let len = (size + 0xfff) & !0xfff;
    let at = map_anon(state, 0, len, prot::READ | prot::WRITE, 0, false, name).map_err(|e| anyhow!("mapping {name}: errno {}", e.0))?;
    note_host_data(state, at, len);
    Ok(at)
}

/// Note a native module's memory for Wine's views (`MemoryMap::note_host_data`).
pub(crate) fn note_host_data(state: &mut HostState, at: u64, len: u64) {
    state.with_kernel(|k| k.mem.lock().note_host_data(at, at + len));
}

/// An anonymous mapping for the host's own use, zeroed as a guest's is
/// (`Mm::mmap_anon`): a wine32 process places by first fit, so the range
/// may be one Wine's own mappings used and unmapped.
fn map_anon(state: &mut HostState, addr: u64, len: u64, prot_bits: u32, extra: u64, low: bool, name: &str) -> std::result::Result<u64, gex_kernel::Errno> {
    let memory = state.memory.clone().ok_or(gex_kernel::Errno::EFAULT)?;
    let mut gm = GuestMem::new(memory.bytes());
    state.with_kernel(|k| k.mem.mmap_anon(&mut gm, addr, len, prot_bits, extra, low, name))
}

/// Data a 64-bit skeleton's export table points at (64-bit Wine, Wine's PE module:
/// wasmdll::install_shared): above every image, so the RVA from any
/// skeleton's base is positive. 64-bit Wine's images stay in its code space, below
/// 2 GiB (Wine's wine64_code_space_limit); its data may use the whole 4 GiB.
/// Top-down from 4 GiB, the first free range.
pub(crate) fn alloc_above_code(state: &mut HostState, size: u64, name: &str) -> Result<u64> {
    use gex_kernel::mem::{flags, prot};
    const FLOOR: u64 = 0x8000_0000;
    const STEP: u64 = 0x10_0000;
    let len = (size + 0xffff) & !0xffff;
    let mut at = (0x1_0000_0000u64.saturating_sub(len + STEP)) & !0xffff;
    while at >= FLOOR {
        let r = map_anon(state, at, len, prot::READ | prot::WRITE, flags::FIXED_NOREPLACE, false, name);
        if let Ok(a) = r {
            return Ok(a);
        }
        at -= STEP;
    }
    bail!("mapping {name}: no {len:#x} bytes free above {FLOOR:#x}")
}

/// Data an i386 skeleton's export table points at in a WoW64 process (the
/// other mode's native PE modules): above the images, so the RVA from any
/// skeleton's base is positive, and below 2 GiB, where a 32-bit program that
/// is not large-address-aware may see it (its pointers signed, and
/// `NtQueryVirtualMemory` refuses addresses past its highest user address).
/// Top-down from Windows' KUSER_SHARED_DATA page, the first free range.
pub(crate) fn alloc_below_2g(state: &mut HostState, size: u64, name: &str) -> Result<u64> {
    use gex_kernel::mem::{flags, prot};
    const FLOOR: u64 = 0x2000_0000;
    const STEP: u64 = 0x1_0000;
    let len = (size + 0xffff) & !0xffff;
    let mut at = (gex_abi::KUSER_WINDOWS.saturating_sub(len)) & !0xffff;
    while at >= FLOOR {
        if let Ok(a) = map_anon(state, at, len, prot::READ | prot::WRITE, flags::FIXED_NOREPLACE, false, name) {
            note_host_data(state, a, len);
            return Ok(a);
        }
        at -= STEP;
    }
    bail!("mapping {name}: no {len:#x} bytes free in [{FLOOR:#x}, {:#x})", gex_abi::KUSER_WINDOWS)
}

/// 64-bit Wine: the window for its PE modules' data ([`alloc_above_code_data`]),
/// reserved when its program is placed ([`reserve_pe_data`]), before any
/// Windows mapping: above Wine's code space, so above every image. On
/// demand, a program that reserves all it can of the space above 2 GiB
/// (a runtime's heap) left no room for the next module's data, and its
/// process ended. 8 MiB: the core's and every Wine module of its own a
/// process loads, packed by the page.
pub(crate) const PE_DATA: (u64, u64) = (0x8000_0000, 0x8080_0000);
const PE_DATA_NAME: &str = "[w64 PE data window]";

/// Reserve [`PE_DATA`] (no access until data is placed in it).
pub(crate) fn reserve_pe_data(state: &mut HostState) {
    use gex_kernel::mem::{flags, prot};
    let (lo, hi) = PE_DATA;
    if let Err(e) = map_anon(state, lo, hi - lo, prot::NONE, flags::FIXED_NOREPLACE, false, PE_DATA_NAME) {
        eprintln!("gex: wine64: reserving the PE data window {lo:#x}..{hi:#x}: errno {}", e.0);
    }
}

/// `len` bytes of [`PE_DATA`], from the top of what is still reserved there
/// down. The map says what is left, so a forked child goes on from its
/// parent's.
fn alloc_pe_data_window(state: &mut HostState, len: u64, name: &str) -> Option<u64> {
    use gex_kernel::mem::{flags, prot};
    let (lo, hi) = PE_DATA;
    let at = state.with_kernel(|k| {
        let mm = k.mem.lock();
        mm.vmas()
            .iter()
            .filter(|v| v.start >= lo && v.end <= hi && v.name == PE_DATA_NAME && v.end - v.start >= len)
            .map(|v| v.end - len)
            .max()
    })?;
    map_anon(state, at, len, prot::READ | prot::WRITE, flags::FIXED, false, name).ok()
}

/// [`alloc_above_code`] for a native module's data, noted for Wine's views:
/// in [`PE_DATA`] while it has room.
pub(crate) fn alloc_above_code_data(state: &mut HostState, size: u64, name: &str) -> Result<u64> {
    let page = (size + 0xfff) & !0xfff;
    let (at, len) = match alloc_pe_data_window(state, page, name) {
        Some(at) => (at, page),
        None => (alloc_above_code(state, size, name)?, (size + 0xffff) & !0xffff),
    };
    note_host_data(state, at, len);
    Ok(at)
}

/// A provider's code region: its thread entry at 0, the return addresses
/// its guest calls push in [0x100, 0x1000), reverse slots from
/// `REVERSE_FIRST`, one per `REVERSE_STRIDE` bytes.
const CODE_PAGE: u64 = 0x10000;
const REVERSE_FIRST: u64 = 0x1000;
const REVERSE_STRIDE: u64 = 16;

/// The guest addresses a provider binds (its code region): low, where code
/// lives in a 64-bit guest memory, and read-only.
fn alloc_code(state: &mut HostState, name: &str) -> Result<u64> {
    use gex_kernel::mem::prot;
    map_anon(state, 0, CODE_PAGE, prot::READ, 0, true, name).map_err(|e| anyhow!("mapping {name}: errno {}", e.0))
}

/// [`alloc`] for a built-in provider's data and heap.
pub(crate) fn alloc_top(state: &mut HostState, size: u64, name: &str) -> Result<u64> {
    alloc_linux(state, size, name)
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
    // guest address of their own, bound to `__gex_thread_start`. The page
    // also holds the return addresses its calls into guest code push
    // (`gex.provider_code_page`): addresses gex's dispatch covers, so a
    // return there that nothing predicted is still caught by the call-out
    // that made the call.
    if let Some(k) = slots.iter().position(|s| s.as_deref() == Some("__gex_thread_start")) {
        // Below 4 GiB, like all code: the call cache keys on 32-bit targets.
        let at = alloc_code(state, &format!("[provider {} code]", p.name))?;
        pairs.push((at, k as u32));
        THREAD_ENTRY.store(at, std::sync::atomic::Ordering::Release);
        // Its reverse slots (`gexp__reverse`): guest-callable addresses for
        // function pointers the provider hands out (a managed delegate's),
        // every one served by that body, which tells them apart by the
        // address it was entered at.
        if let Some(r) = slots.iter().position(|s| s.as_deref() == Some("gexp__reverse")) {
            for a in (at + REVERSE_FIRST..at + CODE_PAGE).step_by(REVERSE_STRIDE as usize) {
                pairs.push((a, r as u32));
            }
        }
    }
    // A Linux library's constructors and destructors (`on_linux_maps`).
    if let Some(k) = slots.iter().position(|s| s.as_deref() == Some("gexp__noop")) {
        for a in std::mem::take(&mut *CTORS.lock().unwrap_or_else(|e| e.into_inner())) {
            pairs.retain(|&(x, _)| x != a);
            pairs.push((a, k as u32));
        }
    }
    // An address several names share is served by whichever is provided.
    pairs.sort_by_key(|&(a, k)| (a, k == missing));
    pairs.dedup_by_key(|&mut (a, _)| a);
    MISSING.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).extend(unprovided);
    let (size, align) = dylink_memory(&wasm)?;
    let data = alloc(state, (size as u64).max(16) + (1u64 << align), &format!("[provider {} data]", p.name))?;
    let data = (data + (1u64 << align) - 1) & !((1u64 << align) - 1);
    let module = gex_cache::load_or_compile_module(engine, &wasm).with_context(|| format!("compiling provider {}", p.name))?;
    crate::aot_share::note(&module, crate::aot_share::Kind::Provider);
    ftab::publish_stateful(shared, memory, module, slots.len() as u32, &pairs, Extras { memory_base: data })?;
    Ok(pairs.len())
}

/// After an instantiation: the module's data relocations (`__wasm_apply_
/// data_relocs`: the absolute data and function addresses its initialised
/// data holds), once per address space -- its data is the process's, and a
/// later thread's instance must not reset what has changed since. Its start
/// function has initialised the data itself, once, by then. Held under a
/// lock, so a thread instantiating meanwhile waits for it.
///
/// Recorded in the process's `JitShared`, not host-wide: keyed by the
/// memory's host address and the NT pid, a child process (native Wine's
/// exec'd children have no NT pid) whose memory landed where an exited
/// one's had been skipped its relocations, and ran with its data's pointers
/// unrelocated (a list head of small offsets, function tables of wrong
/// indices).
pub(crate) fn relocate_once(cx: &mut wasmtime::StoreContextMut<'_, HostState>, inst: &wasmtime::Instance, x: Extras) -> Result<()> {
    let set = cx
        .data()
        .jit
        .as_ref()
        .map(|j| Arc::clone(&j.shared.lock().unwrap_or_else(|e| e.into_inner()).relocated))
        .ok_or_else(|| anyhow!("a stateful module with no function table"))?;
    let mut done = set.lock().unwrap_or_else(|e| e.into_inner());
    if done.contains(&x.memory_base) {
        return Ok(());
    }
    if let Ok(f) = inst.get_typed_func::<(), ()>(&mut *cx, "__wasm_apply_data_relocs") {
        f.call(&mut *cx, ()).context("applying a provider's data relocations")?;
    }
    done.insert(x.memory_base);
    Ok(())
}

/// `gex.provider_try` calls (a Lua protected call each: lua_pcall,
/// lua_resume, lua_close's finalizers, the parser) and the throws they
/// caught, for GEX_WIN_STATS.
pub static TRIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static THROWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A `gex.provider_throw` unwinding to its `gex.provider_try`.
#[derive(Debug)]
pub(crate) struct Throw(pub i32);

impl std::fmt::Display for Throw {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "gex.provider_throw({}) with no gex.provider_try to catch it", self.0)
    }
}

impl std::error::Error for Throw {}

/// `gex.ftab_twin(addr)`: the table index of the lifted twin of the export
/// at `addr` (`gex_abi::ftab::TWIN`), 0 when its unit has none. A provider
/// calls it only to fall back (provider.h `gp_lifted`); each address's
/// first fallback is reported (`GEX_PROVIDER_TWIN_LOG=0` quiets it), every
/// one counted.
fn ftab_twin(caller: Caller<'_, HostState>, addr: i64) -> i32 {
    let addr = addr as u64;
    TWIN_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let idx = caller
        .data()
        .jit
        .as_ref()
        .and_then(|j| j.shared.lock().unwrap_or_else(|e| e.into_inner()).ftab.map.twin(addr))
        .map_or(0, |f| f.idx);
    if std::env::var("GEX_PROVIDER_TWIN_LOG").as_deref() != Ok("0") && TWIN_SEEN.lock().unwrap_or_else(|e| e.into_inner()).insert(addr) {
        if idx == 0 {
            eprintln!("gex: provider at {addr:#x} fell back to its lifted body, but its unit has none");
        } else {
            eprintln!("gex: provider at {addr:#x} fell back to its lifted body");
        }
    }
    idx as i32
}

/// Fallbacks to lifted twins so far ([`ftab_twin`]).
pub(crate) static TWIN_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TWIN_SEEN: Mutex<std::collections::BTreeSet<u64>> = Mutex::new(std::collections::BTreeSet::new());

/// Where a provider's own threads start (`__gex_thread_start`, bound by
/// [`install`]): musl's `clone` sets the child's PC to it.
static THREAD_ENTRY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The text of every library a stateful provider replaced, as mapped: code
/// that never runs, so no tier translates it ([`replaced`]).
static REPLACED: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

fn replaced_add(lo: u64, hi: u64) {
    REPLACED.lock().unwrap_or_else(|e| e.into_inner()).push((lo, hi));
}

/// Whether `[start, end)` overlaps a replaced library's text.
pub(crate) fn replaced(start: u64, end: u64) -> bool {
    REPLACED.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|&(lo, hi)| start < hi && end > lo)
}

/// The host functions stateful providers import.
pub(crate) fn register(linker: &mut Linker<HostState>) -> Result<()> {
    // A wasm64 provider's pointers and table indices are i64: one process is one flavour, so the same names take the
    // wider types.
    if gex_abi::mem64() {
        return register64(linker);
    }
    linker.func_wrap(IMPORT_MODULE, "provider_try", |mut caller: Caller<'_, HostState>, f: i32, arg: i32| -> wasmtime::Result<i32> { crate::host((|| -> anyhow::Result<i32> {
        TRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let func = ftab::table_func(&mut caller, f as u32)?;
        let typed = func.typed::<i32, ()>(&caller).context("gex.provider_try: the function is not void(int)")?;
        match typed.call(&mut caller, arg) {
            Ok(()) => Ok(0),
            Err(e) => match e.downcast_ref::<Throw>() {
                Some(t) => {
                    THROWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(t.0)
                }
                None => Err(anyhow::Error::from(e)),
            },
        }
    })()) })?;
    // gexrt64's guarded call (wasm64.c `__gexw_native_try`): fn(arg), and a
    // memory access out of bounds inside it (a bad pointer Wine's code went
    // through) caught here, every wasm frame since gone: 0xc0000005
    // (STATUS_ACCESS_VIOLATION), else what fn returned (0 for none). Index 0
    // asks whether it is on (2; -1 without the debugging switch
    // GEX_NATIVE_TRY=1). The high bit of `f`: fn takes an i64.
    linker.func_wrap(IMPORT_MODULE, "native_try", |mut caller: Caller<'_, HostState>, f: i32, arg: i32| -> wasmtime::Result<i32> { crate::host((|| -> anyhow::Result<i32> {
        if f == 0 {
            static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            return Ok(if *ON.get_or_init(|| std::env::var("GEX_NATIVE_TRY").as_deref() == Ok("1")) { 2 } else { -1 });
        }
        let func = ftab::table_func(&mut caller, (f & 0x7fff_ffff) as u32)?;
        let a = if f < 0 { wasmtime::Val::I64(arg as u32 as i64) } else { wasmtime::Val::I32(arg) };
        let mut out = [wasmtime::Val::I32(0)];
        let n = func.ty(&caller).results().len().min(1);
        match func.call(&mut caller, &[a], &mut out[..n]) {
            Ok(()) => Ok(if n == 1 { out[0].i32().unwrap_or(0) } else { 0 }),
            Err(e) if matches!(e.downcast_ref::<wasmtime::Trap>(), Some(wasmtime::Trap::MemoryOutOfBounds)) => Ok(0xc000_0005u32 as i32),
            Err(e) => Err(anyhow::Error::from(e)),
        }
    })()) })?;
    linker.func_wrap(IMPORT_MODULE, "provider_throw", |_caller: Caller<'_, HostState>, code: i32| -> wasmtime::Result<()> { crate::host((|| -> anyhow::Result<()> { Err(Throw(code).into()) })()) })?;
    // The `nt_qpc` provider's counter, for an i386 PE ntdll (a WoW64
    // process's): what 64-bit Wine's NtQueryPerformanceCounter reads
    // (`monotonic_counter`: CLOCK_BOOTTIME in 100 ns units).
    linker.func_wrap(IMPORT_MODULE, "nt_qpc", |_caller: Caller<'_, HostState>| -> i64 { (gex_kernel::clock_ns(gex_kernel::system_clock, 7) / 100) as i64 })?;
    linker.func_wrap(IMPORT_MODULE, "provider_ctx", |caller: Caller<'_, HostState>| -> i32 { caller.data().thread.ctx_addr as i32 })?;
    // `clock_gettime(clk)` in nanoseconds, no kernel lock (32-bit Wine's PE ntdll's
    // performance counter).
    linker.func_wrap(IMPORT_MODULE, "clock_ns", |_caller: Caller<'_, HostState>, clk: i32| -> i64 { gex_kernel::clock_ns(gex_kernel::system_clock, clk as u32 as u64) as i64 })?;
    linker.func_wrap(IMPORT_MODULE, gex_abi::ftab::TWIN_IMPORT, ftab_twin)?;
    linker.func_wrap(IMPORT_MODULE, "provider_time", |_caller: Caller<'_, HostState>, which: i32| -> f64 {
        match which {
            0 => std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0),
            _ => {
                static START: OnceLock<std::time::Instant> = OnceLock::new();
                START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
            }
        }
    })?;
    linker.func_wrap(IMPORT_MODULE, "provider_missing", |mut caller: Caller<'_, HostState>, addr: i32| -> wasmtime::Result<()> { crate::host((|| -> anyhow::Result<()> {
        let addr = addr as u32 as u64;
        let name = MISSING.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&addr).cloned()).unwrap_or_else(|| "?".into());
        let what = format!("{addr:#x}");
        Err(crate::aot_only::stop(&mut caller, addr, &format!("gex-eject: the program called {name} ({what}), an export of a replaced library that its provider does not implement")))
    })()) })?;
    linker.func_wrap(IMPORT_MODULE, "provider_alloc", |mut caller: Caller<'_, HostState>, size: i32| -> wasmtime::Result<i32> { crate::host((|| -> anyhow::Result<i32> {
        Ok(alloc(caller.data_mut(), size as u32 as u64, "[provider heap]")? as i32)
    })()) })?;
    // The same from the top down: a built-in provider's heap (`alloc_top`).
    linker.func_wrap(IMPORT_MODULE, "provider_alloc_top", |mut caller: Caller<'_, HostState>, size: i32| -> wasmtime::Result<i32> { crate::host((|| -> anyhow::Result<i32> {
        Ok(alloc_top(caller.data_mut(), size as u32 as u64, "[provider heap]")? as i32)
    })()) })?;
    // fork in a native wasm32 program (the musl fork's
    // src/process/wasm32/__gex_fork.c, asyncified by wcc): asyncify's
    // exports on the calling instance, and the fork point itself, which
    // starts the unwind (normal state) or ends the rewind.
    linker.func_wrap(IMPORT_MODULE, "asyncify", |mut caller: Caller<'_, HostState>, op: i32, arg: i32| -> wasmtime::Result<i32> {
        crate::host(asyncify(&mut caller, op, arg))
    })?;
    crate::dylink::register(linker)?;
    linker.func_wrap(IMPORT_MODULE, "fork_point", |mut caller: Caller<'_, HostState>, rec: i32| -> wasmtime::Result<()> { crate::host((|| -> anyhow::Result<()> {
        match asyncify(&mut caller, 0, 0)? {
            0 => asyncify(&mut caller, 1, rec)?,
            2 => asyncify(&mut caller, 4, 0)?,
            s => bail!("gex.fork_point in asyncify state {s}"),
        };
        Ok(())
    })()) })?;
    // Wine's system-call dispatcher on its Unix side : ntdll.so
    // registers it (op 0), and the PE side's gexrt asks for it (op 1; 0
    // while there is none).
    linker.func_wrap(IMPORT_MODULE, "unix_dispatcher", |caller: Caller<'_, HostState>, op: i32, idx: i32| -> i32 {
        crate::unix_dispatcher(caller.data(), op, idx as u32) as i32
    })?;
    // Door calls into the wineserver (`door`).
    linker.func_wrap(IMPORT_MODULE, "door_publish", |mut caller: Caller<'_, HostState>, area: i32| -> wasmtime::Result<()> {
        crate::host(crate::door::publish(&mut caller, area as u32))
    })?;
    linker.func_wrap(IMPORT_MODULE, "door_call", |mut caller: Caller<'_, HostState>, req: i32, pid: i32, tid: i32| -> wasmtime::Result<i32> {
        crate::host(crate::door::call(&mut caller, req as u32, pid, tid, false))
    })?;
    // A 64-bit client's (64-bit Wine's lowered LP64 ntdll.so): the same call, the
    // request's gather list at LP64 offsets (`door::req_layout`).
    linker.func_wrap(IMPORT_MODULE, "door_call_lp64", |mut caller: Caller<'_, HostState>, req: i32, pid: i32, tid: i32| -> wasmtime::Result<i32> {
        crate::host(crate::door::call(&mut caller, req as u32, pid, tid, true))
    })?;
    // Natively every process sees the door once published (one host
    // process): never missing.
    linker.func_wrap(IMPORT_MODULE, "door_missing", || -> i32 { 0 })?;
    // And an exec in place is one natively: nothing to ask for.
    linker.func_wrap(IMPORT_MODULE, "exec_fresh", || {})?;
    // The end of 64-bit Wine's space in 64 KiB units: 0, Wine probes.
    linker.func_wrap(IMPORT_MODULE, "space_top", || -> i32 { 0 })?;
    // (natively the module's own data is not in the guest's memory)
    linker.func_wrap(IMPORT_MODULE, "host_space", || -> i32 { 0 })?;
    linker.func_wrap(IMPORT_MODULE, "session_copy", |mut caller: Caller<'_, HostState>, dst: i32, off: i32, size: i32| -> wasmtime::Result<i32> {
        crate::host(crate::door::session_copy(&mut caller, dst as u32, off as u32, size as u32))
    })?;
    linker.func_wrap(IMPORT_MODULE, "wake_pipe", |mut caller: Caller<'_, HostState>, fd: i32| -> i32 { crate::door::wake_pipe(&mut caller, fd) })?;
    linker.func_wrap(IMPORT_MODULE, "wake_write", |caller: Caller<'_, HostState>, id: i32, ptr: i32, len: i32| -> i32 {
        let Some(m) = caller.data().memory.clone() else { return -14 };
        let (at, n) = (ptr as u32 as usize, len as u32 as usize);
        let b = m.bytes();
        if at.checked_add(n).is_none_or(|e| e > b.len()) {
            return -14;
        }
        let v = b[at..at + n].to_vec();
        crate::door::wake_write(id, &v)
    })?;
    linker.func_wrap(IMPORT_MODULE, "wake_close", |id: i32| crate::door::wake_close(id))?;
    linker.func_wrap(IMPORT_MODULE, "usd_copy", |mut caller: Caller<'_, HostState>, dst: i32, size: i32| -> wasmtime::Result<i32> {
        crate::host(crate::door::usd_move(&mut caller, dst as u32, size as u32, false))
    })?;
    linker.func_wrap(IMPORT_MODULE, "usd_publish", |mut caller: Caller<'_, HostState>, src: i32, size: i32| -> wasmtime::Result<i32> {
        crate::host(crate::door::usd_move(&mut caller, src as u32, size as u32, true))
    })?;
    // The native DLL a function pointer (a table index) or a static
    // variable's address is in, as its skeleton's base (ntdll's
    // LdrFindEntryForAddress, when no image holds the address).
    linker.func_wrap(IMPORT_MODULE, gex_abi::ftab::IMAGE_OF, |caller: Caller<'_, HostState>, addr: i64| -> i64 {
        caller.data().jit.as_ref().map_or(0, |j| crate::wasmdll::image_of(&j.shared, addr as u64) as i64)
    })?;
    // The memory gex mapped for native modules (their data, heaps,
    // stacks) from the `from`th on, as (start, end) u32 pairs at `out`, at
    // most `max`: how many there are in all. Wine's ntdll makes a view of
    // each (`MemoryMap::note_host_data`).
    linker.func_wrap(IMPORT_MODULE, "host_data", |mut caller: Caller<'_, HostState>, from: i32, out: i32, max: i32| -> i32 {
        let ranges = caller.data_mut().with_kernel(|k| k.mem.lock().host_data().to_vec());
        let Some(m) = caller.data().memory.clone() else { return 0 };
        let b = m.bytes();
        for (i, &(lo, hi)) in ranges.iter().skip(from as u32 as usize).take(max as u32 as usize).enumerate() {
            let at = out as u32 as usize + i * 8;
            let Some(dst) = b.get_mut(at..at + 8) else { break };
            dst[..4].copy_from_slice(&(lo as u32).to_le_bytes());
            dst[4..].copy_from_slice(&(hi.min(0xffff_ffff) as u32).to_le_bytes());
        }
        ranges.len() as i32
    })?;
    // Where i386 code reads KUSER_SHARED_DATA (32-bit Wine checks its own
    // address against it at start; the browser's `gex_kuser_address`).
    linker.func_wrap(IMPORT_MODULE, "kuser_address", || -> i32 {
        gex_abi::kuser_address() as i32
    })?;
    linker.func_wrap(IMPORT_MODULE, "session_seq", |off: i32| -> i64 { crate::door::session_seq(off as u32) })?;
    // Wine's ntdll.so mapped a PE image : a native DLL's skeleton gets
    // Wine's PE module bound at its exports (`wasmdll`). The image is named by the descriptor it was mapped
    // from.
    linker.func_wrap(IMPORT_MODULE, "image_mapped", |mut caller: Caller<'_, HostState>, base: i32, fd: i32| -> wasmtime::Result<()> { crate::host((|| -> anyhow::Result<()> {
        let base = base as u32 as u64;
        let Some(path) = caller.data_mut().with_kernel(|k| k.fds.get(fd).ok().map(|f| f.path.clone())) else { return Ok(()) };
        crate::note_library(&path);
        let Some(bytes) = caller.data_mut().with_kernel(|k| k.read_file(&path)) else { return Ok(()) };
        let Ok(pe) = gex_ld::pe::parse(&bytes) else { return Ok(()) };
        crate::note_wine32_image(base, pe.image_size as u64, &path);
        let (shared, engine) = {
            let Some(jit) = caller.data().jit.as_ref() else { return Ok(()) };
            (Arc::clone(&jit.shared), jit.engine.clone())
        };
        let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
        if let Some(n) = crate::wasmdll::install(&pe, &path, base, caller.data_mut(), &shared, &engine, &memory)? {
            if std::env::var_os("GEX_WINE32_TRACE").is_some() {
                eprintln!("gex-wine32: native module for {path} at {base:#x}: {n} entries");
            }
            crate::ftab::catch_up(&mut caller)?;
            crate::wasmdll::apply_pending(&shared, &memory);
        } else {
            crate::wasmdll::take_back(&shared, &memory, base, base + pe.image_size as u64);
            // An i386 image: its function-tier unit, if one was built.
            crate::pe_units::wine32_image(&mut caller, &path, base, pe)?;
        }
        Ok(())
    })()) })?;
    // Code in `[addr, addr + len)` changed (32-bit Wine: Wine's
    // NtFlushInstructionCache): its translations are retired.
    linker.func_wrap(IMPORT_MODULE, "code_changed", |mut caller: Caller<'_, HostState>, addr: i32, len: i32| {
        let (addr, len) = (addr as u32 as u64, len as u32 as u64);
        caller.data_mut().with_kernel(|k| {
            let (start, end) = (addr & !0xfff, (addr + len + 0xfff) & !0xfff);
            if end > start {
                k.mem.lock().log.retired_code.push((start, end));
            }
        });
        gex_kernel::note_code_change();
    })?;
    // The function-table index of a named function of Wine's PE module
    // (win32u.so starts explorer's desktop thread by
    // `explorer.exe!gexp_desktop_thread`); 0 when there is none.
    linker.func_wrap(IMPORT_MODULE, "wine_pe_slot", |caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i32 {
        let Some(m) = caller.data().memory.as_ref() else { return 0 };
        let Some(b) = m.bytes().get(ptr as u32 as usize..(ptr as u32 as usize).saturating_add(len as u32 as usize)) else { return 0 };
        let name = String::from_utf8_lossy(b).into_owned();
        let Some(jit) = caller.data().jit.as_ref() else { return 0 };
        let sh = jit.shared.lock().unwrap_or_else(|e| e.into_inner());
        // the core's, or a module of its own's (`pelink.py --split`), of
        // the process's own mode (the caller is a Unix side's)
        sh.wine_pe
            .iter()
            .chain(sh.wine_pe_split.iter())
            .filter(|wp| !wp.alt)
            .find_map(|wp| wp.slots.get(&name).map(|&k| (sh.ftab.map.install(wp.install).base + k) as i32))
            .unwrap_or(0)
    })?;
    // A native wasm32 program's signal hook (`crate::SIGNAL_HOOKS`).
    linker.func_wrap(IMPORT_MODULE, "signal_hook", |caller: Caller<'_, HostState>, idx: i32| {
        crate::set_signal_hook(caller.data(), idx as u32);
    })?;
    // A native wasm32 program's guest fault hook (`crate::FAULT_HOOKS`).
    linker.func_wrap(IMPORT_MODULE, "fault_hook", |caller: Caller<'_, HostState>, idx: i32| {
        crate::set_fault_hook(caller.data(), idx as u32);
    })?;
    // Diagnostics: text a provider writes to its stderr.
    // A program's i386 code calling or jumping to `addr` runs the function
    // at table index `idx` (gexcc's rt/i386.c, gex_i386_bind): bound like an
    // export of the install holding the index. -1 for an index no install
    // has.
    linker.func_wrap(IMPORT_MODULE, "i386_bind", |caller: Caller<'_, HostState>, addr: i64, idx: i32| -> wasmtime::Result<i32> { crate::host((|| -> anyhow::Result<i32> {
        let Some(jit) = caller.data().jit.as_ref() else { return Ok(-1) };
        let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
        let shared = Arc::clone(&jit.shared);
        let idx = idx as u32;
        let found = {
            let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
            sh.ftab.map.installs().iter().position(|i| idx >= i.base && idx < i.base + i.len).map(|k| (k as u32, sh.ftab.map.installs()[k].base))
        };
        let Some((install, base)) = found else { return Ok(-1) };
        ftab::bind_pairs(&shared, &memory, install, &[(addr as u64 & 0xffff_ffff, idx - base)])?;
        Ok(0)
    })()) })?;
    linker.func_wrap(IMPORT_MODULE, "provider_log", |caller: Caller<'_, HostState>, ptr: i32, len: i32| -> wasmtime::Result<()> { crate::host((|| -> anyhow::Result<()> {
        if let Some(m) = caller.data().memory.as_ref() {
            if let Some(b) = m.bytes().get(ptr as u32 as usize..(ptr as u32 as usize).saturating_add(len as u32 as usize)) {
                use std::io::Write;
                let _ = std::io::stderr().write_all(b);
            }
        }
        Ok(())
    })()) })?;
    Ok(())
}

/// asyncify's export `op` on the calling instance: 0 `asyncify_get_state`
/// (0 when the module is not asyncified), 1 `start_unwind(arg)`,
/// 2 `stop_unwind`, 3 `start_rewind(arg)`, 4 `stop_rewind`.
fn asyncify(caller: &mut Caller<'_, HostState>, op: i32, arg: i32) -> Result<i32> {
    let name = match op {
        0 => "asyncify_get_state",
        1 => "asyncify_start_unwind",
        2 => "asyncify_stop_unwind",
        3 => "asyncify_start_rewind",
        4 => "asyncify_stop_rewind",
        _ => bail!("gex.asyncify: no operation {op}"),
    };
    let Some(wasmtime::Extern::Func(f)) = caller.get_export(name) else {
        if op == 0 {
            return Ok(0);
        }
        bail!("gex.asyncify: the program has no {name} (fork needs wcc's asyncify)");
    };
    Ok(match op {
        0 => f.typed::<(), i32>(&*caller)?.call(&mut *caller, ())?,
        1 | 3 => {
            f.typed::<i32, ()>(&*caller)?.call(&mut *caller, arg)?;
            0
        }
        _ => {
            f.typed::<(), ()>(&*caller)?.call(&mut *caller, ())?;
            0
        }
    })
}

/// [`register`] for a 64-bit guest memory: the same functions with i64
/// addresses, sizes and function-table indices.
fn register64(linker: &mut Linker<HostState>) -> Result<()> {
    // The native DLL a function pointer (a table index) or a static
    // variable's address is in, as its skeleton's base (ntdll's
    // LdrFindEntryForAddress, when no image holds the address).
    linker.func_wrap(IMPORT_MODULE, gex_abi::ftab::IMAGE_OF, |caller: Caller<'_, HostState>, addr: i64| -> i64 {
        caller.data().jit.as_ref().map_or(0, |j| crate::wasmdll::image_of(&j.shared, addr as u64) as i64)
    })?;
    linker.func_wrap(IMPORT_MODULE, "provider_ctx", |caller: Caller<'_, HostState>| -> i32 { caller.data().thread.ctx_addr as i32 })?;
    // `clock_gettime(clk)` in nanoseconds, no kernel lock (32-bit Wine's PE ntdll's
    // performance counter).
    linker.func_wrap(IMPORT_MODULE, "clock_ns", |_caller: Caller<'_, HostState>, clk: i32| -> i64 { gex_kernel::clock_ns(gex_kernel::system_clock, clk as u32 as u64) as i64 })?;
    linker.func_wrap(IMPORT_MODULE, gex_abi::ftab::TWIN_IMPORT, ftab_twin)?;
    linker.func_wrap(IMPORT_MODULE, "provider_missing", |mut caller: Caller<'_, HostState>, addr: i64| -> wasmtime::Result<()> { crate::host((|| -> anyhow::Result<()> {
        let addr = addr as u64;
        let name = MISSING.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&addr).cloned()).unwrap_or_else(|| "?".into());
        let what = format!("{addr:#x}");
        Err(crate::aot_only::stop(&mut caller, addr, &format!("gex: the program called {name} ({what}), an export of a replaced library that its provider does not implement")))
    })()) })?;
    linker.func_wrap(IMPORT_MODULE, "provider_alloc", |mut caller: Caller<'_, HostState>, size: i64| -> wasmtime::Result<i64> { crate::host((|| -> anyhow::Result<i64> {
        Ok(alloc(caller.data_mut(), size as u64, "[provider heap]")? as i64)
    })()) })?;
    linker.func_wrap(IMPORT_MODULE, "provider_log", |caller: Caller<'_, HostState>, ptr: i64, len: i64| -> wasmtime::Result<()> { crate::host((|| -> anyhow::Result<()> {
        if let Some(m) = caller.data().memory.as_ref() {
            if let Some(b) = m.bytes().get(ptr as usize..(ptr as usize).saturating_add(len as usize)) {
                use std::io::Write;
                let _ = std::io::stderr().write_all(b);
            }
        }
        Ok(())
    })()) })?;
    // This thread's provider C stack (`stack`): its lowest address; it is
    // `STACK` bytes. musl reports it as the thread's stack. (Not derived from
    // the stack pointer's current value, which is below the top by whatever
    // the thread has on its stack when it asks.)
    linker.func_wrap(IMPORT_MODULE, "provider_stack", |mut caller: Caller<'_, HostState>| -> wasmtime::Result<i64> { crate::host((|| -> anyhow::Result<i64> {
        use wasmtime::AsContextMut;
        let mut cx = caller.as_context_mut();
        stack(&mut cx)?;
        Ok(cx.data().jit.as_ref().and_then(|j| j.ftab.as_ref()).map_or(0, |s| s.provider_stack_lo) as i64)
    })()) })?;
    // Constants of gex's ABI a provider calling guest code needs.
    linker.func_wrap(IMPORT_MODULE, "provider_abi", |_caller: Caller<'_, HostState>, which: i32| -> i64 {
        match which {
            0 => gex_abi::FCACHE as i64,
            1 => gex_abi::FCACHE_BITS as i64,
            2 => gex_abi::ctx::V as i64,
            _ => -1,
        }
    })?;
    // How many table slots the module installed at `base` has: what is its
    // own function pointer (a table index) and what is a guest address.
    linker.func_wrap(IMPORT_MODULE, "provider_table_size", |caller: Caller<'_, HostState>, base: i64| -> i64 {
        let Some(jit) = caller.data().jit.as_ref() else { return 0 };
        let sh = jit.shared.lock().unwrap_or_else(|e| e.into_inner());
        sh.ftab.map.installs().iter().find(|i| i.base as i64 == base).map_or(0, |i| i.len as i64)
    })?;
    // A provider's guest call begins or ends: natively a blocking call waits
    // in place anyway (the browser keeps the count: cgen/libprov.rs).
    linker.func_wrap(IMPORT_MODULE, "provider_guest_call", |_caller: Caller<'_, HostState>, _delta: i32| {})?;
    linker.func_wrap(IMPORT_MODULE, "provider_code_page", |_caller: Caller<'_, HostState>| -> i64 {
        THREAD_ENTRY.load(std::sync::atomic::Ordering::Acquire) as i64
    })?;
    linker.func_wrap(IMPORT_MODULE, "provider_thread_entry", |_caller: Caller<'_, HostState>| -> i64 {
        THREAD_ENTRY.load(std::sync::atomic::Ordering::Acquire) as i64
    })?;
    Ok(())
}

/// Linux libraries already given their provider: (path, load bias).
static ELF_INSTALLED: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());

/// After a Linux syscall that mapped executable code (`MapLog::new_code`):
/// a shared object one of this process's stateful providers serves gets
/// its exports bound to the provider, before anything can call them
/// (`libsqlite3.so.0`, say). The library's
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
        // Its constructors and destructors: the guest's loader calls them,
        // and none of the library's code may run (`gexp__noop`).
        let mut ctors: Vec<u64> = image.init_array.clone();
        ctors.extend(elf_fini(&bytes));
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
        // None of the replaced library's code may run, so the function JIT must not translate it
        // either: following a PLT's folded GOT load into it would publish the library's own code
        // over the provider's bindings, and the program would run it.
        replaced_add(lo.wrapping_add(bias), hi.wrapping_add(bias));
        for lp in libs {
            CTORS.lock().unwrap_or_else(|e| e.into_inner()).extend(ctors.iter().filter(|&&v| v >= lo && v < hi).map(|&v| v.wrapping_add(bias)));
            let n = install(lp, caller.data_mut(), &shared, &engine, &memory, &exports, bias, 0)?;
            if std::env::var_os("GEX_PROVIDER_TRACE").is_some() {
                eprintln!("gex: provider {} for {} at {bias:#x}: {n} exports bound", lp.name, m.path);
            }
        }
        crate::ftab::catch_up(&mut *caller)?;
    }
    Ok(())
}

/// Constructor and destructor addresses of the library being installed
/// (unbiased by `on_linux_maps`, biased here).
static CTORS: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// DT_FINI and FINI_ARRAY's entries (their file values, as gex_ld reads
/// INIT_ARRAY's): unbiased addresses.
fn elf_fini(bytes: &[u8]) -> Vec<u64> {
    let u64at = |o: usize| bytes.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
    let u32at = |o: usize| bytes.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let u16at = |o: usize| bytes.get(o..o + 2).map(|b| u16::from_le_bytes(b.try_into().unwrap()));
    let mut out = Vec::new();
    let (Some(phoff), Some(phentsize), Some(phnum)) = (u64at(0x20), u16at(0x36), u16at(0x38)) else { return out };
    // vaddr -> file offset by the PT_LOAD segments
    let mut loads = Vec::new();
    let mut dynamic = None;
    for i in 0..phnum as usize {
        let ph = phoff as usize + i * phentsize as usize;
        let (Some(ty), Some(off), Some(va), Some(fsz)) = (u32at(ph), u64at(ph + 8), u64at(ph + 16), u64at(ph + 32)) else { continue };
        if ty == 1 {
            loads.push((va, off, fsz));
        }
        if ty == 2 {
            dynamic = Some((off, fsz));
        }
    }
    let file_of = |va: u64| loads.iter().find(|&&(v, _, s)| va >= v && va < v + s).map(|&(v, o, _)| (va - v + o) as usize);
    let Some((doff, dsz)) = dynamic else { return out };
    let (mut fini_array, mut fini_arraysz) = (None, 0u64);
    for i in 0..dsz / 16 {
        let (Some(tag), Some(val)) = (u64at((doff + i * 16) as usize), u64at((doff + i * 16 + 8) as usize)) else { break };
        match tag {
            0 => break,
            13 => out.push(val),          // DT_FINI
            26 => fini_array = Some(val), // DT_FINI_ARRAY
            28 => fini_arraysz = val,     // DT_FINI_ARRAYSZ
            _ => {}
        }
    }
    if let Some(fa) = fini_array {
        for i in 0..fini_arraysz / 8 {
            if let Some(v) = file_of(fa + i * 8).and_then(u64at) {
                out.push(v);
            }
        }
    }
    out
}
