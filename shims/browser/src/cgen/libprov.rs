//! Stateful Linux providers in the page (docs/unity-dotnet.md P5 B1): the
//! native host's `libprov` for a Linux process with a 64-bit memory. A shared
//! object named in the page's `wasmdlls` (`soname=guest path of a wasm64 PIC
//! module`, Unity's `libmonobdwgc-2.0.so=/gex/mono.wasm`) is replaced when it
//! maps: every export it defines is bound to the module's `gexp_<name>` (or
//! `gexp__missing`), its constructors and destructors to `gexp__noop`, and none
//! of its own code runs. The module is installed as Wine's PE module is
//! (`wasmdll.rs`): `PROVIDER | number` in the function table, its data from
//! low guest memory, its bytes in the shim's heap for every Worker to
//! instantiate (`gex.mjs` `#cgenInstall`, with the wasm64 `provider_*` imports).
//!
//! A library nothing names is replaced all the same when the guest's
//! filesystem has a provider installed for its soname, under
//! `/usr/lib/gex/<arch>/<soname>.wasm` (`libz.so.1.wasm` for an arm64
//! program's `libz.so.1`); `noauto` in the list turns that off.
//!
//! The protocol with `gex.mjs` (`#cgenProviders`):
//!   - a unit's provider `i` with `gex_cgen_provider_libprov(u, i)` set is
//!     published by `gex_libprov_publish(u, i)`, which returns its provider
//!     module number; JS catches up (instantiating it) and calls
//!     `gex_cgen_pmod_installed`.
//!   - `gex_cgen_pmod_libprov(number)` says a module is one of these (its
//!     imports are the 64-bit `provider_*` set).

use super::*;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;

/// `soname=path` pairs from the page (`gex_libprov_config`).
static DLLS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// One replaced library, mapped and waiting for (or past) its publish.
struct LinuxProv {
    /// The library's path, for messages.
    path: String,
    /// The provider module.
    wasm: Vec<u8>,
    /// (guest address, slot) pairs.
    pairs: Vec<(u64, u32)>,
    /// Its data (`__memory_base`), below 4 GiB.
    data: u64,
    /// Its provider module number once published.
    number: Option<u32>,
    /// Its function-table install, once published.
    install: u32,
    /// Whether its entry thunks are bound (`gex_libprov_installed`).
    bound: bool,
    /// Its code page (0 without one), where its table base is kept for a
    /// fork's child ([`BASE_AT`]).
    code: u64,
    /// In a fork's child: the parent's table base, which the copied data's
    /// function pointers (zlib's `zalloc = zcalloc`, sqlite's methods) hold.
    want_base: Option<u32>,
}

/// Where in a provider's code page its table base is kept (`"GEXB"`, then
/// the base): the code page is the child's copy, as its data is. The first
/// 0x100 bytes are never a bound address that runs (the thread entry is at
/// offset 0 only).
const BASE_AT: u64 = 8;
const BASE_MAGIC: u32 = u32::from_le_bytes(*b"GEXB");

static LIBS: Mutex<Vec<LinuxProv>> = Mutex::new(Vec::new());

/// The guest address bound to the module's `__gex_thread_start` (threads it
/// creates start there; its code page: return addresses and reverse slots).
static THREAD_ENTRY: AtomicU64 = AtomicU64::new(0);

/// The replaced libraries' text: the function JIT leaves it alone (it would
/// otherwise follow a PLT into code that must never run).
static REPLACED: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

/// Exports bound to `gexp__missing`, by guest address: for the message.
static MISSING: Mutex<Option<HashMap<u64, String>>> = Mutex::new(None);

const CODE_PAGE: u64 = 0x10000;
const REVERSE_FIRST: u64 = 0x1000;
const REVERSE_STRIDE: u64 = 16;

/// `soname=path,...`: the libraries this process replaces (the page's
/// `?wasmdlls=`), before the program starts.
#[no_mangle]
pub extern "C" fn gex_libprov_config(ptr: u32, len: u32) {
    // In the shim's heap (`gex_alloc`), above a wasm32 shim's guest slice.
    // SAFETY: JS wrote `len` bytes at `ptr`, a live `gex_alloc` allocation.
    let spec = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr as usize as *const u8, len as usize) }).into_owned();
    let mut v = lock(&DLLS);
    for item in spec.split(',') {
        if item.trim() == "noauto" {
            AUTO.store(false, Ordering::Relaxed);
        } else if let Some((so, path)) = item.split_once('=') {
            let pair = (so.trim().to_string(), path.trim().to_string());
            if !v.contains(&pair) {
                v.push(pair);
            }
        }
    }
}

/// Whether a library nothing names is replaced by the provider installed
/// for its soname (`noauto` in the list turns it off).
static AUTO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Where the guest's filesystem holds providers: `<dir>/<arch>/<soname>.wasm`
/// (`/usr/lib/gex/aarch64/libz.so.1.wasm`), the arch the ELF machine's.
pub(crate) const PROVIDER_DIR: &str = "/usr/lib/gex";

/// Sonames already looked for under [`PROVIDER_DIR`], and what was found.
static AUTO_SEEN: Mutex<Vec<(String, Option<String>)>> = Mutex::new(Vec::new());

/// The provider installed for the library mapped from `path` (file name
/// `file`), if there is one: an ELF shared object whose machine has a
/// directory under [`PROVIDER_DIR`] holding `<file>.wasm`.
fn installed(path: &str, file: &str) -> Option<String> {
    if !AUTO.load(Ordering::Relaxed) || !file.contains(".so") {
        return None;
    }
    let key = format!("{path}\0{file}");
    if let Some((_, r)) = lock(&AUTO_SEEN).iter().find(|(k, _)| *k == key) {
        return r.clone();
    }
    let found = (|| {
        let h = crate::sigs::proc_home();
        let mut ehdr = [0u8; 20];
        if h.read_file_at(path, 0, &mut ehdr) != Some(20) || ehdr[..4] != *b"\x7fELF" {
            return None;
        }
        let arch = match u16::from_le_bytes([ehdr[18], ehdr[19]]) {
            183 => "aarch64",
            62 => "x86_64",
            3 => "i386",
            _ => return None,
        };
        let wasm = format!("{PROVIDER_DIR}/{arch}/{file}.wasm");
        let mut magic = [0u8; 4];
        (h.read_file_at(&wasm, 0, &mut magic) == Some(4) && magic == *b"\0asm").then_some(wasm)
    })();
    lock(&AUTO_SEEN).push((key, found.clone()));
    found
}

/// Whether `addr` is in a replaced library's text.
pub(crate) fn replaced(addr: u64) -> bool {
    lock(&REPLACED).iter().any(|&(lo, hi)| addr >= lo && addr < hi)
}

/// The addresses replaced libraries' providers are bound at.
pub(crate) fn bound_addrs() -> Vec<u64> {
    lock(&LIBS).iter().flat_map(|l| l.pairs.iter().map(|p| p.0)).collect()
}

/// The ranges of replaced libraries' own text.
pub(crate) fn replaced_ranges() -> Vec<(u64, u64)> {
    lock(&REPLACED).clone()
}

/// Whether `addr` is one a replaced library's provider is bound at (its
/// pairs are sorted by address).
pub(crate) fn bound(addr: u64) -> bool {
    lock(&LIBS).iter().any(|l| l.pairs.binary_search_by_key(&addr, |&(a, _)| a).is_ok())
}

/// A dispatch miss in a replaced library's own text that no binding
/// explains (said once per address, on the debug console). Expected ones are
/// the object's own startup and teardown, which ld.so still runs: `.init`,
/// its PLT stubs, crtbegin's `frame_dummy`/`__do_global_dtors_aux` and
/// `.fini` -- libz.so.1 at +0x2340/+0x2360/+0x272c/+0x2780/+0x14f3c in every
/// node child_process spawns. They touch only the library's own data, so
/// tier-1 running them is right; as console errors they failed every page
/// run that spawned a node.
pub(crate) fn note_stray(pc: u64) {
    static SEEN: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    let mut seen = lock(&SEEN);
    if seen.len() < 64 && !seen.contains(&pc) {
        seen.push(pc);
        crate::trace_line(&format!("gex: the guest reached {pc:#x}, the replaced library's own code (not a bound export): tier-1 runs it\n"));
    }
}

/// Guest pages below 4 GiB: a provider's data and code page (its data base
/// and the guest addresses it binds are 32-bit numbers to the page).
fn alloc_low(size: u64, prot: u32, name: &str) -> Result<u64, String> {
    use gex_kernel::mem::flags;
    let len = (size + 0xfff) & !0xfff;
    crate::sigs::proc_home().mm().lock().mmap_low(0, len, prot, flags::PRIVATE | flags::ANONYMOUS, name).map_err(|e| format!("mapping {name}: errno {}", e.0))
}

/// A forked child's copy of the mapping [`alloc_low`] made in its parent
/// (by name): the provider's data, heap and code page are the child's as
/// the rest of its memory is.
fn inherited(name: &str) -> Option<u64> {
    crate::sigs::proc_home().mm().lock().vmas().iter().find(|v| v.name == name).map(|v| v.start)
}

/// The process the state above is for: a forked child's shim starts with
/// none of it (or its parent's copy), while its memory holds the parent's
/// replaced libraries, their providers' data and heap, and the guest's
/// pointers into them.
static PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Before a Worker's guest first runs (`gex_elfprov_wanted`): in a new
/// process, the replaced libraries it inherited are bound again, over the
/// data and code page its parent made (a child must not start a provider
/// over: an open `z_stream` points into its heap). Returns a unit to serve.
pub(crate) fn rescan_if_new() -> Option<usize> {
    // Not for a vfork child running in place (see `elfprov::rescan_if_new`):
    // its memory, providers' data and heap are its parent's, still in use.
    if crate::vfork::active() {
        return None;
    }
    let pid = crate::my_pid();
    let was = PID.swap(pid, Ordering::AcqRel);
    if was == pid || (lock(&DLLS).is_empty() && !AUTO.load(Ordering::Relaxed)) {
        return None;
    }
    lock(&LIBS).clear();
    lock(&REPLACED).clear();
    THREAD_ENTRY.store(0, Ordering::Release);
    let mapped = crate::sigs::proc_home().code_maps();
    maps(&mapped)
}

/// After a syscall that mapped code: a replaced library becomes a unit with
/// one provider for JS to serve (`elfprov`'s queue). Returns whether one was
/// queued.
pub(crate) fn maps(mapped: &[gex_kernel::CodeMap]) -> Option<usize> {
    let dlls = lock(&DLLS).clone();
    if dlls.is_empty() && !AUTO.load(Ordering::Relaxed) {
        return None;
    }
    if PID.load(Ordering::Acquire) == 0 {
        PID.store(crate::my_pid(), Ordering::Release);
    }
    let mut queued = None;
    for m in mapped {
        let file = m.path.rsplit('/').next().unwrap_or(&m.path);
        let named = dlls.iter().find(|(so, _)| so == file).map(|(_, w)| w.clone());
        let Some(wasm_path) = named.or_else(|| installed(&m.path, file)) else { continue };
        match prepare(m, &wasm_path) {
            Ok(Some(u)) => queued = Some(u),
            Ok(None) => {}
            Err(e) => {
                crate::warn_line(&format!("gex: replacing {}: {e}\n", m.path));
            }
        }
    }
    queued
}

fn prepare(m: &gex_kernel::CodeMap, wasm_path: &str) -> Result<Option<usize>, String> {
    let bytes = crate::sigs::proc_home().read_file(&m.path).ok_or("cannot read the library")?;
    let image = gex_ld::elf::load(&bytes, 0).map_err(|e| format!("{e:?}"))?;
    let text = image.segments.iter().find(|s| s.executable).ok_or("no executable segment")?;
    let bias = m.start.wrapping_sub(text.vaddr & !0xfff);
    if lock(&LIBS).iter().any(|l| l.path == m.path) {
        return Ok(None);
    }
    let (lo, hi) = (text.vaddr, text.vaddr + text.bytes.len() as u64);
    lock(&REPLACED).push((lo.wrapping_add(bias), hi.wrapping_add(bias)));
    let wasm = crate::sigs::proc_home().read_file(wasm_path).ok_or_else(|| format!("cannot read its provider module {wasm_path}"))?;
    let slots = gex_cgen::provider::slot_names(&wasm).map_err(|e| e.to_string())?;
    let slot = |n: &str| slots.iter().position(|s| s.as_deref() == Some(n)).map(|k| k as u32);
    let missing = slot("gexp__missing").ok_or("no gexp__missing in its module's table")?;
    let mut provided: HashMap<&str, u32> = HashMap::new();
    for (k, s) in slots.iter().enumerate() {
        if let Some(e) = s.as_deref().and_then(|s| s.strip_prefix("gexp_")).filter(|e| !e.starts_with('_') && !e.starts_with("ptr_")) {
            provided.insert(e, k as u32);
        }
    }
    let mut pairs = Vec::new();
    let mut unprovided = Vec::new();
    for y in image.dynsyms.iter().filter(|y| !y.undefined && !y.tls && y.value >= lo && y.value < hi) {
        let addr = y.value.wrapping_add(bias);
        match provided.get(y.name.as_str()) {
            Some(&k) => pairs.push((addr, k)),
            None => {
                pairs.push((addr, missing));
                unprovided.push((addr, y.name.clone()));
            }
        }
    }
    let mut code = 0;
    let mut want_base = None;
    if let Some(k) = slot("__gex_thread_start") {
        let name = format!("[provider {wasm_path} code]");
        let at = match inherited(&name) {
            Some(a) => {
                let gm = GuestMem::new(crate::guest_bytes());
                if gm.read_u32(a + BASE_AT).ok() == Some(BASE_MAGIC) {
                    want_base = gm.read_u32(a + BASE_AT + 4).ok();
                }
                a
            }
            None => alloc_low(CODE_PAGE, gex_kernel::mem::prot::READ, &name)?,
        };
        code = at;
        pairs.push((at, k));
        THREAD_ENTRY.store(at, Ordering::Release);
        if let Some(r) = slot("gexp__reverse") {
            for a in (at + REVERSE_FIRST..at + CODE_PAGE).step_by(REVERSE_STRIDE as usize) {
                pairs.push((a, r));
            }
        }
    }
    if let Some(k) = slot("gexp__noop") {
        let mut ctors: Vec<u64> = image.init_array.clone();
        ctors.extend(elf_fini(&bytes));
        for a in ctors.into_iter().filter(|&v| v >= lo && v < hi).map(|v| v.wrapping_add(bias)) {
            pairs.retain(|&(x, _)| x != a);
            pairs.push((a, k));
        }
    }
    pairs.sort_by_key(|&(a, k)| (a, k == missing));
    pairs.dedup_by_key(|&mut (a, _)| a);
    lock(&MISSING).get_or_insert_with(Default::default).extend(unprovided);
    let (size, align) = dylink_memory(&wasm)?;
    let a = 1u64 << align;
    let name = format!("[provider {wasm_path} data]");
    let data = match inherited(&name) {
        Some(d) => d,
        None => alloc_low((size as u64).max(16) + a, gex_kernel::mem::prot::READ | gex_kernel::mem::prot::WRITE, &name)?,
    };
    let data = (data + a - 1) & !(a - 1);
    crate::trace_line(&format!("gex: {} replaced by {wasm_path}: {} addresses bound, data at {data:#x}\n", m.path, pairs.len()));
    let index = {
        let mut libs = lock(&LIBS);
        libs.push(LinuxProv { path: m.path.clone(), wasm, pairs, data, number: None, install: 0, bound: false, code, want_base });
        libs.len() - 1
    };
    let u = {
        let mut units = lock(&UNITS);
        units.push(Unit {
            key: String::new(),
            path: m.path.clone(),
            base: m.start,
            wine: true,
            pages: Vec::new(),
            regs: Vec::new(),
            dirs: Vec::new(),
            slots: 0,
            providers: vec![Provided { file: format!("libprov:{index}"), binds: Vec::new(), stateful: None, data: false, skeleton: None, lifted: false }],
            identity: String::new(),
            key_base: String::new(),
        });
        units.len() - 1
    };
    Ok(Some(u))
}

/// Whether provider `i` of unit `u` is a replaced Linux library's.
#[no_mangle]
pub extern "C" fn gex_cgen_provider_libprov(u: u32, i: u32) -> i32 {
    lock(&UNITS).get(u as usize).and_then(|x| x.providers.get(i as usize)).is_some_and(|p| p.file.starts_with("libprov:")) as i32
}

/// Publish provider `i` of unit `u` (a replaced library): its provider
/// module number, or -1 (said).
#[no_mangle]
pub extern "C" fn gex_libprov_publish(u: u32, i: u32) -> i32 {
    let index = {
        let units = lock(&UNITS);
        let Some(p) = units.get(u as usize).and_then(|x| x.providers.get(i as usize)) else { return -1 };
        let Some(Ok(index)) = p.file.strip_prefix("libprov:").map(|s| s.parse::<usize>()) else { return -1 };
        index
    };
    let mut libs = lock(&LIBS);
    let Some(l) = libs.get_mut(index) else { return -1 };
    if let Some(n) = l.number {
        return n as i32;
    }
    let len = match gex_cgen::provider::slot_names(&l.wasm) {
        Ok(s) => s.len() as u32,
        Err(e) => {
            crate::warn_line(&format!("gex: {}: {e}\n", l.path));
            return -1;
        }
    };
    let number = lock(&PROVIDERS).len() as u32;
    l.install = ftab(|t| match l.want_base {
        Some(base) => {
            let (n, ok) = t.publish_at(base, len, PROVIDER | number, Tier::Unit, &l.pairs);
            if !ok {
                crate::warn_line(&format!("gex: {}: fork child: table base {base} is taken, its function pointers will be wrong\n", l.path));
            }
            n
        }
        None => t.publish(len, PROVIDER | number, Tier::Unit, &l.pairs),
    });
    if l.code != 0 {
        let base = ftab(|t| t.install(l.install).base);
        let mut gm = GuestMem::new(crate::guest_bytes());
        let _ = gm.write_u32(l.code + BASE_AT, BASE_MAGIC);
        let _ = gm.write_u32(l.code + BASE_AT + 4, base);
    }
    let addrs: Vec<u64> = l.pairs.iter().map(|&(a, _)| a).collect();
    let mem = crate::guest_bytes();
    for &a in &addrs {
        gex_kernel::ftab::fcache_clear(mem, a);
    }
    let (regs, dirs, slots) = bridges(&addrs);
    lock(&PROVIDERS).push(ProviderModule { file: format!("libprov:{}", l.path), regs, dirs, slots, memory_base: l.data as u32 as u64 });
    l.number = Some(number);
    number as i32
}

/// The publishing Worker instantiated provider module `number` (its data is
/// relocated): when it is a replaced library's, bind the entry thunks of its
/// GexA64Icall pass (item3/a64icall): `__gexa_entries` is the bounds of its
/// {function, thunk} pairs (i64 table indices), and each thunk is bound at
/// its function's index as a guest address, so the guest's call through a
/// function pointer the library gave it (a vfs method) runs the function.
/// Once a process.
#[no_mangle]
pub extern "C" fn gex_libprov_installed(number: u32) {
    let (install, data, off, path) = {
        let mut libs = lock(&LIBS);
        let Some(l) = libs.iter_mut().find(|l| l.number == Some(number)) else { return };
        if std::mem::replace(&mut l.bound, true) {
            return;
        }
        let Ok(exports) = super::wasmdll::exported_offsets(&l.wasm) else { return };
        let Some(&off) = exports.get("__gexa_entries") else { return };
        (l.install, l.data, off, l.path.clone())
    };
    let gm = GuestMem::new(crate::guest_bytes());
    let at = data + off as u64;
    let (start, stop) = (gm.read_u64(at).unwrap_or(0), gm.read_u64(at + 8).unwrap_or(0));
    if start == 0 || stop <= start {
        return;
    }
    let base = ftab(|m| m.install(install).base) as u64;
    let code = THREAD_ENTRY.load(Ordering::Relaxed);
    let mut pairs = Vec::new();
    let mut a = start;
    while a + 16 <= stop {
        let (f, t) = (gm.read_u64(a).unwrap_or(0), gm.read_u64(a + 8).unwrap_or(0));
        a += 16;
        // (an index the code page's addresses also are would be ambiguous)
        if f < base || t < base || f >= 0x3f0000 || (code != 0 && f >= code && f < code + CODE_PAGE) {
            continue;
        }
        pairs.push((f, (t - base) as u32));
    }
    crate::trace_line(&format!("gex: {path}: {} entry thunks bound\n", pairs.len()));
    super::wasmdll::bind(install, &pairs);
}

/// The module bytes of provider module `number` when it is a replaced
/// Linux library's, or a JIT reservation's placeholder (`gex_cgen_pmod_wasm_ptr`/`len`).
pub(crate) fn wasm(number: u32) -> Option<(u32, u32)> {
    if let Some(r) = lock(&LIBS).iter().find(|l| l.number == Some(number)).map(|l| (l.wasm.as_ptr() as usize as u32, l.wasm.len() as u32)) {
        return Some(r);
    }
    lock(&RESERVES).iter().find(|(n, _)| *n == number).map(|(_, w)| (w.as_ptr() as usize as u32, w.len() as u32))
}

/// The JIT's reserved table ranges: (provider module number, placeholder module).
static RESERVES: Mutex<Vec<(u32, Vec<u8>)>> = Mutex::new(Vec::new());

/// `gex.provider_jit_reserve(n)` (docs/unity-dotnet.md P5 B2, the native
/// `wasmjit`): a range of the process's function table for the provider's
/// JIT, every Worker's filled with the JIT-call placeholder as it catches up.
/// Returns the range's first index.
#[no_mangle]
pub extern "C" fn gex_libprov_jit_reserve(n: u64) -> u64 {
    let wasm = placeholder_module(n as u32);
    let number = lock(&PROVIDERS).len() as u32;
    let install = ftab(|t| t.publish(n as u32, PROVIDER | number, Tier::Unit, &[]));
    lock(&PROVIDERS).push(ProviderModule { file: "wasmjit:reserve".into(), regs: Vec::new(), dirs: Vec::new(), slots: 0, memory_base: 0 });
    lock(&RESERVES).push((number, wasm));
    ftab(|t| t.install(install).base) as u64
}

/// A module of `n` copies of the JIT-call placeholder (the browser's
/// `mono_jiterp_placeholder_jit_call`, `(ret_sp, sp, ftndesc, thrown)`, which
/// stores 999 through `thrown`: "not here yet"), filling the function table
/// from `__table_base`, as the native host makes it.
fn placeholder_module(n: u32) -> Vec<u8> {
    use wasm_encoder::*;
    let mut m = wasm_encoder::Module::new();
    let mut types = TypeSection::new();
    types.ty().function([ValType::I64; 4], []);
    m.section(&types);
    let mut imports = ImportSection::new();
    imports.import(
        gex_abi::ftab::PIC_MODULE,
        "memory",
        MemoryType { minimum: 0, maximum: Some(gex_abi::LIMIT64_PAGES), memory64: true, shared: true, page_size_log2: None },
    );
    imports.import(
        gex_abi::ftab::PIC_MODULE,
        gex_abi::ftab::PIC_TABLE,
        TableType { element_type: RefType::FUNCREF, table64: true, minimum: 0, maximum: None, shared: false },
    );
    imports.import(gex_abi::ftab::PIC_MODULE, gex_abi::ftab::PIC_TABLE_BASE, GlobalType { val_type: ValType::I64, mutable: false, shared: false });
    m.section(&imports);
    let mut funcs = FunctionSection::new();
    funcs.function(0);
    m.section(&funcs);
    let mut elems = ElementSection::new();
    let idx: Vec<u32> = vec![0; n as usize];
    elems.active(Some(0), &ConstExpr::global_get(0), Elements::Functions(idx.into()));
    m.section(&elems);
    let mut code = CodeSection::new();
    let mut f = Function::new([]);
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Const(999));
    f.instruction(&Instruction::I32Store(MemArg { offset: 0, align: 2, memory_index: 0 }));
    f.instruction(&Instruction::End);
    code.function(&f);
    m.section(&code);
    m.finish()
}

/// Whether provider module `number` is a replaced Linux library's: its
/// imports are the 64-bit `provider_*` set.
#[no_mangle]
pub extern "C" fn gex_cgen_pmod_libprov(number: u32) -> i32 {
    lock(&LIBS).iter().any(|l| l.number == Some(number)) as i32
}

/// Whether `addr` is one of the return addresses a provider's guest calls push
/// (its code page, `[entry + 0x100, entry + 0x1000)`).
pub(crate) fn provider_return(addr: u64) -> bool {
    let at = THREAD_ENTRY.load(Ordering::Relaxed);
    at != 0 && addr >= at + 0x100 && addr < at + REVERSE_FIRST
}

/// `gex.provider_thread_entry()` / `provider_code_page()`.
#[no_mangle]
pub extern "C" fn gex_libprov_thread_entry() -> u64 {
    THREAD_ENTRY.load(Ordering::Acquire)
}

/// `gex.provider_missing(addr)`: the program called an export the provider
/// does not implement. Ends the process, saying which.
#[no_mangle]
pub extern "C" fn gex_libprov_missing(addr: u64) {
    let name = lock(&MISSING).as_ref().and_then(|m| m.get(&addr).cloned()).unwrap_or_else(|| "?".into());
    panic!("the program called {name} ({addr:#x}), an export of a replaced library that its provider does not implement");
}

/// `gex.provider_table_size(base)`: the slots of the install at table
/// index `base`.
#[no_mangle]
pub extern "C" fn gex_libprov_table_size(base: u64) -> u64 {
    ftab(|t| t.installs().iter().find(|i| i.base as u64 == base).map_or(0, |i| i.len as u64))
}

/// The dynamic symbols of a mapped object, by path: (name -> value, its
/// executable segment's page-aligned vaddr).
type ObjSyms = (HashMap<String, u64>, u64);
static OBJ_SYMS: Mutex<Option<HashMap<String, Arc<ObjSyms>>>> = Mutex::new(None);

/// `gex.provider_guest_sym(name, len)`: the guest address of `name` in the
/// first executable mapping that defines it (RTLD_DEFAULT's order), or 0.
#[no_mangle]
pub extern "C" fn gex_libprov_guest_sym(ptr: u64, len: u64) -> u64 {
    let name = String::from_utf8_lossy(crate::guest_bytes().get(ptr as usize..(ptr + len) as usize).unwrap_or(&[])).into_owned();
    let maps: Vec<(u64, String)> = {
        crate::sigs::proc_home().mm().lock().vmas().iter().filter(|v| v.prot & gex_kernel::mem::prot::EXEC != 0).filter_map(|v| v.code.as_ref().map(|(p, _)| (v.start, p.clone()))).collect()
    };
    let mut seen = std::collections::HashSet::new();
    for (start, path) in maps {
        if !seen.insert(path.clone()) {
            continue;
        }
        let cached = lock(&OBJ_SYMS).get_or_insert_with(Default::default).get(&path).cloned();
        let syms = match cached {
            Some(s) => s,
            None => {
                let Some(bytes) = crate::sigs::proc_home().read_file(&path) else { continue };
                let Ok(image) = gex_ld::elf::load(&bytes, 0) else { continue };
                let Some(text) = image.segments.iter().find(|s| s.executable) else { continue };
                let map = image.dynsyms.iter().filter(|y| !y.undefined && !y.tls && y.value != 0).map(|y| (y.name.clone(), y.value)).collect();
                let s = Arc::new((map, text.vaddr & !0xfff));
                lock(&OBJ_SYMS).get_or_insert_with(Default::default).insert(path.clone(), Arc::clone(&s));
                s
            }
        };
        if let Some(v) = syms.0.get(&name) {
            return start.wrapping_sub(syms.1).wrapping_add(*v);
        }
    }
    0
}

/// `gex.provider_guest_stack_end(sp)`: the end of the mapping holding `sp`.
#[no_mangle]
pub extern "C" fn gex_libprov_guest_stack_end(sp: u64) -> u64 {
    crate::sigs::proc_home().mm().lock().vmas().iter().find(|v| v.start <= sp && sp < v.end).map_or(0, |v| v.end)
}

/// `gex.provider_abi(which)`: constants of gex's ABI a provider calling
/// guest code needs.
#[no_mangle]
pub extern "C" fn gex_libprov_abi(which: u32) -> u64 {
    match which {
        0 => gex_abi::FCACHE,
        1 => gex_abi::FCACHE_BITS as u64,
        2 => gex_abi::ctx::V as u64,
        _ => u64::MAX,
    }
}

/// DT_FINI and FINI_ARRAY's entries (unbiased), as the native host reads them.
fn elf_fini(bytes: &[u8]) -> Vec<u64> {
    let u64at = |o: usize| bytes.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
    let u32at = |o: usize| bytes.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let u16at = |o: usize| bytes.get(o..o + 2).map(|b| u16::from_le_bytes(b.try_into().unwrap()));
    let mut out = Vec::new();
    let (Some(phoff), Some(phentsize), Some(phnum)) = (u64at(0x20), u16at(0x36), u16at(0x38)) else { return out };
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
            13 => out.push(val),
            26 => fini_array = Some(val),
            28 => fini_arraysz = val,
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

// ---------------------------------------------------------------------------
// Stopping the guest's threads for the provider's collector (native
// `libprov::GuestStop`): each thread registers the provider's "in guest
// code" word for it and the provider's stop flag; threads running guest code
// stop at their next safepoint or syscall while a collection runs, a thread
// inside a syscall counts as stopped. Every Worker shares these statics (the
// shim's memory is the process's).
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StopState {
    threads: HashMap<u32, u64>,
    stop_word: u64,
    parked: std::collections::HashSet<u32>,
    in_kernel: std::collections::HashSet<u32>,
    stopper: Option<u32>,
}

static STOPPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Some thread registered (the syscall path's fast test).
static ANY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether any thread of this process registered with a provider.
pub(crate) fn any_registered() -> bool {
    ANY.load(Ordering::Relaxed)
}
static STOP: Mutex<Option<StopState>> = Mutex::new(None);
/// Bumped on every change a waiter could be waiting for (park, start).
static STOP_EPOCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn stop_state<R>(f: impl FnOnce(&mut StopState) -> R) -> R {
    f(lock(&STOP).get_or_insert_with(Default::default))
}

fn word(addr: u64) -> u32 {
    let mem = crate::guest_bytes();
    mem.get(addr as usize..addr as usize + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
}

fn store_word(addr: u64, v: u32) {
    std::sync::atomic::fence(Ordering::SeqCst);
    if let Some(b) = crate::guest_bytes().get_mut(addr as usize..addr as usize + 4) {
        b.copy_from_slice(&v.to_le_bytes());
    }
    std::sync::atomic::fence(Ordering::SeqCst);
}

fn bump() {
    STOP_EPOCH.fetch_add(1, Ordering::SeqCst);
    #[cfg(feature = "shared-memory")]
    unsafe {
        crate::warch::memory_atomic_notify(STOP_EPOCH.as_ptr() as *mut i32, u32::MAX);
    }
}

/// Wait (at most `ns`) for the next bump after `seen`.
fn wait_epoch(seen: u32, ns: i64) {
    #[cfg(feature = "shared-memory")]
    unsafe {
        crate::warch::memory_atomic_wait32(STOP_EPOCH.as_ptr() as *mut i32, seen as i32, ns);
    }
    #[cfg(not(feature = "shared-memory"))]
    let _ = (seen, ns);
}

fn this_tid() -> u32 {
    crate::my_tid() as u32
}

/// Whether a collection is stopping the guest's threads.
pub(crate) fn stop_active() -> bool {
    STOPPING.load(Ordering::Acquire)
}

/// At a safepoint or a syscall boundary in guest code (or from the
/// provider, `forced`): park while a collection runs.
pub(crate) fn park_if_stopping(tid: u32, forced: bool) {
    if !STOPPING.load(Ordering::Acquire) {
        return;
    }
    let parks = stop_state(|st| {
        let Some(&w) = st.threads.get(&tid) else { return false };
        if st.stopper == Some(tid) || (!forced && word(w) == 0) {
            return false;
        }
        st.parked.insert(tid);
        true
    });
    if !parks {
        return;
    }
    bump();
    while STOPPING.load(Ordering::Acquire) {
        let seen = STOP_EPOCH.load(Ordering::SeqCst);
        if !STOPPING.load(Ordering::Acquire) {
            break;
        }
        wait_epoch(seen, 1_000_000);
    }
    stop_state(|st| st.parked.remove(&tid));
}

/// A syscall made from guest code: counts as stopped until it returns.
pub(crate) fn syscall_enter(tid: u32) {
    if lock(&STOP).as_ref().is_some_and(|st| st.threads.contains_key(&tid)) {
        stop_state(|st| st.in_kernel.insert(tid));
        bump();
    }
}

/// Back to guest code (a syscall returned, or a safepoint): park if a
/// collection is on.
pub(crate) fn guest_again(tid: u32) {
    let was = lock(&STOP).as_mut().is_some_and(|st| st.in_kernel.remove(&tid));
    if was || STOPPING.load(Ordering::Acquire) {
        park_if_stopping(tid, false);
    }
}

/// `gex.provider_thread_register(in_guest, stop)`.
#[no_mangle]
pub extern "C" fn gex_libprov_thread_register(in_guest: u64, stop: u64) {
    let tid = this_tid();
    ANY.store(true, Ordering::Relaxed);
    stop_state(|st| {
        st.threads.insert(tid, in_guest);
        st.stop_word = stop;
    });
}

/// `gex.provider_park()`.
thread_local! {
    /// The provider's guest calls open on this Worker (`gexp_call_guest`):
    /// while any is, the guest code running has the provider's wasm frames
    /// below it, whichever tier runs it (a function JIT or unit function is
    /// called straight from the provider, with no call-out of the fallback's).
    static GUEST_CALLS: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// `?fjitlibs=provcheck=1`: [`gex_libprov_guest_call`] checks that each
/// provider guest call ends with the guest's stack just past the return
/// address the call pushed (its SP when the call opens), and says when not.
pub(crate) static PROV_CHECK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

thread_local! {
    /// The open guest calls' return-address slots, for [`PROV_CHECK`].
    static PROV_SLOTS: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// `gex.provider_guest_call(delta)`.
#[no_mangle]
pub extern "C" fn gex_libprov_guest_call(delta: i32) {
    GUEST_CALLS.with(|c| c.set(c.get() + delta));
    if PROV_CHECK.load(Ordering::Relaxed) {
        prov_check(delta);
    }
}

fn prov_check(delta: i32) {
    let ctx = crate::my_ctx();
    let gm = crate::GuestMem::new(crate::guest_bytes());
    let reg = |o: usize| gm.read_u64(ctx + o as u64).unwrap_or(0);
    let sp = reg(gex_abi::ctx::x(4));
    if delta > 0 {
        PROV_SLOTS.with(|s| s.borrow_mut().push(sp));
        return;
    }
    let Some(slot) = PROV_SLOTS.with(|s| s.borrow_mut().pop()) else { return };
    if sp == slot + 8 {
        return;
    }
    static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if SAID.fetch_add(1, Ordering::Relaxed) >= 32 {
        return;
    }
    crate::warn_line(&format!(
        "[gex] provcheck: a provider guest call ended with sp {sp:#x}, but its return address was at {slot:#x} ([slot] now {:#x}); \
         pc {:#x}, last target {:#x}, {} more open; last call-outs {:x?}\n",
        gm.read_u64(slot).unwrap_or(0),
        reg(gex_abi::ctx::PC),
        reg(gex_abi::ctx::LAST_TARGET),
        PROV_SLOTS.with(|s| s.borrow().len()),
        crate::cgen::callouts_recent().iter().rev().take(6).collect::<Vec<_>>(),
    ));
}

/// The count, which `provider_try` restores when a raise unwinds guest calls.
#[no_mangle]
pub extern "C" fn gex_libprov_guest_calls() -> i32 {
    GUEST_CALLS.with(|c| c.get())
}

#[no_mangle]
pub extern "C" fn gex_libprov_guest_calls_set(v: i32) {
    GUEST_CALLS.with(|c| c.set(v));
    PROV_SLOTS.with(|s| s.borrow_mut().truncate(v.max(0) as usize));
}

/// Whether a provider's guest call is open on this Worker.
pub(crate) fn guest_call_open() -> bool {
    GUEST_CALLS.with(|c| c.get()) > 0
}

#[no_mangle]
pub extern "C" fn gex_libprov_park() {
    park_if_stopping(this_tid(), true);
}

/// `gex.provider_stop_guest()`: stop every other registered thread that is
/// in guest code (10 s at most, then on without the stragglers, said).
#[no_mangle]
pub extern "C" fn gex_libprov_stop() {
    let me = this_tid();
    let stop_word = stop_state(|st| {
        st.stopper = Some(me);
        st.stop_word
    });
    if stop_word != 0 {
        store_word(stop_word, 1);
    }
    STOPPING.store(true, Ordering::SeqCst);
    let deadline = crate::now_ns() + 10_000_000_000;
    loop {
        let seen = STOP_EPOCH.load(Ordering::SeqCst);
        let waiting: Vec<u32> = stop_state(|st| {
            st.threads
                .iter()
                .filter(|(&t, &w)| t != me && word(w) != 0 && !st.parked.contains(&t) && !st.in_kernel.contains(&t))
                .map(|(&t, _)| t)
                .collect()
        });
        if waiting.is_empty() {
            return;
        }
        if crate::now_ns() > deadline {
            crate::warn_line(&format!("gex: provider stop-the-world: threads {waiting:?} never reached a safepoint; going on without them\n"));
            return;
        }
        wait_epoch(seen, 2_000_000);
    }
}

/// `gex.provider_start_guest()`.
#[no_mangle]
pub extern "C" fn gex_libprov_start() {
    let stop_word = stop_state(|st| {
        st.stopper = None;
        st.stop_word
    });
    if stop_word != 0 {
        store_word(stop_word, 0);
    }
    STOPPING.store(false, Ordering::SeqCst);
    bump();
}

// ---------------------------------------------------------------------------
// Function-tier units for Linux shared objects in the page (P5 B4; the native
// host's `pe_units::on_elf_maps`): an object named in `?elfunits=` gets the
// unit built natively for it at the bias it maps at (`gexc cgen-image --elf
// SO --base HEX --mem64`), found in the unit cache by the same key.
// ---------------------------------------------------------------------------

/// The objects that get units (file names, or `all`).
static ELF_UNITS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Objects placed already in this process: (path, bias, the span of their
/// executable segments).
static ELF_PLACED: Mutex<Vec<(String, u64, (u64, u64))>> = Mutex::new(Vec::new());

/// `execve`: every replaced library of the old program is gone, and its
/// addresses may hold the new program's code (see `cgen::on_exec`). Returns
/// the provider module numbers that went with them (JIT reservations too).
pub(crate) fn on_exec(old_code: &[(u64, u64)]) -> Vec<u32> {
    let mut dead: Vec<u32> = lock(&LIBS).iter().filter_map(|l| l.number).collect();
    dead.extend(lock(&RESERVES).iter().map(|(n, _)| *n));
    lock(&LIBS).clear();
    lock(&REPLACED).clear();
    THREAD_ENTRY.store(0, Ordering::Release);
    if let Some(m) = lock(&MISSING).as_mut() {
        m.clear();
    }
    lock(&RESERVES).clear();
    forget_gone(old_code);
    dead
}

/// Code no longer mapped executable (`cgen::on_retire`'s `gone`): an object
/// whose code was there is placed again if it is mapped again (ALSA's
/// dlopen/dlclose cycles reload libasound at the same base), since its unit
/// was retired with the pages.
pub(crate) fn forget_gone(gone: &[(u64, u64)]) {
    lock(&ELF_PLACED).retain(|(_, _, (lo, hi))| !gone.iter().any(|&(s, e)| s < *hi && *lo < e));
}

/// `export,export,...` (the page's `?provskip=`): exports every provider
/// leaves to the guest's own code, as `GEX_PROVIDER_SKIP` natively (A/B and
/// debugging runs; the units must be built with the same skip). Before the
/// program starts.
#[no_mangle]
pub extern "C" fn gex_provider_skip(ptr: u32, len: u32) {
    // SAFETY: JS wrote `len` bytes at `ptr`, a live `gex_alloc` allocation.
    let spec = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr as usize as *const u8, len as usize) }).into_owned();
    gex_cgen::provider::set_skip(spec.trim());
}

/// Provider sources from outside gex (the page's `providerSources`, natively
/// `GEX_PROVIDER_DIRS`): entries `name\ntext` separated by NUL. Before the
/// program starts, so every unit key sees them.
#[no_mangle]
pub extern "C" fn gex_provider_sources(ptr: u32, len: u32) {
    // SAFETY: JS wrote `len` bytes at `ptr`, a live `gex_alloc` allocation.
    let spec = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr as usize as *const u8, len as usize) }).into_owned();
    let v = spec
        .split('\0')
        .filter_map(|e| e.split_once('\n'))
        .filter(|(n, _)| !n.trim().is_empty())
        .map(|(n, t)| (n.trim().to_string(), t.to_string()))
        .collect();
    gex_cgen::provider::add_sources(v);
}

/// `name,name,...` (the page's `?elfunits=`), before the program starts.
#[no_mangle]
pub extern "C" fn gex_cgen_elf_units(ptr: u32, len: u32) {
    // In the shim's heap (`gex_alloc`), above a wasm32 shim's guest slice.
    // SAFETY: JS wrote `len` bytes at `ptr`, a live `gex_alloc` allocation.
    let spec = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr as usize as *const u8, len as usize) }).into_owned();
    let mut v = lock(&ELF_UNITS);
    for n in spec.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if !v.iter().any(|x| x == n) {
            v.push(n.to_string());
        }
    }
}

/// The load bias of an ELF object one of whose executable `PT_LOAD`s was
/// mapped at `start` from file offset `offset`.
fn elf_bias(bytes: &[u8], start: u64, offset: u64) -> Option<u64> {
    let rd = |o: usize, n: usize| -> Option<u64> {
        let b = bytes.get(o..o + n)?;
        let mut v = [0u8; 8];
        v[..n].copy_from_slice(b);
        Some(u64::from_le_bytes(v))
    };
    if bytes.get(0..4)? != b"\x7fELF" || *bytes.get(4)? != 2 {
        return None;
    }
    let (phoff, phentsize, phnum) = (rd(0x20, 8)? as usize, rd(0x36, 2)? as usize, rd(0x38, 2)? as usize);
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        let (ty, flags, off, vaddr) = (rd(ph, 4)?, rd(ph + 4, 4)?, rd(ph + 8, 8)?, rd(ph + 16, 8)?);
        if ty == 1 && flags & 1 != 0 && off & !0xfff == offset {
            return Some(start.wrapping_sub(vaddr & !0xfff));
        }
    }
    None
}

/// After a syscall that mapped code: want the unit of each listed object.
/// Returns whether one is now wanted.
pub(crate) fn elf_unit_maps(mapped: &[gex_kernel::CodeMap]) -> bool {
    let list = lock(&ELF_UNITS).clone();
    if list.is_empty() || !super::on() {
        return false;
    }
    let mut any = false;
    for m in mapped {
        let file = m.path.rsplit('/').next().unwrap_or(&m.path);
        if !list.iter().any(|n| n == "all" || n == file) {
            continue;
        }
        let Some(bytes) = crate::sigs::proc_home().read_file(&m.path) else { continue };
        let Some(base) = elf_bias(&bytes, m.start, m.offset) else { continue };
        if lock(&ELF_PLACED).iter().any(|(p, b, _)| *p == m.path && *b == base) {
            continue;
        }
        let mut image = match gex_ld::elf::load(&bytes, base) {
            Ok(i) => i,
            Err(e) => {
                crate::warn_line(&format!("gex-cgen: {}: {e:?}; no unit\n", m.path));
                lock(&ELF_PLACED).push((m.path.clone(), base, (0, 0)));
                continue;
            }
        };
        let span = image.segments.iter().filter(|s| s.executable).fold((u64::MAX, 0u64), |(lo, hi), s| {
            (lo.min(s.vaddr), hi.max(s.vaddr + s.bytes.len() as u64))
        });
        lock(&ELF_PLACED).push((m.path.clone(), base, span));
        gex_ld::elf::apply_own_relative(&mut image);
        crate::warn_line(&format!("gex-cgen: {} at {base:#x}: wanting its unit\n", m.path));
        super::place(&image, &m.path, base, true, None);
        any = true;
    }
    any
}
