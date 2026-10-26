//! Shared libraries for native wasm32 programs in the browser (the
//! native host's `dylink.rs`, and the shim-independent half in the
//! `gex-dylink` crate).
//!
//! A program whose module names libraries (`dylink.0` NEEDED) is published
//! with them: each library's data is mapped like `ld.so` maps it, every
//! module (program included) becomes a wasm32-program provider module
//! (`PROGS`, `PROVIDERS`) with a binding list JS reads
//! (`gex_wasmprog_binds`), published in initialisation order (each after
//! what it needs, the program last), so every Worker instantiates them in
//! that order as it catches up with the function table's log. gex enters
//! threads at libc's `__gex_start` / `__gex_thread_start`; libc learns of
//! the libraries through its `__gex_dsos` registry, filled here.
//!
//! A binding list is one line per import, tab-separated:
//!   `S mod name`            the Worker's own (memory, table, bases, stack
//!                           pointer, asyncify globals, `gex.*`);
//!   `G mod name value mut`  a global (`GOT.mem`, `GOT.func`, a missing one: 0);
//!   `E mod name module`     provider module `module`'s export `name`;
//!   `L mod name slot`       a function through table slot `slot` (a module
//!                           instantiated after this one);
//!   `M mod name message`    a function that throws `message`;
//! then `X slot export` per table slot past the element segment to fill
//! with an export after instantiation, and, for a `dlopen`ed module with
//! thread-local data, `T size align`: each Worker gives it a block of its
//! own as it instantiates it (`__wasm_init_tls`), libc's registry having
//! laid out only the blocks of the modules the program started with.
//!
//! A fork's child gets the link map as text (`gex_wasmprog_link`, put back
//! with `gex_wasmprog_adopt_link`): it reads the same files and publishes
//! the same modules at the same table bases over the parent's data.

use super::wasmprog::{entry_bridges, exe_path, Prog, CURRENT_BASE, PROGS};
use super::*;
use gex_dylink::{Bind, Info, Kind, Link, Loaded, HANDLE_BASE};
use std::collections::{HashMap, HashSet};

struct DyProc {
    link: Link,
    /// The provider module of each link-map entry.
    module_of: Vec<u32>,
    ldpath: Vec<String>,
    origin: String,
    /// How many of the modules the program started with: libc's registry
    /// gives those their thread-local blocks, gex the ones `dlopen` added.
    startup: usize,
}

static DYPROC: Mutex<Option<DyProc>> = Mutex::new(None);
/// Binding lists, by provider module number.
static BINDS: Mutex<Option<HashMap<u32, String>>> = Mutex::new(None);
/// The modules the last `dlopen` loaded, in initialisation order, for JS
/// to run their constructors once it has instantiated them.
static DL_NEW: Mutex<Vec<u32>> = Mutex::new(Vec::new());
/// Bumped by every `dlopen` once its modules are published: a Worker that
/// has not seen the latest number catches up after its next system call
/// (`gex_wasmprog_dl_gen`), so a function pointer another thread got from
/// `dlsym` and handed over is in its table by the time it can call it.
static DL_GEN: AtomicU32 = AtomicU32::new(0);
/// A fork child's link map (`gex_wasmprog_adopt_link`).
static ADOPT: Mutex<Option<String>> = Mutex::new(None);
/// libc's registry to fill once the modules are instantiated (their data
/// initialised): the init order and libc's index.
static REGISTRY: Mutex<Option<(Vec<usize>, usize)>> = Mutex::new(None);

/// A library's bytes as loaded: guarded when [`gex_set_dl_checked`] is on.
fn read(path: &str) -> Option<Vec<u8>> {
    crate::sigs::proc_home().read_file(path).map(checked)
}

/// `GEX_DL_CHECKED_CALLS` (the page's `dlchecked=1`): every wasm32 module
/// of a program linked against shared libraries gets its indirect calls
/// guarded (`gex_dylink::checked_calls`), so a function pointer from a
/// `dlopen` that reached a Worker with no system call between is caught up
/// with before the call; JS gives the guard `gex.dl_stale` and
/// `gex.dl_size`. Off, nothing is rewritten and nothing costs.
static CHECKED: AtomicBool = AtomicBool::new(false);

#[no_mangle]
pub extern "C" fn gex_set_dl_checked(on: i32) {
    CHECKED.store(on != 0, Ordering::Relaxed);
}

fn checked(b: Vec<u8>) -> Vec<u8> {
    if !CHECKED.load(Ordering::Relaxed) || !b.starts_with(b"\0asm") {
        return b;
    }
    match gex_dylink::checked_calls(&b) {
        Ok(out) => out,
        Err(e) => {
            crate::warn_line(&format!("gex: GEX_DL_CHECKED_CALLS: {e:#}; that module is left unguarded\n"));
            b
        }
    }
}

fn find_needed(p: &mut DyProc, from: usize) -> Result<Vec<Vec<u8>>, String> {
    let (o, lp) = (p.origin.clone(), p.ldpath.clone());
    let libs = p
        .link
        .needed_of(from, |name, rp| gex_dylink::search(name, rp, &o, &lp, read))
        .map_err(|e| format!("{e:#}"))?;
    let mut out = Vec::new();
    for (name, path, b, info) in libs {
        p.link.mods.push(Loaded { name, path, info, memory_base: 0, table_base: 0 });
        out.push(b);
    }
    Ok(out)
}

/// Publish `p.link.mods[from..]` (whose bytes are `bytes`) in
/// initialisation order: map data where it has none, give table ranges
/// where `keep` is false, bind. `entries` are gex's entries, bound in
/// libc's table range. Returns the order and libc's provider module.
fn publish(p: &mut DyProc, from: usize, bytes: Vec<Vec<u8>>, relocate: bool, keep: bool, entries: Option<(u64, u64)>) -> Result<(Vec<usize>, Option<u32>), String> {
    for m in p.link.mods[from..].iter_mut() {
        if m.memory_base != 0 {
            continue;
        }
        let align = 1u64 << m.info.mem_align.min(16);
        let size = (m.info.mem_size as u64).max(1) + if align > 0x1000 { align } else { 0 };
        let at = linux_alloc(size, &m.path)?;
        let base = (at + align - 1) & !(align - 1);
        if base > u32::MAX as u64 {
            return Err(format!("{}: data mapped above 4 GiB", m.path));
        }
        m.memory_base = base as u32;
    }
    let order = p.link.init_order(from);
    if !keep {
        let mut next = ftab(|m| m.size());
        for &i in &order {
            let (len, _) = p.link.mods[i].info.slots();
            p.link.mods[i].table_base = next;
            next += len.max(1);
        }
    }
    let libc = entries.and_then(|_| p.link.lookup("__gex_start", Kind::Func));
    if entries.is_some() && libc.is_none() {
        return Err(format!("{}: no library defines __gex_start (libc.so)", p.link.mods[0].path));
    }
    p.module_of.resize(p.link.mods.len(), u32::MAX);
    let mut bytes: Vec<Option<Vec<u8>>> = bytes.into_iter().map(Some).collect();
    let mut done: HashSet<usize> = (0..from).collect();
    let mut libc_number = None;
    for &i in &order {
        let binds = p.link.bind(i, &done).map_err(|e| format!("{e:#}"))?;
        done.insert(i);
        let m = &p.link.mods[i];
        let (len, at) = m.info.slots();
        let pairs = match (entries, libc) {
            (Some((entry, thread)), Some(l)) if l == i => {
                let slot = |n: &str| at[&m.info.exports[n].index];
                vec![(entry, slot("__gex_start")), (thread, slot("__gex_thread_start"))]
            }
            _ => Vec::new(),
        };
        let (regs, dirs, slots) = if pairs.is_empty() {
            (Vec::new(), Vec::new(), 0)
        } else {
            entry_bridges(&pairs.iter().map(|&(a, _)| a).collect::<Vec<_>>())
        };
        let mut text = String::new();
        for (imp, b) in m.info.imports.iter().zip(binds) {
            let (mo, na) = (&imp.module, &imp.name);
            let line = match b {
                Bind::Shim => format!("S\t{mo}\t{na}"),
                Bind::GotMem(v) | Bind::GotFunc(v) => format!("G\t{mo}\t{na}\t{v}\t{}", imp.ty),
                Bind::Export { module, name } => format!("E\t{mo}\t{na}\t{}\t{name}", p.module_of[module]),
                Bind::Late { slot } => format!("L\t{mo}\t{na}\t{slot}"),
                Bind::Missing(_) if imp.kind == Kind::Global => format!("G\t{mo}\t{na}\t0\t{}", imp.ty),
                Bind::Missing(msg) => {
                    let msg = if msg.is_empty() { format!("{}: undefined weak symbol {na} called", m.path) } else { msg };
                    format!("M\t{mo}\t{na}\tsymbol lookup error: {}", msg.replace(['\t', '\n'], " "))
                }
            };
            text.push_str(&line);
            text.push('\n');
        }
        for (s, n) in m.info.extra_slots() {
            text.push_str(&format!("X\t{}\t{n}\n", m.table_base + s));
        }
        if let Some((size, align)) = dl_tls(m, i >= p.startup)? {
            text.push_str(&format!("T\t{size}\t{align}\n"));
        }
        let number = {
            let mut provs = lock(&PROVIDERS);
            provs.push(ProviderModule { file: format!("wasmprog:{}", m.path), regs, dirs, slots, memory_base: m.memory_base as u64 });
            provs.len() as u32 - 1
        };
        p.module_of[i] = number;
        lock(&BINDS).get_or_insert_with(HashMap::new).insert(number, text);
        lock(&PROGS).push(Prog { number, wasm: bytes[i - from].take().unwrap_or_default(), relocate, key: None, path: m.path.clone(), lowered: false });
        let (_, ok) = ftab(|f| f.publish_at(m.table_base, len.max(1), PROVIDER | number, Tier::Unit, &pairs));
        if !ok {
            return Err(format!("{}: table range {}+{} is taken", m.path, m.table_base, len));
        }
        if Some(i) == libc {
            libc_number = Some(number);
            CURRENT_BASE.store(m.table_base, Ordering::Relaxed);
        }
    }
    if std::env::var_os("GEX_WASM_TRACE").is_some() || crate::tracing() {
        for &i in &order {
            let m = &p.link.mods[i];
            crate::warn_line(&format!("gex: wasm32 module {}: data {:#x}+{:#x}, table {}\n", m.path, m.memory_base, m.info.mem_size, m.table_base));
        }
    }
    Ok((order, libc_number))
}

/// A module's thread-local block when `dlopen` loaded it (`dlopened`), as
/// the native host's `dylink::dl_tls`.
fn dl_tls(m: &Loaded, dlopened: bool) -> Result<Option<(u32, u32)>, String> {
    let size = m.info.value("__tls_size");
    if !dlopened || size == 0 {
        return Ok(None);
    }
    for f in ["__wasm_init_tls", "__gex_set_tls_base"] {
        if m.info.exports.get(f).is_none_or(|e| e.kind != Kind::Func) {
            return Err(format!("{}: has thread-local data but does not export {f} (link it with a newer gexcc)", m.path));
        }
    }
    Ok(Some((size, m.info.value("__tls_align").max(1))))
}

/// A fresh program that names libraries: load them, publish every module,
/// fill libc's registry. Returns libc's provider module.
pub(super) fn install(wasm: Vec<u8>, info: Info, mem_base: u32, entry: u64, thread_entry: u64) -> Result<u32, String> {
    let (wasm, info) = if CHECKED.load(Ordering::Relaxed) {
        let w = checked(wasm);
        let i = Info::parse(&w).map_err(|e| format!("{e:#}"))?;
        (w, i)
    } else {
        (wasm, info)
    };
    let path = exe_path();
    // The environment as published at the load or exec (no kernel lock).
    let env = crate::sigs::proc_home().envp();
    let ldpath = env
        .iter()
        .find_map(|e| e.strip_prefix("LD_LIBRARY_PATH="))
        .map(|v| v.split(':').filter(|s| !s.is_empty()).map(str::to_string).collect())
        .unwrap_or_default();
    let origin = path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_else(|| ".".into());
    let mut p = DyProc {
        link: Link { mods: vec![Loaded { name: path.clone(), path: path.clone(), info, memory_base: mem_base, table_base: 0 }] },
        module_of: Vec::new(),
        ldpath,
        origin,
        startup: usize::MAX,
    };
    let mut bytes = vec![wasm];
    bytes.extend(find_needed(&mut p, 0)?);
    p.startup = p.link.mods.len();
    let (order, libc) = publish(&mut p, 0, bytes, true, false, Some((entry, thread_entry)))?;
    let libc = libc.ok_or("no libc")?;
    let li = p.module_of.iter().position(|&n| n == libc).unwrap();
    *lock(&REGISTRY) = Some((order, li));
    *lock(&DYPROC) = Some(p);
    Ok(libc)
}

/// A static program (no libraries): its exported functions past its
/// element segment get table slots (`dlsym` of a program linked
/// `-rdynamic`, GtkBuilder's signal handlers), and `dlsym` sees them.
/// Returns how many slots it takes.
pub(super) fn static_program(info: Option<&Info>, elem_len: u32) -> u32 {
    match info {
        Some(info) => info.slots().0.max(elem_len),
        None => elem_len,
    }
}

/// After [`static_program`]'s module is published as `number` at `base`.
pub(super) fn static_published(number: u32, info: Option<Info>, path: &str, base: u32, mem_base: u32) {
    let Some(info) = info else { return };
    let mut text = String::new();
    // undefined weak symbols: null, as ELF leaves them (a strong one stays
    // an import nothing gives, and instantiating says so)
    for (mo, na, b) in info.static_weak_binds().unwrap_or_default() {
        text.push_str(&match b {
            Bind::GotMem(v) | Bind::GotFunc(v) => format!("G\t{mo}\t{na}\t{v}\t1\n"),
            _ => format!("M\t{mo}\t{na}\tsymbol lookup error: {path}: undefined weak symbol {na} called\n"),
        });
    }
    for (s, n) in info.extra_slots() {
        text.push_str(&format!("X\t{}\t{n}\n", base + s));
    }
    lock(&BINDS).get_or_insert_with(HashMap::new).insert(number, text);
    *lock(&DYPROC) = Some(DyProc {
        link: Link { mods: vec![Loaded { name: path.to_string(), path: path.to_string(), info, memory_base: mem_base, table_base: base }] },
        module_of: vec![number],
        ldpath: Vec::new(),
        origin: String::new(),
        startup: 1,
    });
}

/// A fork's child: the parent's link map (`ADOPT`) again, at the same
/// table bases, over the parent's data.
pub(super) fn adopt(wasm: Vec<u8>, entry: u64, thread_entry: u64) -> Result<u32, String> {
    let text = lock(&ADOPT).take().ok_or("a fork's child of a program with shared libraries, without its link map")?;
    let _ = exe_path();
    let mut lines = text.lines();
    let origin = lines.next().and_then(|l| l.strip_prefix("origin\t")).ok_or("link map: no origin")?.to_string();
    let ldpath = lines
        .next()
        .and_then(|l| l.strip_prefix("ldpath\t"))
        .ok_or("link map: no ldpath")?
        .split(':')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let startup = lines
        .next()
        .and_then(|l| l.strip_prefix("startup\t"))
        .and_then(|n| n.parse().ok())
        .ok_or("link map: no startup count")?;
    let mut p = DyProc { link: Link::default(), module_of: Vec::new(), ldpath, origin, startup };
    let mut bytes = Vec::new();
    for (k, l) in lines.enumerate() {
        let f: Vec<&str> = l.splitn(4, '\t').collect();
        let [mb, tb, name, path] = f[..] else { return Err(format!("link map: bad line {l:?}")) };
        let b = if k == 0 { checked(wasm.clone()) } else { read(path).ok_or_else(|| format!("{path}: gone since the fork"))? };
        let info = Info::parse(&b).map_err(|e| format!("{path}: {e:#}"))?;
        let num = |s: &str| s.parse::<u32>().map_err(|_| format!("link map: bad number {s:?}"));
        p.link.mods.push(Loaded { name: name.to_string(), path: path.to_string(), info, memory_base: num(mb)?, table_base: num(tb)? });
        bytes.push(b);
    }
    let (_, libc) = publish(&mut p, 0, bytes, false, true, Some((entry, thread_entry)))?;
    *lock(&DYPROC) = Some(p);
    libc.ok_or_else(|| "no libc".into())
}

/// The program's modules are instantiated (JS, after catching up): fill
/// libc's registry if a fresh program is waiting for it. Returns 0, or -1
/// (the process ends) when that fails.
#[no_mangle]
pub extern "C" fn gex_wasmprog_ready() -> i32 {
    let Some((order, libc)) = lock(&REGISTRY).take() else { return 0 };
    let r = lock(&DYPROC).as_ref().map(|p| p.link.write_registry(crate::guest_bytes(), &order, libc));
    if let Some(Err(e)) = r {
        crate::warn_line(&format!("gex: wasm32 program: {e:#}\n"));
        crate::exit_process(127);
        return -1;
    }
    0
}

/// Provider module `number`'s binding list into `buf`; its length (0: a
/// module without one, a static program).
#[no_mangle]
pub extern "C" fn gex_wasmprog_binds(number: u32, buf: u32, cap: u32) -> u32 {
    lock(&BINDS).as_ref().and_then(|b| b.get(&number)).map_or(0, |t| put_str(t, buf, cap))
}

/// The process's link map as text, for a fork's child; 0 when the program
/// has no shared libraries.
#[no_mangle]
pub extern "C" fn gex_wasmprog_link(buf: u32, cap: u32) -> u32 {
    let g = lock(&DYPROC);
    let Some(p) = g.as_ref().filter(|p| !p.link.mods[0].info.needed.is_empty()) else { return 0 };
    let mut t = format!("origin\t{}\nldpath\t{}\nstartup\t{}\n", p.origin, p.ldpath.join(":"), p.startup);
    for m in &p.link.mods {
        t.push_str(&format!("{}\t{}\t{}\t{}\n", m.memory_base, m.table_base, m.name, m.path));
    }
    put_str(&t, buf, cap)
}

/// A fork's child: its parent's link map ([`gex_wasmprog_link`]).
#[no_mangle]
pub extern "C" fn gex_wasmprog_adopt_link(ptr: u32, len: u32) {
    let b = crate::shim_slice_mut(ptr, len).to_vec();
    *lock(&ADOPT) = Some(String::from_utf8_lossy(&b).into_owned());
}

fn guest_str(ptr: u64) -> Option<String> {
    let s = crate::guest_bytes().get(ptr as usize..)?;
    let n = s.iter().position(|&c| c == 0)?;
    Some(String::from_utf8_lossy(&s[..n]).into_owned())
}

fn dlopen(path: &str) -> Result<u32, String> {
    let mut g = lock(&DYPROC);
    let Some(p) = g.as_mut() else {
        return Err(format!("{path}: dlopen in a static program (link it with GEXCC_SHARED=1)"));
    };
    if let Some(i) = p.link.loaded(path) {
        return Ok(HANDLE_BASE + i as u32 * 16);
    }
    let (o, lp) = (p.origin.clone(), p.ldpath.clone());
    let (found, b) = gex_dylink::search(path, &[], &o, &lp, read).map_err(|e| format!("{e:#}"))?;
    let info = Info::parse(&b).map_err(|_| format!("{found}: not a wasm shared library"))?;
    if !info.dylink {
        return Err(format!("{found}: not a wasm shared library"));
    }
    // on failure the link map is as it was
    let mut q = DyProc { link: p.link.clone(), module_of: p.module_of.clone(), ldpath: lp, origin: o, startup: p.startup };
    let from = q.link.mods.len();
    q.link.mods.push(Loaded { name: path.to_string(), path: found, info, memory_base: 0, table_base: 0 });
    let mut all = vec![b];
    all.extend(find_needed(&mut q, from)?);
    let (order, _) = publish(&mut q, from, all, true, false, None)?;
    DL_GEN.fetch_add(1, Ordering::Release);
    *lock(&DL_NEW) = order.iter().map(|&i| q.module_of[i]).collect();
    *p = q;
    Ok(HANDLE_BASE + from as u32 * 16)
}

/// `gex.dlopen(path, err, errlen)` (libgex's dl.c), the shim's half: load a
/// library and what it needs; its handle, or 0 with the message in `err`.
/// JS then catches up (instantiating them) and runs their constructors
/// ([`gex_wasmprog_dl_new`]). Addresses are 64-bit for a memory64
/// program's (libgex built with GEX_MEMORY64); JS widens a 32-bit one's.
#[no_mangle]
pub extern "C" fn gex_wasmprog_dlopen(path: u64, err: u64, errlen: u32) -> u32 {
    lock(&DL_NEW).clear();
    let r = guest_str(path).ok_or_else(|| "dlopen: bad path".to_string()).and_then(|p| dlopen(&p));
    match r {
        Ok(h) => h,
        Err(msg) => {
            let n = msg.len().min(errlen.saturating_sub(1) as usize);
            if let Some(d) = crate::guest_bytes().get_mut(err as usize..err as usize + n + 1) {
                d[..n].copy_from_slice(&msg.as_bytes()[..n]);
                d[n] = 0;
            }
            0
        }
    }
}

/// The `k`th module the last dlopen loaded, in initialisation order, or -1.
#[no_mangle]
pub extern "C" fn gex_wasmprog_dl_new(k: u32) -> i32 {
    lock(&DL_NEW).get(k as usize).map_or(-1, |&n| n as i32)
}

/// `gex.dlsym(handle, name)`: a function's table index or a datum's
/// address, 0 when not found (handle 0: the global scope).
#[no_mangle]
pub extern "C" fn gex_wasmprog_dlsym(handle: u32, name: u64) -> u32 {
    let Some(name) = guest_str(name) else { return 0 };
    lock(&DYPROC).as_ref().map_or(0, |p| p.link.dlsym(handle, &name))
}

/// Guest pages for a Worker's thread-local blocks of `dlopen`ed modules
/// (JS bump-allocates them); 0 when mapping fails.
#[no_mangle]
pub extern "C" fn gex_wasmprog_tls_chunk(len: u32) -> u32 {
    match linux_alloc(len as u64, "[dlopen tls]") {
        Ok(a) if a + len as u64 <= u32::MAX as u64 => a as u32,
        Ok(_) | Err(_) => 0,
    }
}

/// A Worker's thread ended (not its process): its chunks go.
#[no_mangle]
pub extern "C" fn gex_wasmprog_tls_free(at: u32, len: u32) {
    let _ = crate::sigs::proc_home().mm().lock().unmap(at as u64, (len as u64 + 0xfff) & !0xfff);
}

/// Provider module `number`'s place in the link map (the same in a fork's
/// child, whose module numbers are its own), or -1.
#[no_mangle]
pub extern "C" fn gex_wasmprog_link_index(number: u32) -> i32 {
    lock(&DYPROC).as_ref().and_then(|p| p.module_of.iter().position(|&n| n == number)).map_or(-1, |i| i as i32)
}

/// [`DL_GEN`], for a Worker to compare after a system call.
#[no_mangle]
pub extern "C" fn gex_wasmprog_dl_gen() -> u32 {
    DL_GEN.load(Ordering::Acquire)
}
