//! Native Wine DLLs on wasm32: a DLL
//! whose code is a wasm module, behind a PE skeleton that winebuild
//! (`CPU_WASM32`) wrote. The skeleton maps like any PE; gex binds the
//! module's entry adapters at its export addresses. Its sections:
//!
//! - `.text`: the entry point and a 4-byte `ud2; ud2` stub per exported
//!   function, never run;
//! - `.wasmdat`: the module's data (`__memory_base`), so data exports
//!   point into it;
//! - `.edata`: the exports;
//! - `.wasm`: the module.
//!
//! The module is installed like a stateful provider (`libprov`), with its
//! data at `.wasmdat` rather than allocated: `gexp_<export>` binds at each
//! export, `gexp__entry` at the entry point, `gexp__missing` at exports it
//! has no adapter for (varargs, Wine's system calls, until they have one).

use super::*;

/// If the PE just mapped at `base` is a wasm32 skeleton, install its
/// module. Returns how many exports were bound, or `None` for an ordinary
/// PE.
pub(crate) fn install(
    pe: &gex_ld::pe::PeImage,
    path: &str,
    base: u64,
    state: &mut HostState,
    shared: &Arc<Mutex<JitShared>>,
    engine: &Engine,
    memory: &gex_mem::GuestMemory,
) -> Result<Option<usize>> {
    if let Some(r) = pe.sections.iter().find(|s| s.name == ".wasmref") {
        return install_shared(pe, path, base, r, state, shared, engine, memory).map(Some);
    }
    let Some(wasm_sec) = pe.sections.iter().find(|s| s.name == ".wasm") else { return Ok(None) };
    let data_sec = pe
        .sections
        .iter()
        .find(|s| s.name == ".wasmdat")
        .ok_or_else(|| anyhow!("{path}: a .wasm section but no .wasmdat"))?;
    let at = (base + wasm_sec.rva as u64) as usize;
    let wasm = memory
        .bytes()
        .get(at..at + wasm_sec.vsize as usize)
        .ok_or_else(|| anyhow!("{path}: .wasm outside memory"))?
        .to_vec();
    if !pe.is64 {
        if let Some(e) = gex_cgen::header::abi_refusal(&wasm, &format!("Wine's DLL {path}")) {
            bail!("{e}");
        }
    }
    let (size, align) = crate::libprov::dylink_memory(&wasm)?;
    let data = base + data_sec.rva as u64;
    if data & ((1u64 << align) - 1) != 0 || (size as u64) > data_sec.vsize as u64 {
        bail!("{path}: .wasmdat ({:#x}, {:#x} bytes) does not hold the module's data ({size:#x} bytes, align {align})", data, data_sec.vsize);
    }
    let slots = gex_cgen::provider::slot_names(&wasm)?;
    let slot = |name: &str| slots.iter().position(|s| s.as_deref() == Some(name)).map(|k| k as u32);
    let missing = slot("gexp__missing").ok_or_else(|| anyhow!("{path}: no gexp__missing in the module's table"))?;
    let mut pairs = Vec::new();
    for e in pe.exports.entries.iter().filter(|e| e.rva != 0 && e.forwarder.is_none()) {
        // Data exports point into .wasmdat: nothing to bind.
        if e.rva >= data_sec.rva && e.rva < data_sec.rva + data_sec.vsize {
            continue;
        }
        let k = e
            .names
            .iter()
            .find_map(|n| slot(&format!("gexp_{n}")))
            .or_else(|| slot(&format!("gexp_#{}", e.ordinal)))
            .unwrap_or(missing);
        pairs.push((base + e.rva as u64, k));
    }
    if let (Some(k), true) = (slot("gexp__entry"), pe.entry_rva != 0) {
        pairs.push((base + pe.entry_rva as u64, k));
    }
    pairs.sort_unstable();
    pairs.dedup_by_key(|&mut (a, _)| a);
    let n = pairs.len();
    // Compiled once and kept in the shared cache by content, as providers
    // are; a runtime without a compiler only loads.
    let module = gex_cache::load_or_compile_module(engine, &wasm)
        .with_context(|| format!("compiling {path}'s wasm module"))?;
    crate::aot_share::note(&module, crate::aot_share::Kind::Provider);
    let _ = state;
    crate::ftab::publish_stateful(shared, memory, module, slots.len() as u32, &pairs, crate::libprov::Extras { memory_base: data })?;
    crate::dylink::published();
    Ok(Some(n))
}

/// Wine's PE side as one module (pelink.py): the
/// install, where its data is, its slots by name and its data exports'
/// offsets.
pub(crate) struct WinePe {
    pub(crate) install: u32,
    /// The module's file (a skeleton's `.wasmref` names it beside itself).
    file: std::path::PathBuf,
    /// Its function-table module number (`ftab` `modules`): what a
    /// `--split` module's imports of the core are bound to.
    module: u32,
    data: u64,
    pub(crate) slots: std::collections::HashMap<String, u32>,
    /// `gexp_<export>` slots by export name, whichever DLL owns them: a
    /// DLL's `-import` re-exports are served by the owner's adapter.
    any_dll: std::collections::HashMap<String, u32>,
    data_exports: std::collections::HashMap<String, u32>,
    /// Writes into the module's data that must follow its initialisation
    /// (`apply_pending`, after the drain's catch-up instantiated it).
    pending: Mutex<Vec<(u64, Vec<u8>)>>,
    /// The entry thunks are bound (`bind_entry_thunks`, once).
    entries_bound: std::sync::atomic::AtomicBool,
    /// An LLP64 module (64-bit Wine, its skeletons PE32+): pointers in its data are
    /// 8 bytes, the entry pairs 16.
    ptr64: bool,
    /// A module of the process's other mode (an ILP32 module, 32-bit Wine's,
    /// in an x86-64 process: WoW64): its addresses are bound in the other
    /// dispatch directory (`ftab::Shared::alt_installs`), and it binds only to
    /// modules of its own mode, whose names (`kernel32.dll!CreateFileW`) are
    /// the 64-bit side's too.
    pub(crate) alt: bool,
    /// The DLLs whose skeleton has been mapped: a later mapping of one (an
    /// image view, LoadLibraryEx as an image resource) binds its exports
    /// but writes none of the variables, which belong to the loaded DLL.
    /// Each with the base of that first mapping, the loaded DLL's
    /// ([`image_of`]).
    mapped: Mutex<std::collections::HashMap<String, u64>>,
    /// The module's data bytes from `data`.
    data_size: u64,
    /// Each DLL's table slots and data (pelink's `gexw.dllslots`).
    dll_slots: Arc<Vec<DllRange>>,
    /// Each DLL's data as it was before its first load ran anything
    /// ([`DataSnap`]), to start it afresh when it is loaded again; and the
    /// DLLs whose snapshot the next [`apply_pending`] takes.
    snaps: Mutex<std::collections::HashMap<String, Arc<Vec<DataSnap>>>>,
    snap_todo: Mutex<Vec<String>>,
}

/// One piece of a DLL's data range: its bytes, or (`None`) `len` zeros.
pub(crate) struct DataSnap {
    at: u64,
    len: u64,
    bytes: Option<Vec<u8>>,
}

/// A DLL's data, `ranges` (its `gexw.dllslots` data, past the module's
/// data at `data`), in 4 KiB pieces, the zero ones not kept.
pub(crate) fn snap_data(memory: &[u8], data: u64, ranges: &[DllRange], dll: &str) -> Vec<DataSnap> {
    let mut out = Vec::new();
    for r in ranges.iter().filter(|r| !r.func && r.dll == dll) {
        let mut a = data + r.lo;
        while a < data + r.hi {
            let len = (data + r.hi - a).min(4096);
            let Some(b) = memory.get(a as usize..(a + len) as usize) else { break };
            out.push(DataSnap { at: a, len, bytes: b.iter().any(|&x| x != 0).then(|| b.to_vec()) });
            a += len;
        }
    }
    out
}

/// The writes that put a DLL's data back as [`snap_data`] took it.
pub(crate) fn snap_writes(snap: &[DataSnap]) -> Vec<(u64, Vec<u8>)> {
    snap.iter().map(|p| (p.at, p.bytes.clone().unwrap_or_else(|| vec![0; p.len as usize]))).collect()
}

/// A range of one DLL's table slots (past the install's base) or data
/// bytes (past the module's data), end exclusive.
pub(crate) struct DllRange {
    dll: String,
    func: bool,
    lo: u64,
    hi: u64,
}

/// pelink's `gexw.dllslots` custom section: lines `dll f|d LO HI` (hex).
fn dll_slots(wasm: &[u8]) -> Vec<DllRange> {
    let mut out = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let Ok(wasmparser::Payload::CustomSection(c)) = payload {
            if c.name() == "gexw.dllslots" {
                for line in String::from_utf8_lossy(c.data()).lines() {
                    let t: Vec<&str> = line.split(' ').collect();
                    if let [dll, kind, lo, hi] = t[..] {
                        if let (Ok(lo), Ok(hi)) = (u64::from_str_radix(lo, 16), u64::from_str_radix(hi, 16)) {
                            out.push(DllRange { dll: dll.to_string(), func: kind == "f", lo, hi });
                        }
                    }
                }
            }
        }
    }
    out
}

/// `gex.image_of`: the base of the loaded skeleton of the native DLL whose
/// function `addr` is (a table index below `NATIVE_FN_TOP`, in a Wine PE
/// module's slots) or whose static data it is (in a module's data, which no
/// image holds), else 0. ntdll asks when no image contains `addr`
/// (GetModuleHandleExW's FROM_ADDRESS, RtlPcToFileHeader).
pub(crate) fn image_of(shared: &Arc<Mutex<JitShared>>, addr: u64) -> u64 {
    let s = shared.lock().unwrap_or_else(|e| e.into_inner());
    let func = addr < gex_abi::ftab::NATIVE_FN_TOP;
    for wp in s.wine_pe.iter().chain(s.wine_pe_split.iter()) {
        let off = if func {
            let inst = s.ftab.map.install(wp.install);
            match addr.checked_sub(inst.base as u64) {
                Some(off) if off < inst.len as u64 => off,
                _ => continue,
            }
        } else {
            match addr.checked_sub(wp.data) {
                Some(off) if off < wp.data_size => off,
                _ => continue,
            }
        };
        let dll = wp.dll_slots.iter().find(|r| r.func == func && off >= r.lo && off < r.hi).map(|r| r.dll.as_str());
        return dll.and_then(|d| wp.mapped.lock().unwrap_or_else(|e| e.into_inner()).get(d).copied()).unwrap_or(0);
    }
    0
}

/// An ordinary PE image mapped at `[start, end)`: the bindings of Wine's PE
/// modules there belong to a skeleton that was mapped there before and is
/// gone (`ftab::on_retire` keeps providers on an unmap), so they go too.
/// Kept, the new image's own code at a freed stub's address called the
/// native export: DXSETUP frees advpack (an i386 skeleton), then dsetup.dll
/// maps over it and its DllMain ran advpack's DelNodeA. Keyed on the remap,
/// not on the bytes, so a hook's rewrite of a live skeleton's stub keeps its
/// binding. Every arch and every Wine.
pub(crate) fn take_back(shared: &Arc<Mutex<JitShared>>, memory: &gex_mem::GuestMemory, start: u64, end: u64) {
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    let installs: Vec<u32> = s.wine_pe.iter().chain(s.wine_pe_split.iter()).map(|wp| wp.install).collect();
    if installs.is_empty() {
        return;
    }
    let gone: Vec<u64> = s.ftab.map.range(start, end).filter(|(_, f)| installs.contains(&f.install)).map(|(a, _)| a).collect();
    for &a in &gone {
        gex_kernel::ftab::fcache_clear(&mut GuestMem::new(memory.bytes()), a);
        s.ftab.map.retire(a);
    }
    if !gone.is_empty() && std::env::var_os("GEX_WINE32_TRACE").is_some() {
        eprintln!("gex-wine32: {} bindings of Wine's PE modules at {start:#x}..{end:#x} taken back by an image mapped over them", gone.len());
    }
}

impl WinePe {
    /// Whether this module's binding at `addr` outlived its skeleton: the
    /// bytes there are no longer the skeleton's stub or a stub's alias
    /// (`gex_kernel::win::skel`), so the code there is the guest's (a hook,
    /// whose trampoline reaches the export at the alias, [`bind_alias`])
    /// and the native export has nothing to do with it.
    /// A view unmapped and its addresses reused (an image view, freed, then
    /// `VirtualAlloc`'d for the program's own code) is that: kept, the
    /// binding sent those calls to Wine's PE module. ([`take_back`] covers a
    /// PE image mapped there; this, code that lands without one.) 64-bit Wine's module
    /// only (32-bit Wine's skeletons keep their bindings beside a Detours
    /// hook), and only at guest addresses: the entry thunks are bound at
    /// table indices below the ICALL floor, where no stub is.
    pub(crate) fn binding_is_stale(&self, memory: &[u8], addr: u64) -> bool {
        if !self.ptr64 || addr < 0x3f0000 {
            return false;
        }
        memory.get(addr as usize..addr as usize + 4).is_some() && !gex_kernel::win::skel::intact(memory, addr)
    }
}

/// A miss at a 64-bit stub's alias (`gex_kernel::win::skel`) whose stub has
/// a binding of a Wine PE module: a hook's trampoline, calling the original
/// before a call through the rewritten stub retired that binding
/// ([`bind_stub_alias`] then). The native export is bound at the alias too,
/// for the caller to bridge.
pub(crate) fn bind_alias(s: &mut JitShared, memory: &[u8], pc: u64) -> bool {
    pc % 16 == gex_kernel::win::skel::ALIAS && bind_stub_alias(s, memory, pc - gex_kernel::win::skel::ALIAS)
}

/// Bind the native export of the 64-bit Wine PE stub at `stub` at its
/// alias too, when the alias is still there and nothing is bound at it:
/// before the stub's binding is retired (a hook rewrote it), or on a miss
/// at the alias.
pub(crate) fn bind_stub_alias(s: &mut JitShared, memory: &[u8], stub: u64) -> bool {
    use gex_kernel::win::skel;
    let alias = stub + skel::ALIAS;
    if s.ftab.map.get(alias).is_some() || !skel::alias(memory, alias) {
        return false;
    }
    let Some(f) = s.ftab.map.get(stub) else { return false };
    if !s.wine_pe.iter().chain(s.wine_pe_split.iter()).any(|wp| wp.ptr64 && wp.install == f.install) {
        return false;
    }
    let base = s.ftab.map.install(f.install).base;
    s.ftab.map.bind(f.install, &[(alias, f.idx - base)]);
    true
}

/// After an install's catch-up (the module instantiated, its data
/// initialised): the writes that had to wait for it.
pub(crate) fn apply_pending(shared: &Arc<Mutex<JitShared>>, memory: &gex_mem::GuestMemory) {
    let all: Vec<Arc<WinePe>> = {
        let s = shared.lock().unwrap_or_else(|e| e.into_inner());
        s.wine_pe.iter().chain(s.wine_pe_split.iter()).cloned().collect()
    };
    for wp in all {
        apply_pending_one(&wp, shared, memory);
    }
}

fn apply_pending_one(wp: &Arc<WinePe>, shared: &Arc<Mutex<JitShared>>, memory: &gex_mem::GuestMemory) {
    // the snapshots first: the module's data is initialised now, and the
    // DLLs mapped for the first time have run nothing yet
    let todo = std::mem::take(&mut *wp.snap_todo.lock().unwrap_or_else(|e| e.into_inner()));
    for dll in todo {
        let snap = Arc::new(snap_data(memory.bytes(), wp.data, &wp.dll_slots, &dll));
        wp.snaps.lock().unwrap_or_else(|e| e.into_inner()).insert(dll, snap);
    }
    let writes = std::mem::take(&mut *wp.pending.lock().unwrap_or_else(|e| e.into_inner()));
    for (at, bytes) in writes {
        if std::env::var_os("GEX_WASM_TRACE").is_some() {
            eprintln!("gex: wine PE: boot variable at {at:#x} <- {bytes:02x?}");
        }
        if let Some(dst) = memory.bytes().get_mut(at as usize..at as usize + bytes.len()) {
            dst.copy_from_slice(&bytes);
        }
    }
    if !wp.entries_bound.swap(true, std::sync::atomic::Ordering::AcqRel) {
        if let Err(e) = bind_entry_thunks(wp, shared, memory) {
            eprintln!("gex-pe: Wine's PE module {}: entry thunks: {e:#}", wp.file.display());
        }
    }
}

/// The ICALL pass's entry thunks (toolchain/sysroot/icall): native functions
/// whose address may reach guest code, as a function-table index. gexrt's
/// `__gexw_entries` bounds the `{ function, thunk }` pairs (table indices,
/// relocated with the data); each thunk is bound at its function's index as
/// a guest address, so i386 code calling or jumping there runs the native
/// function through its adapter. Indices sit below the ICALL floor
/// (0x3f0000), where native code takes a value for a table index anyway.
fn bind_entry_thunks(wp: &WinePe, shared: &Arc<Mutex<JitShared>>, memory: &gex_mem::GuestMemory) -> Result<()> {
    let Some(&off) = wp.data_exports.get("__gexw_entries") else { return Ok(()) };
    let at = wp.data + off as u64;
    // A pointer of the module's (an LLP64 module's are 8 bytes, its values
    // below 4 GiB: the lowering's addresses and table indices).
    let w = if wp.ptr64 { 8 } else { 4 };
    let ptr = |a: u64| memory_u32(memory, a);
    let (start, stop) = (ptr(at)? as u64, ptr(at + w)? as u64);
    if start == 0 || stop <= start {
        return Ok(());
    }
    let base = shared.lock().unwrap_or_else(|e| e.into_inner()).ftab.map.install(wp.install).base;
    let read = |start: u64, stop: u64| -> Result<Vec<(u64, u32)>> {
        let mut out = Vec::with_capacity(((stop.saturating_sub(start)) / (2 * w)) as usize);
        let mut a = start;
        while a + 2 * w <= stop {
            let (f, t) = (ptr(a)?, ptr(a + w)?);
            a += 2 * w;
            if f < base || t < base || f as u64 >= 0x3f0000 {
                continue;
            }
            out.push((f as u64, t - base));
        }
        Ok(out)
    };
    let mut pairs = read(start, stop)?;
    // Thunks made where a function is handed out but not defined: only for
    // functions no defining object gave one (it knows the convention).
    if let Some(&off) = wp.data_exports.get("__gexw_entries_decl") {
        let at = wp.data + off as u64;
        let (s2, e2) = (ptr(at)? as u64, ptr(at + w)? as u64);
        if s2 != 0 && e2 > s2 {
            let strong: std::collections::HashSet<u64> = pairs.iter().map(|&(f, _)| f).collect();
            let mut seen = std::collections::HashSet::new();
            for (f, t) in read(s2, e2)? {
                if !strong.contains(&f) && seen.insert(f) {
                    pairs.push((f, t));
                }
            }
        }
    }
    if std::env::var_os("GEX_WASM_TRACE").is_some() {
        eprintln!("gex: wine PE: {} entry thunks bound", pairs.len());
    }
    crate::ftab::bind_pairs(shared, memory, wp.install, &pairs)
}

/// The variables gex-kernel's NT boot fills in ntdll (Wine's unix side
/// does it in `load_ntdll_functions`), by link name: in a native ntdll they
/// are the module's, whose data the boot's writes went past (its skeleton
/// points them all at one placeholder until the install). The dispatchers
/// are the i386 trampolines, which guest code and native code (through the
/// ICALL check, whose floor they sit at) both call.
fn ntdll_variables() -> [(&'static str, Vec<u8>); 3] {
    use gex_kernel::win::tramp;
    [
        ("__wine_syscall_dispatcher", (tramp::SYSCALL_DISPATCHER_32 as u32).to_le_bytes().to_vec()),
        ("__wine_unix_call_dispatcher", (tramp::UNIX_CALL_32 as u32).to_le_bytes().to_vec()),
        ("__wine_unixlib_handle", gex_kernel::win::NTDLL_HANDLE.to_le_bytes().to_vec()),
    ]
}

/// pelink's `gexw.forwards` custom section: (export name, `dll!name` it
/// reaches).
fn forwards(wasm: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let Ok(wasmparser::Payload::CustomSection(c)) = payload {
            if c.name() == "gexw.forwards" {
                for line in String::from_utf8_lossy(c.data()).lines() {
                    if let Some((a, b)) = line.split_once(' ') {
                        out.push((a.to_string(), b.to_string()));
                    }
                }
            }
        }
    }
    out
}

/// Exported i32 (or a wasm64 module's i64) globals with constant
/// initialisers: a PIC module's data symbols, as offsets from `__memory_base`.
/// Wine's PE module, read, parsed and loaded: what every process that
/// installs it shares (all but its data's address). Kept for the host
/// process by path, size and modification time, so a Wine child does not
/// read, hash and parse the 8.6 MB module again (0.3 s of each child's
/// start).
struct ParsedModule {
    size: u32,
    align: u32,
    n_names: u32,
    slots: std::collections::HashMap<String, u32>,
    any_dll: std::collections::HashMap<String, u32>,
    data_exports: std::collections::HashMap<String, u32>,
    /// With modules of their own (pelink's `gexw.modules`): each DLL's module file.
    modules: std::collections::HashMap<String, String>,
    dll_slots: Arc<Vec<DllRange>>,
    module: wasmtime::Module,
}

/// `probe`: a 32-bit Wine module, whose inline probe (gexrt's `gex_ftab_idx`) reads
/// the probe cache, refused when it was built for another layout.
fn parsed_module(file: &std::path::Path, engine: &wasmtime::Engine, t0: std::time::Instant, probe: bool) -> Result<Arc<ParsedModule>> {
    type Memo = Vec<((std::path::PathBuf, u64, Option<std::time::SystemTime>), wasmtime::Engine, Arc<ParsedModule>)>;
    static MEMO: Mutex<Memo> = Mutex::new(Vec::new());
    let md = std::fs::metadata(file).with_context(|| format!("reading Wine's PE module {}", file.display()))?;
    let id = (file.to_path_buf(), md.len(), md.modified().ok());
    if let Some((_, _, pm)) = MEMO.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(i, e, _)| *i == id && wasmtime::Engine::same(e, engine)) {
        return Ok(Arc::clone(pm));
    }
    let wasm = std::fs::read(file).with_context(|| format!("reading Wine's PE module {}", file.display()))?;
    if probe {
        if let Some(e) = gex_cgen::header::abi_refusal(&wasm, &format!("Wine's PE module {}", file.display())) {
            bail!("{e}");
        }
    }
    let (size, align) = crate::libprov::dylink_memory(&wasm)?;
    let names = gex_cgen::provider::slot_names(&wasm)?;
    let mut slots = std::collections::HashMap::new();
    let mut any_dll = std::collections::HashMap::new();
    for (k, n) in names.iter().enumerate() {
        if let Some(n) = n {
            if let Some((_, export)) = n.split_once("!gexp_") {
                any_dll.entry(export.to_string()).or_insert(k as u32);
            }
            slots.insert(n.clone(), k as u32);
        }
    }
    // Forwarded names (pelink's `gexw.forwards`): a re-export of one
    // binds to the adapter of what the forward reaches.
    for (name, target) in forwards(&wasm) {
        if let Some((dll, fname)) = target.split_once('!') {
            if let Some(&k) = slots.get(&format!("{dll}!gexp_{fname}")) {
                any_dll.entry(name).or_insert(k);
            }
        }
    }
    let mut modules = std::collections::HashMap::new();
    for payload in wasmparser::Parser::new(0).parse_all(&wasm) {
        if let Ok(wasmparser::Payload::CustomSection(c)) = payload {
            if c.name() == "gexw.modules" {
                for line in String::from_utf8_lossy(c.data()).lines() {
                    if let Some((d, f)) = line.split_once(' ') {
                        modules.insert(d.to_string(), f.to_string());
                    }
                }
            }
        }
    }
    let t1 = std::time::Instant::now();
    let _ = gex_cache::module_key(engine, &wasm);
    let t2 = std::time::Instant::now();
    let module = gex_cache::load_or_compile_module(engine, &wasm).with_context(|| format!("compiling {}", file.display()))?;
    crate::aot_share::note(&module, crate::aot_share::Kind::Provider);
    if crate::wasm_trace() {
        eprintln!(
            "gex: wine PE: {} KiB module: read and parsed in {:.3}s, keyed in {:.3}s, loaded in {:.3}s",
            wasm.len() >> 10,
            (t1 - t0).as_secs_f64(),
            (t2 - t1).as_secs_f64(),
            t2.elapsed().as_secs_f64()
        );
    }
    let pm = Arc::new(ParsedModule {
        size: size as u32,
        align: align as u32,
        n_names: names.len() as u32,
        slots,
        any_dll,
        data_exports: exported_offsets(&wasm)?,
        modules,
        dll_slots: Arc::new(dll_slots(&wasm)),
        module,
    });
    MEMO.lock().unwrap_or_else(|e| e.into_inner()).push((id, engine.clone(), Arc::clone(&pm)));
    Ok(pm)
}

fn exported_offsets(wasm: &[u8]) -> Result<std::collections::HashMap<String, u32>> {
    let mut imported_globals = 0u32;
    let mut inits: Vec<Option<u32>> = Vec::new();
    let mut out = std::collections::HashMap::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        match payload? {
            wasmparser::Payload::ImportSection(r) => {
                for imp in r {
                    if let wasmparser::TypeRef::Global(_) = imp?.ty {
                        imported_globals += 1;
                    }
                }
            }
            wasmparser::Payload::GlobalSection(r) => {
                for g in r {
                    let g = g?;
                    let mut ops = g.init_expr.get_operators_reader();
                    inits.push(match ops.read()? {
                        wasmparser::Operator::I32Const { value } => Some(value as u32),
                        // a wasm64 module's, lowered: offsets below 4 GiB
                        wasmparser::Operator::I64Const { value } => u32::try_from(value).ok(),
                        _ => None,
                    });
                }
            }
            wasmparser::Payload::ExportSection(r) => {
                for e in r {
                    let e = e?;
                    if e.kind == wasmparser::ExternalKind::Global && e.index >= imported_globals {
                        if let Some(Some(v)) = inits.get((e.index - imported_globals) as usize) {
                            out.insert(e.name.to_string(), *v);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn install_shared(
    pe: &gex_ld::pe::PeImage,
    path: &str,
    base: u64,
    sec: &gex_ld::pe::Section,
    state: &mut HostState,
    shared: &Arc<Mutex<JitShared>>,
    engine: &Engine,
    memory: &gex_mem::GuestMemory,
) -> Result<usize> {
    // GEXWREF1 module\0 dll\0 u32 count { u32 index, name\0 }
    let at = (base + sec.rva as u64) as usize;
    let raw = memory.bytes().get(at..at + sec.vsize as usize).ok_or_else(|| anyhow!("{path}: .wasmref outside memory"))?.to_vec();
    if raw.get(..8) != Some(&b"GEXWREF1"[..]) {
        bail!("{path}: .wasmref is not GEXWREF1");
    }
    let mut p = 8;
    let cstr = |p: &mut usize| -> Result<String> {
        let end = raw[*p..].iter().position(|&c| c == 0).ok_or_else(|| anyhow!("{path}: .wasmref truncated"))? + *p;
        let s = String::from_utf8_lossy(&raw[*p..end]).into_owned();
        *p = end + 1;
        Ok(s)
    };
    let module_name = cstr(&mut p)?;
    let dll = cstr(&mut p)?;
    let count = u32::from_le_bytes(raw.get(p..p + 4).ok_or_else(|| anyhow!("{path}: .wasmref truncated"))?.try_into()?);
    p += 4;
    let mut data_exports = Vec::new();
    for _ in 0..count {
        let idx = u32::from_le_bytes(raw.get(p..p + 4).ok_or_else(|| anyhow!("{path}: .wasmref truncated"))?.try_into()?);
        p += 4;
        data_exports.push((idx, cstr(&mut p)?));
    }

    // The module: installed once, at the first skeleton that names it. A
    // DLL linked as a module of its own (`pelink.py --split`) binds its
    // imports to the core's exports, the core (winepe.wasm beside it)
    // installed first if no skeleton of its has been (a program's image
    // maps before ntdll's).
    // beside the image's file itself, a link to it followed (when the link
    // names a host path)
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| std::path::PathBuf::from(path));
    let beside = |name: &str| {
        let file = real.with_file_name(name);
        // a twin install's memory64 module, beside it
        let twin = real.with_file_name(format!("{name}.m64"));
        if gex_abi::mem64() && twin.is_file() { twin } else { file }
    };
    let file = beside(&module_name);
    let core_file = beside(CORE_MODULE);
    // one install at a time: two threads mapping skeletons of one module
    // (or of modules installing a shared dependency) install it once
    static INSTALLING: Mutex<()> = Mutex::new(());
    let installing = INSTALLING.lock().unwrap_or_else(|e| e.into_inner());
    let same = |wp: &Arc<WinePe>, f: &std::path::Path| wp.file == f || wp.file.file_name() == f.file_name();
    let find = |f: &std::path::Path| {
        let s = shared.lock().unwrap_or_else(|e| e.into_inner());
        s.wine_pe.iter().chain(s.wine_pe_split.iter()).find(|wp| wp.ptr64 == pe.is64 && same(wp, f)).cloned()
    };
    let is_core = file.file_name() == core_file.file_name() || !core_file.is_file();
    let core = match (find(&core_file), is_core) {
        (Some(c), _) => Some(c),
        (None, true) => None,
        (None, false) => Some(install_module(&core_file, None, pe.is64, state, shared, engine, memory)?),
    };
    let wp = match find(&file) {
        Some(wp) => wp,
        None => install_module(&file, if is_core { None } else { core.clone() }, pe.is64, state, shared, engine, memory)?,
    };
    drop(installing);

    // This DLL's bindings: its own module's adapters, and for a `-import`
    // re-export the owner's, in whichever install has it (the core's, or
    // another module of its own: advapi32's OpenSCManagerW is sechost's).
    let slot = |n: &str| wp.slots.get(n).copied();
    let missing = slot(&format!("{dll}!gexp__missing"))
        .or_else(|| wp.slots.iter().find(|(k, _)| k.ends_with("!gexp__missing")).map(|(_, v)| *v))
        .ok_or_else(|| anyhow!("{path}: Wine's PE module has no gexp__missing"))?;
    let others: Vec<Arc<WinePe>> = {
        let s = shared.lock().unwrap_or_else(|e| e.into_inner());
        core.iter().chain(s.wine_pe_split.iter()).filter(|c| !Arc::ptr_eq(c, &wp) && c.ptr64 == wp.ptr64).cloned().collect()
    };
    let mut pairs = Vec::new();
    let mut other_pairs: Vec<Vec<(u64, u32)>> = vec![Vec::new(); others.len()];
    for e in pe.exports.entries.iter().filter(|e| e.rva != 0 && e.forwarder.is_none()) {
        let own = e
            .names
            .iter()
            .find_map(|n| slot(&format!("{dll}!gexp_{n}")))
            .or_else(|| slot(&format!("{dll}!gexp_#{}", e.ordinal)))
            .or_else(|| e.names.iter().find_map(|n| wp.any_dll.get(n).copied()));
        match own {
            Some(k) => pairs.push((base + e.rva as u64, k)),
            None => match others.iter().enumerate().find_map(|(i, c)| e.names.iter().find_map(|n| c.any_dll.get(n).copied()).map(|k| (i, k))) {
                Some((i, k)) => other_pairs[i].push((base + e.rva as u64, k)),
                None => pairs.push((base + e.rva as u64, missing)),
            },
        }
    }
    if let (Some(k), true) = (slot(&format!("{dll}!gexp__entry")), pe.entry_rva != 0) {
        pairs.push((base + pe.entry_rva as u64, k));
    }
    pairs.sort_unstable();
    pairs.dedup_by_key(|&mut (a, _)| a);

    // Data exports: point the export table at the module's data.
    let dir = pe.dirs.first().map(|d| d.0).unwrap_or(0);
    if dir != 0 && !data_exports.is_empty() {
        let functions = memory_u32(memory, base + dir as u64 + 28)? as u64;
        for (idx, link) in &data_exports {
            let Some(off) = wp.data_exports.get(&format!("{dll}!{link}")) else {
                eprintln!("gex-pe: {path}: data export {link} is not in Wine's PE module");
                continue;
            };
            let target = (wp.data + *off as u64).wrapping_sub(base) as u32;
            let at = (base + functions + 4 * *idx as u64) as usize;
            memory.bytes()[at..at + 4].copy_from_slice(&target.to_le_bytes());
        }
    }

    // The variables below are the loaded DLL's, written at its first
    // mapping only. Written again at a later one (wineboot's install maps
    // DLLs as image views), they pointed winecrt0 at the view, and under
    // native Wine's own Unix side (32-bit Wine) reset ntdll's __wine_unixlib_handle,
    // which load_ntdll_functions had set, to gex-kernel's handle: every
    // later Unix call of ntdll's went through a table at address 1.
    // A DLL freed and loaded again maps anew, often at another base (the
    // Start menu's Control Panel applets: setupapi, cfgmgr32, rpcrt4): its
    // first mapping is gone, released to a PROT_NONE reservation or with
    // the new one at its place, and this one is the loaded DLL's. Kept,
    // winecrt0's base of it (delay loads, unix calls, resources) and
    // GetModuleHandleEx's FROM_ADDRESS named whatever had landed at the old
    // one.
    // Released is also another mapping at the old base since, not this
    // DLL's: a program's own data there (Telegram's crypt32, loaded again
    // after its first was freed, asked NtQueryVirtualMemory about the old
    // base and its DllMain failed). Still the DLL's, its .wasmref is there.
    let old = wp.mapped.lock().unwrap_or_else(|e| e.into_inner()).get(&dll).copied();
    let still_there = |o: u64| {
        let at = (o + sec.rva as u64) as usize;
        memory.bytes().get(at..at + raw.len()).is_some_and(|b| b == &raw[..])
    };
    let released = old.is_some_and(|o| {
        o == base || state.with_kernel(|k| k.mem.lock().vma_at(o).is_none_or(|v| v.prot == 0)) || !still_there(o)
    });
    let first_mapping = {
        let mut m = wp.mapped.lock().unwrap_or_else(|e| e.into_inner());
        let first = !m.contains_key(&dll) || released;
        if first {
            m.insert(dll.clone(), base);
        }
        first
    };
    // Loaded again, the DLL starts from its data as it was before its first
    // load: Windows maps a fresh copy, and Wine's DLLs count on it (a
    // one-time initialisation's flag set and what it made freed by the
    // detach; urlmon's second detach called through a pointer the first had
    // cleared). First loaded, that is what the next apply_pending keeps.
    if released {
        let snap = wp.snaps.lock().unwrap_or_else(|e| e.into_inner()).get(&dll).cloned();
        if let Some(snap) = snap {
            wp.pending.lock().unwrap_or_else(|e| e.into_inner()).extend(snap_writes(&snap));
        }
    } else if first_mapping {
        wp.snap_todo.lock().unwrap_or_else(|e| e.into_inner()).push(dll.clone());
    }
    // winecrt0's base of this DLL (its unix calls, delay loads and version
    // resources find the DLL by it): the skeleton's.
    if let (true, Some(off)) = (first_mapping, wp.data_exports.get(&format!("{dll}!__gexw_image_base"))) {
        let bytes = if wp.ptr64 { base.to_le_bytes().to_vec() } else { (base as u32).to_le_bytes().to_vec() };
        wp.pending.lock().unwrap_or_else(|e| e.into_inner()).push((wp.data + *off as u64, bytes));
    }
    // (64-bit Wine's ntdll has Wine's own Unix side, whose load_ntdll_functions
    // sets them: gex-kernel's are i386 trampolines)
    if first_mapping && dll == "ntdll.dll" && !wp.ptr64 {
        let mut pending = wp.pending.lock().unwrap_or_else(|e| e.into_inner());
        for (name, bytes) in ntdll_variables() {
            if let Some(off) = wp.data_exports.get(&format!("ntdll.dll!{name}")) {
                pending.push((wp.data + *off as u64, bytes));
            }
        }
    }

    let re = other_pairs.iter().map(Vec::len).sum::<usize>();
    let n = pairs.len() + re;
    crate::ftab::bind_pairs(shared, memory, wp.install, &pairs)?;
    for (c, p) in others.iter().zip(&other_pairs).filter(|(_, p)| !p.is_empty()) {
        crate::ftab::bind_pairs(shared, memory, c.install, p)?;
    }
    if crate::wasm_trace() {
        eprintln!("gex: wine PE: {dll}: {} exports bound in {}{}", n, file.display(), if re == 0 { String::new() } else { format!(" ({re} re-exports in other modules)") });
    }
    Ok(n)
}

/// A call from one of Wine's modules of their own into a function another
/// one defines that was not installed when the caller was: through the
/// slot `off` after the caller's own, filled in each store once both are
/// instantiated there.
#[derive(Clone)]
pub(crate) struct Late {
    importer: u32,
    off: u32,
    name: String,
    provider: Option<u32>,
    /// The importer's mode (`WinePe::ptr64`): only a module of that mode provides it.
    ptr64: bool,
}

/// After `module`'s instantiation in a store (at table base `base`): the
/// `Late` slots it is either side of whose other side this store has.
pub(crate) fn fill_late(c: &mut wasmtime::StoreContextMut<'_, HostState>, table: wasmtime::Table, base: u32, module: u32) -> Result<()> {
    let Some(sh) = c.data().jit.as_ref().map(|j| Arc::clone(&j.shared)) else { return Ok(()) };
    let todo: Vec<(u32, u32, u32, String, u32)> = {
        let s = sh.lock().unwrap_or_else(|e| e.into_inner());
        let mine: Vec<&Late> = s.wine_pe_late.iter().filter(|l| l.provider.is_some() && (l.importer == module || l.provider == Some(module))).collect();
        if mine.is_empty() {
            return Ok(());
        }
        let base_of = |m: u32| if m == module { Some(base) } else { s.ftab.map.installs().iter().find(|i| i.module == m).map(|i| i.base) };
        mine.iter().filter_map(|l| Some((l.importer, l.off, l.provider?, l.name.clone(), base_of(l.importer)?))).collect()
    };
    for (importer, off, provider, name, ibase) in todo {
        let dyl = |c: &wasmtime::StoreContextMut<'_, HostState>, m: u32| c.data().jit.as_ref().and_then(|j| j.ftab.as_ref()).and_then(|s| s.dyl.get(&m).copied());
        let (Some(_), Some(p)) = (dyl(c, importer), dyl(c, provider)) else { continue };
        if let Some(f) = p.get_func(&mut *c, &name) {
            table.set(&mut *c, (ibase + off) as u64, wasmtime::Ref::Func(Some(f)))?;
        }
    }
    Ok(())
}

/// The C functions a module may import from a module of the process's
/// other mode (wasm32 i386 Wine calling the lowered wasm64 side, or back):
/// an export of the exact wasm type, never a slot (as the page's,
/// hosts/browser cgen/wasmdll.rs). WoW64's native i386 ntdll makes its
/// system calls and Unix calls through them.
const CROSS_MODE: &[&str] = &["wow64cpu.dll!__wine_gex_syscall32", "wow64cpu.dll!__wine_gex_unix_call32"];

/// The wasm type of `module`'s `env` import `name` (or its export, `export`).
fn cross_mode_type(module: &wasmtime::Module, name: &str, export: bool) -> Result<String> {
    let ty = if export {
        module.exports().find(|e| e.name() == name).map(|e| e.ty())
    } else {
        module.imports().find(|i| i.module() == "env" && i.name() == name).map(|i| i.ty())
    };
    match ty {
        Some(wasmtime::ExternType::Func(f)) => Ok(format!("{:?} -> {:?}", f.params().collect::<Vec<_>>(), f.results().collect::<Vec<_>>())),
        _ => bail!("{name}: no function of that name to bind across modes"),
    }
}

/// Whether a Wine module of this pointer size is of the process's other
/// mode: an ILP32 one (32-bit Wine's, PE32 skeletons) in an x86-64 process.
fn module_alt(is64: bool) -> bool {
    gex_abi::X86 && !is64 && !gex_xlate::default_compat()
}

/// The core module's file name: the module a skeleton of a DLL linked as
/// a module of its own finds beside itself.
const CORE_MODULE: &str = "winepe.wasm";

/// Install one of Wine's PE modules (every store instantiates it): its data
/// allocated, and with `core` (a module of its own) its imports of the
/// core bound to the core's exports: functions it calls to the core's
/// functions, a core function's address it takes (`GOT.func`) to a slot
/// after its own that each store fills with the core's function. The rest
/// is left to the instantiation (memory, table, bases, `gex.*`). The core's
/// own binds are empty: it is recorded per store as a dylink module, for
/// the others to bind to.
#[allow(clippy::too_many_arguments)]
fn install_module(
    file: &std::path::Path,
    core: Option<Arc<WinePe>>,
    is64: bool,
    state: &mut HostState,
    shared: &Arc<Mutex<JitShared>>,
    engine: &Engine,
    memory: &gex_mem::GuestMemory,
) -> Result<Arc<WinePe>> {
    let t0 = std::time::Instant::now();
    let alt = module_alt(is64);
    if alt {
        crate::ensure_alt_dir_in(state, shared)?;
    }
    let pm = parsed_module(file, engine, t0, !is64)?;
    let (size, align) = (pm.size, pm.align);
    let a = 1u64 << align;
    // (an LLP64 module's above the images: a PE32+ export's RVA is
    // added to its base in 64 bits, so data below an image's base
    // would be past 4 GiB; and an i386 module's in a 64-bit process,
    // whose 64-bit side finds ntdll's data exports so, wow64's
    // Wow64Transition, there too, but below 2 GiB, as the page's: the
    // 32-bit program sees it)
    let data = if alt {
        crate::libprov::alloc_below_2g(state, (size as u64).max(16) + a, "[wine PE data]")?
    } else if is64 {
        crate::libprov::alloc_above_code_data(state, (size as u64).max(16) + a, "[wine PE data]")?
    } else {
        crate::libprov::alloc_top(state, (size as u64).max(16) + a, "[wine PE data]")?
    };
    let data = (data + a - 1) & !(a - 1);
    let module = pm.module.clone();
    let mut len = pm.n_names;
    let funcs_of = |m: &Module| -> Vec<String> {
        m.exports().filter(|e| matches!(e.ty(), wasmtime::ExternType::Func(_))).map(|e| e.name().to_string()).collect()
    };
    let mine: std::collections::HashSet<String> = funcs_of(&module).into_iter().collect();
    // The modules it calls into or takes function addresses of, installed
    // first (no code of theirs runs until Wine loads their DLLs): its
    // imports then bind to their functions directly, and a function
    // address is the defining module's own. Only a cycle's far side is
    // left to `Late` slots.
    if core.is_some() {
        thread_local! {
            static BUSY: std::cell::RefCell<Vec<std::path::PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let mut deps = std::collections::BTreeSet::new();
        // (not a DLL it only delay-imports, which it marks `gexw-delay!DLL`:
        // its calls load the DLL first, and then are bound, through `Late`
        // slots, when Wine loads it)
        let delayed: std::collections::HashSet<&str> =
            module.exports().filter_map(|e| e.name().strip_prefix("gexw-delay!")).collect();
        for imp in module.imports() {
            if let (true, Some((dll, _))) = (matches!(imp.module(), "env" | "GOT.func"), imp.name().split_once('!')) {
                if delayed.contains(dll) {
                    continue;
                }
                if let Some(f) = pm.modules.get(dll) {
                    deps.insert(f.clone());
                }
            }
        }
        BUSY.with(|b| b.borrow_mut().push(file.to_path_buf()));
        let r = (|| -> Result<()> {
            for f in deps {
                let path = file.with_file_name(&f);
                let installed = {
                    let s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    s.wine_pe.iter().chain(s.wine_pe_split.iter()).any(|wp| wp.ptr64 == is64 && wp.file.file_name() == path.file_name())
                };
                let busy = BUSY.with(|b| b.borrow().iter().any(|p| p.file_name() == path.file_name()));
                if installed || busy || !path.is_file() {
                    continue;
                }
                install_module(&path, core.clone(), is64, state, shared, engine, memory)?;
            }
            Ok(())
        })();
        BUSY.with(|b| b.borrow_mut().pop());
        r?;
    }
    // what the core exports, and the functions every installed module exports
    let (have_core, others, cross) = {
        let s = shared.lock().unwrap_or_else(|e| e.into_inner());
        let have_core: std::collections::HashSet<String> = match &core {
            Some(c) => s.ftab.modules[c.module as usize].exports().map(|e| e.name().to_string()).collect(),
            None => Default::default(),
        };
        let mut others = std::collections::HashMap::new();
        let mut cross = std::collections::HashMap::new();
        for wp in s.wine_pe.iter().chain(s.wine_pe_split.iter()) {
            for n in funcs_of(&s.ftab.modules[wp.module as usize]) {
                if wp.ptr64 == is64 {
                    others.entry(n).or_insert(wp.module);
                } else if CROSS_MODE.contains(&n.as_str()) {
                    let ty = cross_mode_type(&s.ftab.modules[wp.module as usize], &n, true)?;
                    cross.entry(n).or_insert((wp.module, ty, wp.file.display().to_string()));
                }
            }
        }
        (have_core, others, cross)
    };
    let mut imports = Vec::new();
    let mut lates = Vec::new();
    let mut gots = Vec::new(); // (import index, name), bound below
    for imp in module.imports() {
        let (m, name) = (imp.module().to_string(), imp.name().to_string());
        let b = if m == "GOT.func" {
            gots.push((imports.len(), name.clone()));
            crate::dylink::NBind::Shim
        } else if m == "env" && core.is_some() && have_core.contains(&name) {
            crate::dylink::NBind::Export { module: core.as_ref().unwrap().module, name: name.clone() }
        } else if m == "env" && matches!(imp.ty(), wasmtime::ExternType::Func(_)) && !gex_dylink::shim_import(&m, &name) {
            // another module's: bound to it if it is installed, else a call
            // through a slot filled when it is (`fill_late`; the core's calls
            // into user32, say)
            match (others.get(&name), cross.get(&name)) {
                (Some(&p), _) => crate::dylink::NBind::Export { module: p, name: name.clone() },
                (None, Some((p, ty, file))) => {
                    let want = cross_mode_type(&module, &name, false)?;
                    if want != *ty {
                        bail!("{name}: imported as {want}, {file} exports it as {ty}");
                    }
                    crate::dylink::NBind::Export { module: *p, name: name.clone() }
                }
                (None, None) => {
                    lates.push((len, name.clone(), None));
                    len += 1;
                    crate::dylink::NBind::LateOwn(len - 1)
                }
            }
        } else {
            crate::dylink::NBind::Shim
        };
        imports.push((m, name, b));
    }
    let number = {
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        let m = s.ftab.modules.len() as u32;
        // A function's address (GOT.func), its own or another module's: one
        // slot per function in the process. That is the defining module's
        // own when its element segment has the function (it takes the
        // address itself, and its code and data use that slot); else the
        // slot of the first module naming it, filled in each store with the
        // function once the defining module is instantiated there. The
        // modules a module calls into are installed before it, so only a
        // cycle's far side can miss the defining module's own slot.
        for (i, name) in gots {
            // the defining module's own slot for it, when its element segment has it
            let own = if mine.contains(&name) { pm.slots.get(&name).copied() } else { None };
            let theirs = others.get(&name).and_then(|&p| {
                let k = s.wine_pe.iter().chain(s.wine_pe_split.iter()).find(|wp| wp.module == p)?.slots.get(&name).copied()?;
                Some(s.ftab.map.installs().iter().find(|x| x.module == p)?.base + k)
            });
            let b = match (own, theirs, s.wine_pe_canon.get(&(is64, name.clone())).copied()) {
                (Some(k), _, _) => crate::dylink::NBind::OwnSlot(k),
                (None, Some(v), _) => crate::dylink::NBind::Global(v),
                (None, None, c) => match c {
                Some((owner, off)) => {
                    let base = s.ftab.map.installs().iter().find(|x| x.module == owner).map(|x| x.base);
                    match base {
                        Some(base) => crate::dylink::NBind::Global(base + off),
                        None => bail!("{}: {name}'s slot belongs to a module not yet published", file.display()),
                    }
                }
                None => {
                    let off = len;
                    len += 1;
                    s.wine_pe_canon.insert((is64, name.clone()), (m, off));
                    let provider = if mine.contains(&name) { Some(m) } else { others.get(&name).copied() };
                    lates.push((off, name.clone(), provider));
                    crate::dylink::NBind::OwnSlot(off)
                }
                },
            };
            if crate::wasm_trace() {
                eprintln!("gex: wine PE: {}: &{name}: {:?}", file.display(), b);
            }
            imports[i].2 = b;
        }
        let binds = crate::dylink::Binds { path: file.display().to_string(), imports, extra: Vec::new(), tls: None, own_tls: true };
        s.ftab.dylink.insert(m, Arc::new(binds));
        // its calls and addresses into modules not yet installed; earlier
        // modules' into it
        s.wine_pe_late.extend(lates.into_iter().map(|(off, name, provider)| Late { importer: m, off, name, provider, ptr64: is64 }));
        for k in 0..s.wine_pe_late.len() {
            let l = &s.wine_pe_late[k];
            if l.provider.is_some() || l.importer == m || !mine.contains(&l.name) {
                continue;
            }
            if l.ptr64 != is64 {
                if !CROSS_MODE.contains(&l.name.as_str()) {
                    continue;
                }
                let want = cross_mode_type(&s.ftab.modules[l.importer as usize], &l.name, false)?;
                let have = cross_mode_type(&module, &l.name, true)?;
                if want != have {
                    bail!("{}: imported as {want}, {} exports it as {have}", l.name, file.display());
                }
            }
            s.wine_pe_late[k].provider = Some(m);
        }
        m
    };
    let install = crate::ftab::publish_stateful(shared, memory, module, len, &[], crate::libprov::Extras { memory_base: data })?;
    {
        // its function pointers are its slots: below NATIVE_FN_TOP, or they
        // would read as data addresses
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        // bound at each DLL's skeleton, freed and loaded again: its slots
        // are its functions however the addresses come and go
        s.ftab.map.keep(install);
        let inst = s.ftab.map.install(install);
        let end = inst.base as u64 + inst.len as u64;
        if end > gex_abi::ftab::NATIVE_FN_TOP {
            let msg = format!(
                "{}: its {} table slots, from index {:#x} ({} indices taken before it), end at {end:#x}, past {:#x}: \
                 its function pointers would read as data addresses",
                file.display(), inst.len, inst.base, inst.base, gex_abi::ftab::NATIVE_FN_TOP
            );
            eprintln!("gex-pe: {msg}");
            bail!("{msg}");
        }
    }
    if alt {
        shared.lock().unwrap_or_else(|e| e.into_inner()).ftab.alt_installs.insert(install);
    }
    crate::dylink::published();
    let wp = Arc::new(WinePe {
        install,
        file: file.to_path_buf(),
        module: number,
        data,
        slots: pm.slots.clone(),
        any_dll: pm.any_dll.clone(),
        data_exports: pm.data_exports.clone(),
        pending: Mutex::new(Vec::new()),
        entries_bound: Default::default(),
        ptr64: is64,
        alt,
        mapped: Default::default(),
        data_size: size as u64,
        dll_slots: Arc::clone(&pm.dll_slots),
        snaps: Default::default(),
        snap_todo: Default::default(),
    });
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    // (`wine_pe` is the core of the process's own mode; the other mode's
    // core is with the modules of their own)
    if core.is_none() && !alt {
        s.wine_pe = Some(wp.clone());
    } else {
        s.wine_pe_split.push(wp.clone());
    }
    Ok(wp)
}

fn memory_u32(memory: &gex_mem::GuestMemory, addr: u64) -> Result<u32> {
    let b = memory.bytes().get(addr as usize..addr as usize + 4).ok_or_else(|| anyhow!("read outside memory at {addr:#x}"))?;
    Ok(u32::from_le_bytes(b.try_into()?))
}
