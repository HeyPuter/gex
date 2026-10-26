//! Native Wine DLLs on wasm32 (stream W, docs/wine-native.md §3.2): a DLL
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
    // Compiled once and kept beside the DLL by content, as providers are
    // (libprov::cwasm_path); a runtime without a compiler (gex-rt) only loads.
    let module = gex_cache::load_or_compile_at(engine, &crate::libprov::cwasm_path(std::path::Path::new(path), &wasm), &wasm)
        .with_context(|| format!("compiling {path}'s wasm module"))?;
    crate::aot_share::note(&module, crate::aot_share::Kind::Provider);
    let _ = state;
    crate::ftab::publish_stateful(shared, memory, module, slots.len() as u32, &pairs, crate::libprov::Extras { memory_base: data })?;
    Ok(Some(n))
}

/// Wine's PE side as one module (pelink.py, docs/wine-native.md §3.14): the
/// install, where its data is, its slots by name and its data exports'
/// offsets.
pub(crate) struct WinePe {
    pub(crate) install: u32,
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
    /// The DLLs whose skeleton has been mapped: a later mapping of one (an
    /// image view, LoadLibraryEx as an image resource) binds its exports
    /// but writes none of the variables, which belong to the loaded DLL.
    mapped: Mutex<std::collections::HashSet<String>>,
}

/// After an install's catch-up (the module instantiated, its data
/// initialised): the writes that had to wait for it.
pub(crate) fn apply_pending(shared: &Arc<Mutex<JitShared>>, memory: &gex_mem::GuestMemory) {
    let Some(wp) = shared.lock().unwrap_or_else(|e| e.into_inner()).wine_pe.clone() else { return };
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
        if let Err(e) = bind_entry_thunks(&wp, shared, memory) {
            eprintln!("gex-nt: Wine's PE module: entry thunks: {e:#}");
        }
    }
}

/// The ICALL pass's entry thunks (tools/wine-wasm/icall): native functions
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
    let t1 = std::time::Instant::now();
    let cwasm = crate::libprov::cwasm_path(file, &wasm);
    let t2 = std::time::Instant::now();
    let module = gex_cache::load_or_compile_at(engine, &cwasm, &wasm).with_context(|| format!("compiling {}", file.display()))?;
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

    // The module: installed once, at the first skeleton.
    let existing = shared.lock().unwrap_or_else(|e| e.into_inner()).wine_pe.clone();
    let (wp, first) = match existing {
        Some(wp) => (wp, None),
        None => {
            let t0 = std::time::Instant::now();
            let file = std::path::Path::new(path).with_file_name(&module_name);
            // a twin install's memory64 module, beside it (docs/mem64.md §5)
            let twin = std::path::Path::new(path).with_file_name(format!("{module_name}.m64"));
            let file = if gex_abi::mem64() && twin.is_file() { twin } else { file };
            let pm = parsed_module(&file, engine, t0, !pe.is64)?;
            let (size, align) = (pm.size, pm.align);
            let a = 1u64 << align;
            // (an LLP64 module's above the images: a PE32+ export's RVA is
            // added to its base in 64 bits, so data below an image's base
            // would be past 4 GiB)
            let data = if pe.is64 {
                crate::libprov::alloc_above_code(state, (size as u64).max(16) + a, "[wine PE data]")?
            } else {
                crate::libprov::alloc_top(state, (size as u64).max(16) + a, "[wine PE data]")?
            };
            let data = (data + a - 1) & !(a - 1);
            let (slots, any_dll, module, n_names) = (pm.slots.clone(), pm.any_dll.clone(), pm.module.clone(), pm.n_names);
            let data_exports_map = pm.data_exports.clone();
            let wp = WinePe {
                install: 0,
                data,
                slots,
                any_dll,
                data_exports: data_exports_map,
                pending: Mutex::new(Vec::new()),
                entries_bound: Default::default(),
                ptr64: pe.is64,
                mapped: Default::default(),
            };
            (Arc::new(wp), Some((module, n_names)))
        }
    };

    // This DLL's bindings.
    let slot = |n: &str| wp.slots.get(n).copied();
    let missing = slot(&format!("{dll}!gexp__missing"))
        .or_else(|| wp.slots.iter().find(|(k, _)| k.ends_with("!gexp__missing")).map(|(_, v)| *v))
        .ok_or_else(|| anyhow!("{path}: Wine's PE module has no gexp__missing"))?;
    let mut pairs = Vec::new();
    for e in pe.exports.entries.iter().filter(|e| e.rva != 0 && e.forwarder.is_none()) {
        let k = e
            .names
            .iter()
            .find_map(|n| slot(&format!("{dll}!gexp_{n}")))
            .or_else(|| slot(&format!("{dll}!gexp_#{}", e.ordinal)))
            .or_else(|| e.names.iter().find_map(|n| wp.any_dll.get(n).copied()))
            .unwrap_or(missing);
        pairs.push((base + e.rva as u64, k));
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
                eprintln!("gex-nt: {path}: data export {link} is not in Wine's PE module");
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
    let first_mapping = wp.mapped.lock().unwrap_or_else(|e| e.into_inner()).insert(dll.clone());
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

    let n = pairs.len();
    match first {
        Some((module, len)) => {
            let install = crate::ftab::publish_stateful(shared, memory, module, len, &pairs, crate::libprov::Extras { memory_base: wp.data })?;
            let mut wp = Arc::try_unwrap(wp).ok().expect("WinePe not yet shared");
            wp.install = install;
            shared.lock().unwrap_or_else(|e| e.into_inner()).wine_pe = Some(Arc::new(wp));
        }
        None => crate::ftab::bind_pairs(shared, memory, wp.install, &pairs)?,
    }
    Ok(n)
}

fn memory_u32(memory: &gex_mem::GuestMemory, addr: u64) -> Result<u32> {
    let b = memory.bytes().get(addr as usize..addr as usize + 4).ok_or_else(|| anyhow!("read outside memory at {addr:#x}"))?;
    Ok(u32::from_le_bytes(b.try_into()?))
}
