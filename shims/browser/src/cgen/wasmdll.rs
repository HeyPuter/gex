//! Native wasm32 Wine DLLs in the browser (docs/wine-native.md §3.14,
//! §3.15): the native host's `wasmdll`, shared mode. Wine's PE side is one
//! module (pelink.py) behind a skeleton per DLL (a `.wasmref` section names
//! it). The first skeleton to map installs it as a stateful provider (its
//! data from the NT allocator, `PROVIDER | number` in the function table);
//! every later one binds its exports to slots of the same install
//! (`FuncMap::bind`), whose tier-1 bridges every Worker writes as it
//! catches up (`gex_wasmdll_binds_len` and friends, `BINDS`).
//!
//! The protocol with `gex.mjs` (`#cgenProviders`, `#wasmdllCatchUp`):
//!   - a unit's provider `i` with `gex_cgen_provider_wasmdll(u, i)` set is
//!     published by `gex_wasmdll_publish(u, i)`: a provider module number
//!     (the first skeleton: JS catches up, which instantiates the module
//!     from `gex_cgen_pmod_wasm_ptr/len`, then calls
//!     `gex_cgen_pmod_installed` and `gex_wasmdll_installed`), or -2 (bound
//!     into the install: only bridges to write);
//!   - every Worker writes each `BINDS` entry's bridges once.
//!   - The module's `gex.syscall` is JS's (`#nativeSyscall`), so a system
//!     call from native code that traps to JS (a unit or thread to start)
//!     is served there and returns, keeping the native frames below it.

use super::*;
use std::collections::HashMap;

/// Wine's PE module, once its first skeleton mapped.
struct WinePe {
    /// Its provider module number (`PROVIDERS`).
    number: u32,
    /// Its function-table install.
    install: u32,
    /// Its data (`__memory_base`).
    data: u64,
    slots: HashMap<String, u32>,
    /// `gexp_<export>` slots by export name, whichever DLL owns them (a
    /// DLL's `-import` re-exports are served by the owner's adapter).
    any_dll: HashMap<String, u32>,
    /// Data symbols' offsets in its data (exported globals).
    data_exports: HashMap<String, u32>,
    /// Writes into its data that follow its initialisation (ntdll's boot
    /// variables).
    pending: Vec<(u64, Vec<u8>)>,
    wasm: Vec<u8>,
    /// The file `wasm` is from, and its content key, made on first asking
    /// (`gex_cgen_pmod_wasm_key`): from the system-wide cache, so a process
    /// neither hashes the 9 MB nor has JS copy them out when the page
    /// already has the module.
    file: String,
    key: Option<(u64, u64)>,
    installed: bool,
    /// The DLLs whose skeleton has been mapped. A later mapping of one (an
    /// image view: wineboot's install maps DLLs so) binds its exports but
    /// writes none of the variables, which are the loaded DLL's -- under W3
    /// rewriting them reset ntdll's `__wine_unixlib_handle`, which Wine's
    /// own loader had set (as the native host found).
    mapped: std::collections::HashSet<String>,
    /// An LLP64 module (W64, its skeletons PE32+): pointers in its data are
    /// 8 bytes, the entry pairs 16 (as natively).
    ptr64: bool,
}

static WINE_PE: Mutex<Option<WinePe>> = Mutex::new(None);

/// Bridges for addresses bound into the install after it was published.
struct Bind {
    regs: Vec<u32>,
    dirs: Vec<(u64, u32)>,
    slots: u64,
}

static BINDS: Mutex<Vec<Bind>> = Mutex::new(Vec::new());

pub(super) fn is_skeleton(pe: &gex_ld::pe::PeImage) -> bool {
    pe.sections.iter().any(|s| s.name == ".wasmref")
}

/// Whether provider `i` of unit `u` is a native Wine DLL's skeleton.
#[no_mangle]
pub extern "C" fn gex_cgen_provider_wasmdll(u: u32, i: u32) -> i32 {
    lock(&UNITS).get(u as usize).and_then(|x| x.providers.get(i as usize)).is_some_and(|p| p.skeleton.is_some()) as i32
}

/// Publish provider `i` of unit `u`, a skeleton: the module's number for
/// the first (JS installs it), -2 when bound into the install, -1 on an
/// error (said).
#[no_mangle]
pub extern "C" fn gex_wasmdll_publish(u: u32, i: u32) -> i32 {
    let (path, base, pe) = {
        let units = lock(&UNITS);
        let Some(x) = units.get(u as usize) else { return -1 };
        let Some(pe) = x.providers.get(i as usize).and_then(|p| p.skeleton.clone()) else { return -1 };
        (x.path.clone(), x.base, pe)
    };
    match publish(&path, base, &pe) {
        Ok(n) => {
            let mut r = lock(&SKELETON_CODE);
            for sec in pe.sections.iter().filter(|s| s.executable() && s.vsize.max(s.raw_size) > 0) {
                let lo = base + sec.rva as u64;
                r.push((lo, lo + sec.vsize.max(sec.raw_size) as u64));
            }
            n
        }
        Err(e) => {
            crate::warn_line(&format!("gex-nt: {path}: native wasm32 DLL: {e}\n"));
            crate::nt::end_process(0xc000_0135);
        }
    }
}

fn publish(path: &str, base: u64, pe: &gex_ld::pe::PeImage) -> Result<i32, String> {
    let sec = pe.sections.iter().find(|s| s.name == ".wasmref").ok_or("no .wasmref")?;
    let (module_name, dll, data_list) = wasmref(&crate::guest_bytes()[..], base, sec)?;
    let mut g = lock(&WINE_PE);
    let first = g.is_none();
    if first {
        let file = match path.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/{module_name}"),
            None => module_name.clone(),
        };
        let wasm = gex_kernel::nt::file::read_guest_file(&crate::sigs::proc_home().ns(), &file).map_err(|e| format!("reading Wine's PE module {file}: {e:?}"))?;
        let (size, align) = dylink_memory(&wasm)?;
        let a = 1u64 << align;
        // (an LLP64 module's above the images: see `crate::W64_PE_DATA`)
        let data = if pe.is64 {
            alloc_pe_data_window((size as u64).max(16) + a, "[wine PE data]")?
        } else {
            provider_alloc_top_bytes((size as u64).max(16) + a, "[wine PE data]")?
        };
        let data = (data + a - 1) & !(a - 1);
        let names = gex_cgen::provider::slot_names(&wasm).map_err(|e| e.to_string())?;
        let mut slots = HashMap::new();
        let mut any_dll = HashMap::new();
        for (k, n) in names.iter().enumerate() {
            if let Some(n) = n {
                if let Some((_, export)) = n.split_once("!gexp_") {
                    any_dll.entry(export.to_string()).or_insert(k as u32);
                }
                slots.insert(n.clone(), k as u32);
            }
        }
        for (name, target) in forwards(&wasm) {
            if let Some((d, f)) = target.split_once('!') {
                if let Some(&k) = slots.get(&format!("{d}!gexp_{f}")) {
                    any_dll.entry(name).or_insert(k);
                }
            }
        }
        let data_exports = exported_offsets(&wasm)?;
        let number = lock(&PROVIDERS).len() as u32;
        *g = Some(WinePe { number, install: 0, data, slots, any_dll, data_exports, pending: Vec::new(), wasm, file, key: None, installed: false, mapped: Default::default(), ptr64: pe.is64 });
    }
    let wp = g.as_mut().expect("Wine's PE module");
    let slot = |n: &str| wp.slots.get(n).copied();
    let missing = slot(&format!("{dll}!gexp__missing"))
        .or_else(|| wp.slots.iter().find(|(k, _)| k.ends_with("!gexp__missing")).map(|(_, v)| *v))
        .ok_or("Wine's PE module has no gexp__missing")?;
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

    // Data exports: the skeleton's export address table pointed at the
    // module's data (32-bit RVAs wrap to any address).
    let dir = pe.dirs.first().map(|d| d.0).unwrap_or(0);
    if dir != 0 && !data_list.is_empty() {
        let mut gm = GuestMem::new(crate::guest_bytes());
        let functions = gm.read_u32(base + dir as u64 + 28).map_err(|_| "export directory outside memory")? as u64;
        for (idx, link) in &data_list {
            let Some(off) = wp.data_exports.get(&format!("{dll}!{link}")) else {
                crate::warn_line(&format!("gex-nt: {path}: data export {link} is not in Wine's PE module\n"));
                continue;
            };
            let target = (wp.data + *off as u64).wrapping_sub(base) as u32;
            let _ = gm.write_u32(base + functions + 4 * *idx as u64, target);
        }
    }
    // winecrt0's base of this DLL: the skeleton's (after the module's data
    // initialisation: at its install for the first DLL, now for later ones)
    let first_mapping = wp.mapped.insert(dll.clone());
    if let (true, Some(off)) = (first_mapping, wp.data_exports.get(&format!("{dll}!__gexw_image_base"))) {
        let at = wp.data + *off as u64;
        let bytes = if wp.ptr64 { base.to_le_bytes().to_vec() } else { (base as u32).to_le_bytes().to_vec() };
        if first {
            wp.pending.push((at, bytes));
        } else if let Some(dst) = crate::guest_bytes().get_mut(at as usize..at as usize + bytes.len()) {
            dst.copy_from_slice(&bytes);
        }
    }
    // (W64's ntdll has Wine's own Unix side, which sets them: these are
    // gex-kernel's i386 trampolines)
    if first_mapping && dll == "ntdll.dll" && !wp.ptr64 {
        for (name, bytes) in ntdll_variables() {
            if let Some(off) = wp.data_exports.get(&format!("ntdll.dll!{name}")) {
                wp.pending.push((wp.data + *off as u64, bytes));
            }
        }
    }

    let addrs: Vec<u64> = pairs.iter().map(|&(a, _)| a).collect();
    crate::warn_line(&format!("gex-nt: native wasm32 module for {path}: {} entries\n", pairs.len()));
    if first {
        let len = gex_cgen::provider::slot_names(&wp.wasm).map_err(|e| e.to_string())?.len() as u32;
        wp.install = ftab(|m| m.publish(len, PROVIDER | wp.number, Tier::Unit, &pairs));
        clear(&addrs);
        let (regs, dirs, slots) = bridges(&addrs);
        lock(&PROVIDERS).push(ProviderModule { file: format!("wasmdll:{path}"), regs, dirs, slots, memory_base: wp.data as u32 });
        Ok(wp.number as i32)
    } else {
        let install = wp.install;
        drop(g);
        bind(install, &pairs);
        Ok(-2)
    }
}

/// The executable sections of every skeleton mapped: `ud2` stubs, never
/// x86 code -- Wine's PE module serves their exports through the table.
/// The function JIT reads no code there (`fjit::translate_root`): an
/// import thunk's `jmp [iat]`, its slot read-only once Wine's loader filled
/// it, resolves to a one-case jump table whose target is the stub, and the
/// thunk's translation then ran the `ud2` (imm32's call of user32's
/// `User32InitializeImmEntryTable`: SIGILL in every W3 page process with
/// `?fjit=1`).
static SKELETON_CODE: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

/// Unmapped ranges (`super::on_retire`'s `gone`): a skeleton mapped there
/// is gone, and so are its exports' bindings. Kept, they sent the code
/// that next landed at those addresses to Wine's PE module: W64's wineboot
/// maps DLLs as image views and unmaps them, and two pages of anonymous
/// code later at 0x7fc11000 called a native export until the rebridge
/// guard gave up.
pub(super) fn forget_gone(gone: &[(u64, u64)]) {
    if gone.is_empty() {
        return;
    }
    let Some(install) = lock(&WINE_PE).as_ref().map(|w| w.install) else { return };
    let bound: Vec<u64> = ftab(|m| {
        gone.iter().flat_map(|&(s, e)| m.range(s, e).filter(|(_, f)| f.install == install).map(|(a, _)| a).collect::<Vec<_>>()).collect()
    });
    let skeletons = lock(&SKELETON_CODE).clone();
    // Only where nothing is mapped now (Wine releases a view as a PROT_NONE
    // reservation): a live skeleton's page made writable for a while (a
    // hook's VirtualProtect) keeps its exports.
    let (dead, dead_code): (Vec<u64>, Vec<(u64, u64)>) = {
        let mm = crate::sigs::proc_home().mm().lock();
        let released = |a: u64| mm.vma_at(a).is_none_or(|v| v.prot == 0);
        (
            bound.into_iter().filter(|&a| released(a)).collect(),
            skeletons.into_iter().filter(|&(lo, hi)| gone.iter().any(|&(s, e)| s <= lo && hi <= e) && released(lo)).collect(),
        )
    };
    if !dead.is_empty() {
        let memory = crate::guest_bytes();
        ftab(|m| {
            for &a in &dead {
                m.retire(a);
                gex_kernel::ftab::fcache_clear(memory, a);
            }
        });
    }
    if !dead_code.is_empty() {
        lock(&SKELETON_CODE).retain(|r| !dead_code.contains(r));
    }
}

/// Whether `pc` is in a mapped skeleton's executable sections.
pub(crate) fn in_skeleton(pc: u64) -> bool {
    lock(&SKELETON_CODE).iter().any(|&(lo, hi)| lo <= pc && pc < hi)
}

pub(crate) fn skeleton_code() -> Vec<(u64, u64)> {
    lock(&SKELETON_CODE).clone()
}

/// From a PE file's first bytes: its size mapped (`SizeOfImage`) and
/// whether it is a native Wine DLL's skeleton (a `.wasmref` section, as
/// [`is_skeleton`]); `None` when the headers are not all there.
fn pe_head(b: &[u8]) -> Option<(u64, bool)> {
    let u16_at = |o: usize| b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
    let u32_at = |o: usize| b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]));
    if b.get(..2)? != b"MZ" {
        return None;
    }
    let pe = u32_at(0x3c)? as usize;
    if b.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let (sections, opt_size) = (u16_at(pe + 6)? as usize, u16_at(pe + 20)? as usize);
    let opt = pe + 24;
    // `SizeOfImage` is at 56 in both PE32 and PE32+ optional headers.
    let size = u32_at(opt + 56)? as u64;
    let mut skeleton = false;
    for i in 0..sections {
        let name = b.get(opt + opt_size + i * 40..opt + opt_size + i * 40 + 8)?;
        skeleton |= name.split(|&c| c == 0).next() == Some(b".wasmref".as_slice());
    }
    Some((size, skeleton))
}

/// `gex.image_mapped(base, fd)` (W3, docs/wine-native.md §3.17): Wine's
/// own PE loader mapped the image open at `fd` at `base`, relocated. A
/// native DLL's skeleton gets Wine's PE module bound at its exports, as the
/// native host's `image_mapped` does. The module number when this is the
/// first skeleton (the host instantiates it, then `gex_cgen_pmod_installed`
/// and `gex_wasmdll_installed`), -2 when bound into the install (bridges
/// to write), -3 for an i386 image with a function-tier unit to serve
/// (`super::w3_mapped`), -1 for anything else (an i386 image stays on
/// tier-1).
#[no_mangle]
pub extern "C" fn gex_image_mapped(base: u32, fd: i32) -> i32 {
    let base = base as u64;
    // The table needs no kernel lock (step 3), and the read is the file's.
    let Some((path, file)) = crate::fds_free().and_then(|fds| {
        let f = fds.get(fd).ok()?;
        Some((f.path.clone(), Arc::clone(&f.handle)))
    }) else {
        return -1;
    };
    // An x86 image (not a skeleton): left unread for its first miss, which
    // a resource-only map or an import scan never has. Its headers say
    // which.
    if super::w3_may_defer(&path, base) {
        let mut head = vec![0u8; 4096];
        let n = file.read_at(0, &mut head).unwrap_or(0) as usize;
        head.truncate(n);
        if let Some((size, false)) = pe_head(&head) {
            crate::note_w3_image(base, size, &path);
            super::w3_defer_unread(&path, base, size, file);
            return -1;
        }
    }
    let Some(pe) = super::read_pe(&*file) else { return -1 };
    crate::note_w3_image(base, pe.image_size as u64, &path);
    if !is_skeleton(&pe) {
        // An i386 image (the program's, a game's DLL): its function-tier
        // unit, when units are on; -3 tells JS to serve it.
        return if super::w3_mapped(&path, base, &pe) { -3 } else { -1 };
    }
    match publish(&path, base, &pe) {
        Ok(n) => {
            let mut r = lock(&SKELETON_CODE);
            for sec in pe.sections.iter().filter(|s| s.executable() && s.vsize.max(s.raw_size) > 0) {
                let lo = base + sec.rva as u64;
                r.push((lo, lo + sec.vsize.max(sec.raw_size) as u64));
            }
            n
        }
        Err(e) => {
            crate::warn_line(&format!("gex-w3: {path}: native wasm32 DLL: {e}\n"));
            -1
        }
    }
}

/// A PE32+ core's data, at the top of [`crate::W64_PE_DATA`] (reserved at
/// the x86-64 split), zeroed.
fn alloc_pe_data_window(size: u64, name: &str) -> Result<u64, String> {
    use gex_kernel::mem::{flags, prot};
    let (lo, hi) = crate::W64_PE_DATA;
    let len = (size + 0xffff) & !0xffff;
    if len > hi - lo {
        return Err(format!("{name}: {len:#x} bytes do not fit the W64 PE data window"));
    }
    let at = hi - len;
    let a = crate::sigs::proc_home()
        .mm()
        .lock()
        .mmap(at, len, prot::READ | prot::WRITE, flags::PRIVATE | flags::ANONYMOUS | flags::FIXED, name)
        .map_err(|e| format!("mapping {name} at {at:#x}: errno {}", e.0))?;
    if let Some(b) = crate::guest_bytes().get_mut(a as usize..(a + len) as usize) {
        b.fill(0);
    }
    Ok(a)
}

fn clear(addrs: &[u64]) {
    let mem = crate::guest_bytes();
    for &a in addrs {
        gex_kernel::ftab::fcache_clear(mem, a);
    }
}

/// Map `pairs` to slots of `install` and queue their bridges for every
/// Worker.
pub(super) fn bind(install: u32, pairs: &[(u64, u32)]) {
    if pairs.is_empty() {
        return;
    }
    ftab(|m| m.bind(install, pairs));
    let addrs: Vec<u64> = pairs.iter().map(|&(a, _)| a).collect();
    clear(&addrs);
    let (regs, dirs, slots) = bridges(&addrs);
    lock(&BINDS).push(Bind { regs, dirs, slots });
}

/// Provider module `number`'s bytes when it is Wine's PE module or a
/// wasm32 program (in the shim's heap), else 0.
#[no_mangle]
pub extern "C" fn gex_cgen_pmod_wasm_ptr(number: u32) -> u32 {
    if let Some((p, _)) = super::wasmprog::wasm(number).or_else(|| super::libprov::wasm(number)) {
        return p;
    }
    lock(&WINE_PE).as_ref().filter(|w| w.number == number).map_or(0, |w| w.wasm.as_ptr() as u32)
}

#[no_mangle]
pub extern "C" fn gex_cgen_pmod_wasm_len(number: u32) -> u32 {
    if let Some((_, n)) = super::wasmprog::wasm(number).or_else(|| super::libprov::wasm(number)) {
        return n;
    }
    lock(&WINE_PE).as_ref().filter(|w| w.number == number).map_or(0, |w| w.wasm.len() as u32)
}

/// Wine's PE module's content key when it is provider module `number`.
pub(super) fn wasm_key(number: u32) -> Option<(u64, u64)> {
    let mut g = lock(&WINE_PE);
    let w = g.as_mut().filter(|w| w.number == number)?;
    Some(*w.key.get_or_insert_with(|| super::wasmprog::file_key(&w.file, &w.wasm)))
}

/// The publishing Worker instantiated Wine's PE module (its data is
/// initialised and relocated): ntdll's boot variables, then the entry
/// thunks (once).
#[no_mangle]
pub extern "C" fn gex_wasmdll_installed() {
    let (install, data, entries, pending, w) = {
        let mut g = lock(&WINE_PE);
        let Some(wp) = g.as_mut() else { return };
        let pending = std::mem::take(&mut wp.pending);
        let first = !std::mem::replace(&mut wp.installed, true);
        let entries = first.then(|| wp.data_exports.get("__gexw_entries").copied()).flatten();
        let decl = wp.data_exports.get("__gexw_entries_decl").copied();
        (wp.install, wp.data, entries.map(|e| (e, decl)), pending, if wp.ptr64 { 8u64 } else { 4 })
    };
    let mem = crate::guest_bytes();
    for (at, bytes) in pending {
        if let Some(dst) = mem.get_mut(at as usize..at as usize + bytes.len()) {
            dst.copy_from_slice(&bytes);
        }
    }
    let Some((off, decl)) = entries else { return };
    // The ICALL pass's {function, thunk} pairs (table indices): each thunk
    // at its function's index as a guest address (as the native
    // `bind_entry_thunks`).
    let gm = GuestMem::new(crate::guest_bytes());
    let base = ftab(|m| m.install(install).base);
    let read = |off: u32| -> Vec<(u64, u32)> {
        let at = data + off as u64;
        // (an LLP64 module's pointers are 8 bytes, their values below 4 GiB)
        let (start, stop) = (gm.read_u32(at).unwrap_or(0) as u64, gm.read_u32(at + w).unwrap_or(0) as u64);
        let mut out = Vec::new();
        let mut a = start;
        while start != 0 && a + 2 * w <= stop {
            let (f, t) = (gm.read_u32(a).unwrap_or(0), gm.read_u32(a + w).unwrap_or(0));
            a += 2 * w;
            if f < base || t < base || f as u64 >= 0x3f0000 {
                continue;
            }
            out.push((f as u64, t - base));
        }
        out
    };
    let mut pairs = read(off);
    // Thunks made where a function is handed out but not defined: only for
    // functions no defining object gave one (it knows the convention).
    if let Some(off) = decl {
        let strong: std::collections::HashSet<u64> = pairs.iter().map(|&(f, _)| f).collect();
        let mut seen = std::collections::HashSet::new();
        for (f, t) in read(off) {
            if !strong.contains(&f) && seen.insert(f) {
                pairs.push((f, t));
            }
        }
    }
    bind(install, &pairs);
}

/// How many late bindings there are: a Worker writes each one's bridges.
#[no_mangle]
pub extern "C" fn gex_wasmdll_binds_len() -> u32 {
    lock(&BINDS).len() as u32
}

/// Binding `i`'s bridges, `[table, slot]` flattened, as `gex_cgen_regs`.
#[no_mangle]
pub extern "C" fn gex_wasmdll_bind_regs(i: u32, buf: u32, cap: u32) -> u32 {
    let b = lock(&BINDS);
    let Some(x) = b.get(i as usize) else { return 0 };
    let n = x.regs.len().min(cap as usize);
    put_u32s(&x.regs[..n], buf);
    x.regs.len() as u32
}

#[no_mangle]
pub extern "C" fn gex_wasmdll_bind_slots(i: u32) -> u32 {
    lock(&BINDS).get(i as usize).map_or(0, |x| x.slots.min(u32::MAX as u64) as u32)
}

/// A Worker wrote binding `i`'s bridges: the directory for the pages it
/// numbered (idempotent).
#[no_mangle]
pub extern "C" fn gex_wasmdll_bind_done(i: u32) {
    let dirs = lock(&BINDS).get(i as usize).map(|x| x.dirs.clone()).unwrap_or_default();
    write_dirs(dirs);
}

/// The skeleton's `.wasmref`: `GEXWREF1 module\0 dll\0 u32 count { u32
/// index, name\0 }` (winebuild `--wasm-shared`).
fn wasmref(mem: &[u8], base: u64, sec: &gex_ld::pe::Section) -> Result<(String, String, Vec<(u32, String)>), String> {
    let at = (base + sec.rva as u64) as usize;
    let raw = mem.get(at..at + sec.vsize as usize).ok_or(".wasmref outside memory")?;
    if raw.get(..8) != Some(&b"GEXWREF1"[..]) {
        return Err(".wasmref is not GEXWREF1".into());
    }
    let mut p = 8;
    let cstr = |p: &mut usize| -> Result<String, String> {
        let end = raw[*p..].iter().position(|&c| c == 0).ok_or(".wasmref truncated")? + *p;
        let s = String::from_utf8_lossy(&raw[*p..end]).into_owned();
        *p = end + 1;
        Ok(s)
    };
    let u32_at = |p: &mut usize| -> Result<u32, String> {
        let v = u32::from_le_bytes(raw.get(*p..*p + 4).ok_or(".wasmref truncated")?.try_into().unwrap());
        *p += 4;
        Ok(v)
    };
    let module = cstr(&mut p)?;
    let dll = cstr(&mut p)?;
    let count = u32_at(&mut p)?;
    let mut data = Vec::new();
    for _ in 0..count {
        let idx = u32_at(&mut p)?;
        data.push((idx, cstr(&mut p)?));
    }
    Ok((module, dll, data))
}

/// As natively (`wasmdll::ntdll_variables`): what gex-kernel's NT boot
/// fills in ntdll, in a native ntdll's own data.
fn ntdll_variables() -> [(&'static str, Vec<u8>); 3] {
    use gex_kernel::nt::boot::tramp;
    [
        ("__wine_syscall_dispatcher", (tramp::SYSCALL_DISPATCHER_32 as u32).to_le_bytes().to_vec()),
        ("__wine_unix_call_dispatcher", (tramp::UNIX_CALL_32 as u32).to_le_bytes().to_vec()),
        ("__wine_unixlib_handle", gex_kernel::nt::unixcall::NTDLL_HANDLE.to_le_bytes().to_vec()),
    ]
}

/// pelink's `gexw.forwards` custom section: (export name, `dll!name`).
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

/// Exported globals with constant initialisers: a PIC module's data
/// symbols, as offsets from `__memory_base` (i64 in a wasm64 module).
pub(super) fn exported_offsets(wasm: &[u8]) -> Result<HashMap<String, u32>, String> {
    let e = |e: wasmparser::BinaryReaderError| e.to_string();
    let mut imported_globals = 0u32;
    let mut inits: Vec<Option<u32>> = Vec::new();
    let mut out = HashMap::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        match payload.map_err(e)? {
            wasmparser::Payload::ImportSection(r) => {
                for imp in r {
                    if let wasmparser::TypeRef::Global(_) = imp.map_err(e)?.ty {
                        imported_globals += 1;
                    }
                }
            }
            wasmparser::Payload::GlobalSection(r) => {
                for g in r {
                    let g = g.map_err(e)?;
                    let mut ops = g.init_expr.get_operators_reader();
                    inits.push(match ops.read().map_err(e)? {
                        wasmparser::Operator::I32Const { value } => Some(value as u32),
                        wasmparser::Operator::I64Const { value } => u32::try_from(value).ok(),
                        _ => None,
                    });
                }
            }
            wasmparser::Payload::ExportSection(r) => {
                for x in r {
                    let x = x.map_err(e)?;
                    if x.kind == wasmparser::ExternalKind::Global && x.index >= imported_globals {
                        if let Some(Some(v)) = inits.get((x.index - imported_globals) as usize) {
                            out.insert(x.name.to_string(), *v);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// `gex.wine_pe_slot(name)` (W3): the function-table index of a named
/// function of Wine's PE module (win32u.so starts explorer's desktop thread
/// by it), or 0 when the module is not installed or has no such function.
#[no_mangle]
pub extern "C" fn gex_wine_pe_slot(ptr: u32, len: u32) -> u32 {
    let mem = crate::guest_bytes();
    let Some(b) = mem.get(ptr as usize..(ptr as usize).saturating_add(len as usize)) else { return 0 };
    let name = String::from_utf8_lossy(b).into_owned();
    let (install, k) = {
        let g = lock(&WINE_PE);
        let Some(wp) = g.as_ref() else { return 0 };
        let Some(&k) = wp.slots.get(&name) else { return 0 };
        (wp.install, k)
    };
    ftab(|m| m.install(install).base) + k
}

/// `gex.code_changed(addr, len)` (W3: Wine's NtFlushInstructionCache): the
/// translations of `[addr, addr + len)` are retired, as natively.
#[no_mangle]
pub extern "C" fn gex_code_changed(addr: u32, len: u32) {
    let (addr, len) = (addr as u64, len as u64);
    let (start, end) = (addr & !0xfff, (addr + len + 0xfff) & !0xfff);
    if end > start {
        crate::sigs::proc_home().mm().lock().log.retired_code.push((start, end));
    }
    gex_kernel::note_code_change();
}
