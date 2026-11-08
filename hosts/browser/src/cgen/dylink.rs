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
//!   `G mod name #value mut [64]`  a global (`GOT.mem`, `GOT.func`, a missing
//!                           one: 0), `i64` with the `64` column (a wasm64 module);
//!   `E mod name #module export at`  provider module `module`'s export
//!                           `export`; `at` is `s` and its table slot (`#slot`,
//!                           a function), `g` and its value (a constant
//!                           global) or `-`, for a module whose exports the
//!                           page packed;
//!   `L mod name #slot [sig]` a function through table slot `slot` (a module
//!                           instantiated after this one), of type `sig`
//!                           (`Info::sigs`) when it has one the page can write;
//!   `M mod name message`    a function that throws `message`;
//! then `X #slot export` per table slot past the element segment to fill
//! with an export after instantiation (or, packed, by an element segment
//! the page adds), and, for a `dlopen`ed module with
//! thread-local data, `T size align`: each Worker gives it a block of its
//! own as it instantiates it (`__wasm_init_tls`), libc's registry having
//! laid out only the blocks of the modules the program started with.
//! A `#` column is empty: its number, which depends on where this process
//! loaded the modules, is in the last line, `V` and the numbers in the
//! lines' order, comma-separated. The rest is the same for a module in every
//! process that binds it alike, so the page parses it once
//! (gex.mjs `bindPlan`).
//!
//! A fork's child gets the link map as text (`gex_wasmprog_link`, put back
//! with `gex_wasmprog_adopt_link`): it reads the same files and publishes
//! the same modules at the same table bases over the parent's data.

use super::wasmprog::{entry_bridges, exe_path, Prog, Shed, CURRENT_BASE, PROGS};
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
    /// Each module's table slots (`Info::slots`), kept for a fork's child,
    /// which may not have parsed the module ([`DyProc::lazy`]).
    slot_len: Vec<u32>,
    /// libc's place in the link map and its `__gex_start` and
    /// `__gex_thread_start` slots (past its table base).
    entry_slots: Option<(usize, u32, u32)>,
    /// A fork's child that took its parent's link ([`adopt_inherited`]):
    /// every library's `Info` but the program's is a placeholder, parsed
    /// from its file at the first `dlopen` or `dlsym` ([`DyProc::infos`]).
    lazy: bool,
}

impl DyProc {
    /// Every module's `Info`, parsed now if this is a fork's child that took
    /// them on trust ([`DyProc::lazy`]): before anything looks a symbol up or
    /// links another module in.
    fn infos(&mut self) -> Result<(), String> {
        if !self.lazy {
            return Ok(());
        }
        for m in self.link.mods.iter_mut().skip(1) {
            // (its Info's sections only: the parent published its bytes)
            let parts = if CHECKED.load(Ordering::Relaxed) { None } else { crate::sigs::proc_home().read_wasm_parts(&m.path, Info::reads_section) };
            let b = parts.or_else(|| read(&m.path)).ok_or_else(|| format!("{}: gone since the fork", m.path))?;
            m.info = Info::parse(&b).map_err(|e| format!("{}: {e:#}", m.path))?;
        }
        // (built from the placeholders' empty exports if anything had asked)
        self.link.scope = Default::default();
        self.lazy = false;
        Ok(())
    }
}

static DYPROC: Mutex<Option<DyProc>> = Mutex::new(None);

thread_local! {
    /// [`DYPROC`] held by this thread from its fork's hold until its memory
    /// is copied ([`fork_hold`]): the link map the child gets is the one its
    /// memory has, with no `dlopen` on another thread halfway between.
    static FORK_HELD: std::cell::RefCell<Option<std::sync::MutexGuard<'static, Option<DyProc>>>> = const { std::cell::RefCell::new(None) };
}

/// `gex_fork_hold`, once the process's other threads are out of guest code:
/// this thread takes the link map's lock until [`fork_release`] (one in a
/// `dlopen` finishes it first).
pub(super) fn fork_hold() {
    let g = lock(&DYPROC);
    FORK_HELD.with(|h| *h.borrow_mut() = Some(g));
}

/// The fork's memory is copied: the link map's lock goes.
pub(super) fn fork_release() {
    let g = FORK_HELD.with(|h| h.borrow_mut().take());
    drop(g);
}

/// The link map, through the lock this thread holds for its fork if it
/// does ([`fork_hold`]): taking it again would wait for itself.
fn dy<R>(f: impl FnOnce(&mut Option<DyProc>) -> R) -> R {
    let mut f = Some(f);
    let held = FORK_HELD.with(|h| h.borrow_mut().as_mut().map(|g| (f.take().unwrap())(g)));
    match held {
        Some(r) => r,
        None => (f.take().unwrap())(&mut lock(&DYPROC)),
    }
}

/// A binding list's `#` columns, in order: its `V` line.
#[derive(Default)]
struct Vals(String);

impl Vals {
    fn push(&mut self, v: impl std::fmt::Display) {
        use std::fmt::Write;
        if !self.0.is_empty() {
            self.0.push(',');
        }
        let _ = write!(self.0, "{v}");
    }
    fn end(self, text: &mut String) {
        if !self.0.is_empty() {
            text.push_str("V\t");
            text.push_str(&self.0);
            text.push('\n');
        }
    }
}
/// Binding lists, by provider module number.
static BINDS: Mutex<Option<HashMap<u32, Binds>>> = Mutex::new(None);

/// A binding list and its shape's key: the lines before its `V` line are
/// the same in every process binding the module alike, and the page keeps
/// what JS parsed of them by this key (gex.mjs `plan:`), so a Worker that
/// finds it there reads only the `V` numbers ([`gex_wasmprog_bind_vals`]).
struct Binds {
    text: String,
    /// The shape's length (where the `V` line starts) and two independent
    /// hashes of it (Fx and FNV-1a), so that a key names one shape: the page
    /// takes a plan by key alone.
    shape: usize,
    hash: u64,
    hash2: u64,
    /// The `V` numbers (all 32-bit), as JS takes them.
    vals: Vec<u32>,
}

fn binds_put(number: u32, text: String) {
    use std::hash::Hasher;
    let shape = if text.starts_with("V\t") { 0 } else { text.rfind("\nV\t").map_or(text.len(), |i| i + 1) };
    let mut h = gex_dylink::FxHasher::default();
    h.write(&text.as_bytes()[..shape]);
    let hash = h.finish();
    let hash2 = text.as_bytes()[..shape].iter().fold(0xcbf2_9ce4_8422_2325u64, |a, &b| (a ^ b as u64).wrapping_mul(0x100_0000_01b3));
    let v = text[shape..].strip_prefix("V\t").unwrap_or("").trim_end_matches('\n');
    let vals = if v.is_empty() { Vec::new() } else { v.split(',').map(|n| n.parse().unwrap_or(0)).collect() };
    lock(&BINDS).get_or_insert_with(HashMap::new).insert(number, Binds { text, shape, hash, hash2, vals });
}
thread_local! {
    /// The modules this thread's last `dlopen` loaded, in initialisation
    /// order, for JS to run their constructors once it has instantiated them.
    /// The calling thread's own: a `dlopen` on another thread between this
    /// one's and its constructors would otherwise hand it that thread's
    /// list, and this one's constructors would never run.
    static DL_NEW: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
}
/// Bumped by every `dlopen` once its modules are published, and by every
/// Wine PE module's install ([`published`]): a Worker that
/// has not seen the latest number catches up after its next system call
/// (`gex_wasmprog_dl_gen`), so a function pointer another thread got from
/// `dlsym` and handed over is in its table by the time it can call it.
static DL_GEN: AtomicU32 = AtomicU32::new(0);

/// Modules another thread may call into were published outside a `dlopen`
/// (a Wine PE module, which Wine's loader installs on the loading thread):
/// every other Worker catches up after its next system call, before a
/// pointer into them handed over (an IRP's dispatch routine, a callback) can
/// reach a slot its table does not hold yet.
pub(crate) fn published() {
    DL_GEN.fetch_add(1, Ordering::Release);
}
/// A fork child's link map (`gex_wasmprog_adopt_link`).
static ADOPT: Mutex<Option<String>> = Mutex::new(None);
/// libc's registry to fill once the modules are instantiated (their data
/// initialised): the init order and libc's index.
static REGISTRY: Mutex<Option<(Vec<usize>, usize)>> = Mutex::new(None);

/// A library's bytes as loaded: guarded when [`gex_set_dl_checked`] is on.
pub(super) fn read(path: &str) -> Option<Vec<u8>> {
    crate::sigs::proc_home().read_file(path).map(checked)
}

/// A library's bytes for its `Info` only, when the page has its content key
/// (a process hashed this file, `wasmprog::shed_known`): the sections
/// `Info::parse` reads, and what stands for the rest, which the page
/// compiled from or will read again. Otherwise the whole file ([`read`]).
/// Plasma's processes each read 150 to 200 MiB of libraries to parse 10.
fn read_lib(path: &str, skel: &std::cell::RefCell<HashMap<String, Shed>>) -> Option<Vec<u8>> {
    if !CHECKED.load(Ordering::Relaxed) {
        if let Some(s) = super::wasmprog::shed_known(path) {
            if let Some(b) = crate::sigs::proc_home().read_wasm_parts(path, Info::reads_section) {
                skel.borrow_mut().insert(path.to_string(), s);
                return Some(b);
            }
        }
    }
    read(path)
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

/// A module's bytes at publishing: the bytes, or what stands for them
/// once they went ([`super::wasmprog::shed`]).
type Bytes = (Vec<u8>, Option<Shed>);

fn find_needed(p: &mut DyProc, from: usize) -> Result<Vec<Bytes>, String> {
    let (o, lp) = (p.origin.clone(), p.ldpath.clone());
    let mut sheds: HashMap<String, Shed> = HashMap::new();
    let skel = std::cell::RefCell::new(HashMap::new());
    let libs = p
        .link
        .needed_of_shed(
            from,
            |name, rp| gex_dylink::search(name, rp, &o, &lp, |path| read_lib(path, &skel)),
            |path, b| match skel.borrow_mut().remove(path).or_else(|| super::wasmprog::shed(path, &b)) {
                Some(s) => {
                    sheds.insert(path.to_string(), s);
                    Vec::new()
                }
                None => b,
            },
        )
        .map_err(|e| format!("{e:#}"))?;
    let mut out = Vec::new();
    for (name, path, b, info) in libs {
        let s = sheds.remove(&path);
        p.link.mods.push(Loaded { name, path, info, memory_base: 0, table_base: 0 });
        out.push((b, s));
    }
    Ok(out)
}

/// Publish `p.link.mods[from..]` (whose bytes are `bytes`) in
/// initialisation order: map data where it has none, give table ranges
/// where `keep` is false, bind. `entries` are gex's entries, bound in
/// libc's table range. Returns the order and libc's provider module.
fn publish(p: &mut DyProc, from: usize, bytes: Vec<Bytes>, relocate: bool, keep: bool, entries: Option<(u64, u64)>) -> Result<(Vec<usize>, Option<u32>), String> {
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
    p.slot_len.resize(p.link.mods.len(), 0);
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
    let mut bytes: Vec<Option<Bytes>> = bytes.into_iter().map(Some).collect();
    let mut done: HashSet<usize> = (0..from).collect();
    let mut libc_number = None;
    let (mut lens, mut entry_slots) = (Vec::new(), None);
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
        lens.push((i, len));
        if let [(_, s0), (_, s1)] = pairs[..] {
            entry_slots = Some((i, s0, s1));
        }
        let (regs, dirs, slots) = if pairs.is_empty() {
            (Vec::new(), Vec::new(), 0)
        } else {
            entry_bridges(&pairs.iter().map(|&(a, _)| a).collect::<Vec<_>>())
        };
        let mut text = String::new();
        let mut vals = Vals::default();
        for (imp, b) in m.info.imports.iter().zip(binds) {
            let (mo, na) = (&imp.module, &imp.name);
            let line = match b {
                Bind::Shim => format!("S\t{mo}\t{na}"),
                Bind::GotMem(v) | Bind::GotFunc(v) => {
                    vals.push(v);
                    format!("G\t{mo}\t{na}\t\t{}{}", imp.ty, if imp.wide { "\t64" } else { "" })
                }
                // and where the page finds it when the module's exports
                // are packed (gex.mjs `packExports`): a function's table
                // slot, a constant global's value
                Bind::Export { module, name } => {
                    let d = &p.link.mods[module];
                    vals.push(p.module_of[module]);
                    let at = match d.info.exports.get(&name) {
                        Some(x) if x.kind == Kind::Func => d.info.slots().1.get(&x.index).map(|s| {
                            vals.push(d.table_base + s);
                            "s".to_string()
                        }),
                        Some(x) if x.kind == Kind::Global && x.index >= d.info.global_imports => d.info.global_init
                            .get((x.index - d.info.global_imports) as usize)
                            .copied()
                            .flatten()
                            .map(|v| format!("g{}", v as i32)),
                        _ => None,
                    }
                    .unwrap_or_else(|| "-".to_string());
                    format!("E\t{mo}\t{na}\t\t{name}\t{at}")
                }
                // and the import's type, which the page calls the slot with
                Bind::Late { slot } => {
                    vals.push(slot);
                    match m.info.sigs.get(imp.ty as usize).cloned().flatten() {
                        Some(sig) => format!("L\t{mo}\t{na}\t\t{sig}"),
                        None => format!("L\t{mo}\t{na}\t"),
                    }
                }
                Bind::Missing(_) if imp.kind == Kind::Global => {
                    vals.push(0);
                    format!("G\t{mo}\t{na}\t\t{}{}", imp.ty, if imp.wide { "\t64" } else { "" })
                }
                Bind::Missing(msg) => {
                    let msg = if msg.is_empty() { format!("{}: undefined weak symbol {na} called", m.path) } else { msg };
                    format!("M\t{mo}\t{na}\tsymbol lookup error: {}", msg.replace(['\t', '\n'], " "))
                }
            };
            text.push_str(&line);
            text.push('\n');
        }
        for (s, n) in m.info.extra_slots() {
            vals.push(m.table_base + s);
            text.push_str(&format!("X\t\t{n}\n"));
        }
        if let Some((size, align)) = dl_tls(m, i >= p.startup)? {
            text.push_str(&format!("T\t{size}\t{align}\n"));
        }
        vals.end(&mut text);
        let number = {
            let mut provs = lock(&PROVIDERS);
            provs.push(ProviderModule { file: format!("wasmprog:{}", m.path), regs, dirs, slots, memory_base: m.memory_base as u64 });
            provs.len() as u32 - 1
        };
        p.module_of[i] = number;
        binds_put(number, text);
        let (wasm, shed) = bytes[i - from].take().unwrap_or_default();
        // the program's own bytes are from its image, a library's from its file
        let from_file = i != 0;
        let (size, key, gone) = match shed {
            Some(s) => (s.len, Some(s.key), Some(s.gone)),
            None => (wasm.len() as u32, None, None),
        };
        // built for wasm64 and lowered, as its program (64-bit Wine's loader
        // and its side module): i64 bases (is_lowered, from the import's type)
        let lowered = m.info.imports.iter().any(|imp| imp.module == "env" && imp.name == "__memory_base" && imp.wide);
        lock(&PROGS).push(Prog { number, len: size, wasm, relocate, relocating: false, key, path: m.path.clone(), lowered, from_file, gone });
        let (_, ok) = ftab(|f| f.publish_at(m.table_base, len.max(1), PROVIDER | number, Tier::Unit, &pairs));
        if !ok {
            return Err(format!("{}: table range {}+{} is taken", m.path, m.table_base, len));
        }
        if Some(i) == libc {
            libc_number = Some(number);
            CURRENT_BASE.store(m.table_base, Ordering::Relaxed);
        }
    }
    for (i, len) in lens {
        p.slot_len[i] = len;
    }
    if entry_slots.is_some() {
        p.entry_slots = entry_slots;
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
        link: Link { mods: vec![Loaded { name: path.clone(), path: path.clone(), info, memory_base: mem_base, table_base: 0 }], ..Default::default() },
        module_of: Vec::new(),
        ldpath,
        origin,
        startup: usize::MAX,
        slot_len: Vec::new(),
        entry_slots: None,
        lazy: false,
    };
    let mut bytes = vec![(wasm, None)];
    bytes.extend(find_needed(&mut p, 0)?);
    p.startup = p.link.mods.len();
    let (order, libc) = publish(&mut p, 0, bytes, true, false, Some((entry, thread_entry)))?;
    let libc = libc.ok_or("no libc")?;
    let li = p.module_of.iter().position(|&n| n == libc).unwrap();
    *lock(&REGISTRY) = Some((order, li));
    dy(|d| *d = Some(p));
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
    let mut vals = Vals::default();
    // undefined weak symbols: null, as ELF leaves them (a strong one stays
    // an import nothing gives, and instantiating says so)
    for (mo, na, b, wide) in info.static_weak_binds().unwrap_or_default() {
        text.push_str(&match b {
            Bind::GotMem(v) | Bind::GotFunc(v) => {
                vals.push(v);
                format!("G\t{mo}\t{na}\t\t1{}\n", if wide { "\t64" } else { "" })
            }
            _ => format!("M\t{mo}\t{na}\tsymbol lookup error: {path}: undefined weak symbol {na} called\n"),
        });
    }
    for (s, n) in info.extra_slots() {
        vals.push(base + s);
        text.push_str(&format!("X\t\t{n}\n"));
    }
    vals.end(&mut text);
    binds_put(number, text);
    let slots = info.slots().0;
    dy(|d| *d = Some(DyProc {
        link: Link { mods: vec![Loaded { name: path.to_string(), path: path.to_string(), info, memory_base: mem_base, table_base: base }], ..Default::default() },
        module_of: vec![number],
        ldpath: Vec::new(),
        // (its directory: where `dlopen` finds a bare soname, `Link::dlopen_runpath`)
        origin: path.rsplit_once('/').map(|(d, _)| if d.is_empty() { "/" } else { d }.to_string()).unwrap_or_else(|| ".".into()),
        startup: 1,
        slot_len: vec![slots],
        entry_slots: None,
        lazy: false,
    }));
}

/// `execve` (`wasmprog::on_exec`): the old program's link map goes with it.
pub(super) fn on_exec() {
    dy(|d| *d = None);
    *lock(&BINDS) = None;
    DL_NEW.with(|d| d.borrow_mut().clear());
    *lock(&ADOPT) = None;
    *lock(&REGISTRY) = None;
}

/// A fork's child: the parent's link map (`ADOPT`) again, at the same
/// table bases, over the parent's data.
pub(super) fn adopt(wasm: Vec<u8>, entry: u64, thread_entry: u64) -> Result<u32, String> {
    let text = lock(&ADOPT).take().ok_or("a fork's child of a program with shared libraries, without its link map")?;
    let _ = exe_path();
    let mut lines = text.splitn(4, '\n');
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
    let rest = lines.next().unwrap_or("");
    let mut p = DyProc { link: Link::default(), module_of: Vec::new(), ldpath, origin, startup, slot_len: Vec::new(), entry_slots: None, lazy: false };
    // The modules' lines, then what the parent had made of them, if it said.
    let (mods, inherited) = match rest.split_once("\ninherit\t") {
        Some((m, i)) => (m, Some(i)),
        None => (rest.strip_suffix('\n').unwrap_or(rest), None),
    };
    if let Some(inh) = inherited {
        match adopt_inherited(&mut p, mods, inh, &wasm, entry, thread_entry) {
            Ok(Some(libc)) => {
                dy(|d| *d = Some(p));
                return Ok(libc);
            }
            Ok(None) => p.link = Link::default(),
            Err(e) => return Err(e),
        }
    }
    let mut bytes = Vec::new();
    for (k, l) in mods.lines().enumerate() {
        let f: Vec<&str> = l.splitn(4, '\t').collect();
        let [mb, tb, name, path] = f[..] else { return Err(format!("link map: bad line {l:?}")) };
        let b = if k == 0 { checked(wasm.clone()) } else { read(path).ok_or_else(|| format!("{path}: gone since the fork"))? };
        let info = Info::parse(&b).map_err(|e| format!("{path}: {e:#}"))?;
        let num = |s: &str| s.parse::<u32>().map_err(|_| format!("link map: bad number {s:?}"));
        p.link.mods.push(Loaded { name: name.to_string(), path: path.to_string(), info, memory_base: num(mb)?, table_base: num(tb)? });
        let shed = if k == 0 { None } else { super::wasmprog::shed(path, &b) };
        bytes.push(if shed.is_some() { (Vec::new(), shed) } else { (b, None) });
    }
    let (_, libc) = publish(&mut p, 0, bytes, false, true, Some((entry, thread_entry)))?;
    dy(|d| *d = Some(p));
    libc.ok_or_else(|| "no libc".into())
}

/// One module of a fork's parent, as [`gex_wasmprog_link`] says it: its
/// provider module number, table slots, the program record's facts and
/// its binding list.
struct Inherited<'a> {
    index: usize,
    number: u32,
    slots: u32,
    lowered: bool,
    len: u32,
    key: Option<(u64, u64)>,
    gone: Option<(u64, i64, u64)>,
    binds: &'a str,
}

/// [`adopt`] from what the parent had made of its link map: its modules'
/// binding lists and records, at its module numbers, so the child reads,
/// parses and binds none of its libraries (266 of them, 248 MB, were 287 ms
/// of every fork of Plasma's shell before its first instruction). Their
/// `Info`s wait for a `dlopen` or `dlsym` ([`DyProc::infos`]). `Ok(None)`:
/// this child cannot number them as the parent did (a provider of its own
/// already there), and binds them itself.
fn adopt_inherited(p: &mut DyProc, mods: &str, inh: &str, wasm: &[u8], entry: u64, thread_entry: u64) -> Result<Option<u32>, String> {
    if CHECKED.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let bad = || "link map: bad inherited section".to_string();
    let (head, mut rest) = inh.split_once('\n').ok_or_else(bad)?;
    let h: Vec<&str> = head.split('\t').collect();
    let [li, s0, s1] = h[..] else { return Err(bad()) };
    let num = |s: &str| s.parse::<u64>().map_err(|_| format!("link map: bad number {s:?}"));
    let entry_slots = (num(li)? as usize, num(s0)? as u32, num(s1)? as u32);
    let pair = |s: &str| -> Result<Option<Vec<u64>>, String> {
        if s == "-" {
            return Ok(None);
        }
        s.split(':').map(|x| x.parse::<i64>().map(|v| v as u64).map_err(|_| bad())).collect::<Result<Vec<_>, _>>().map(Some)
    };
    let mut list = Vec::new();
    while !rest.is_empty() {
        let (line, after) = rest.split_once('\n').ok_or_else(bad)?;
        let f: Vec<&str> = line.split('\t').collect();
        let ["P", index, number, slots, lowered, len, key, gone, blen] = f[..] else { return Err(bad()) };
        let blen = num(blen)? as usize;
        let binds = after.get(..blen).ok_or_else(bad)?;
        rest = &after[blen..];
        list.push(Inherited {
            index: num(index)? as usize,
            number: num(number)? as u32,
            slots: num(slots)? as u32,
            lowered: lowered == "1",
            len: num(len)? as u32,
            key: pair(key)?.map(|v| (v[0], v[1])),
            gone: pair(gone)?.map(|v| (v[0], v[1] as i64, v[2])),
            binds,
        });
    }
    // The parent's numbers, in order from where this child's next one is:
    // its binding lists name modules by them.
    let first = lock(&PROVIDERS).len() as u32;
    if list.iter().enumerate().any(|(k, m)| m.number != first + k as u32) {
        return Ok(None);
    }
    for (k, l) in mods.lines().enumerate() {
        let f: Vec<&str> = l.splitn(4, '\t').collect();
        let [mb, tb, name, path] = f[..] else { return Err(format!("link map: bad line {l:?}")) };
        // the program's own, from its image (`gex_wasmprog_link` says
        // whether it names libraries); the rest from their files when asked
        let info = if k == 0 { Info::parse(wasm).map_err(|e| format!("{path}: {e:#}"))? } else { Info::default() };
        p.link.mods.push(Loaded { name: name.to_string(), path: path.to_string(), info, memory_base: num(mb)? as u32, table_base: num(tb)? as u32 });
    }
    let n = p.link.mods.len();
    if list.len() != n || list.iter().any(|m| m.index >= n) || entry_slots.0 >= n {
        return Err(bad());
    }
    p.module_of = vec![u32::MAX; n];
    p.slot_len = vec![0; n];
    p.entry_slots = Some(entry_slots);
    p.lazy = true;
    let mut libc_number = None;
    for m in &list {
        let i = m.index;
        let (path, table_base, memory_base) = (p.link.mods[i].path.clone(), p.link.mods[i].table_base, p.link.mods[i].memory_base);
        let pairs = if i == entry_slots.0 { vec![(entry, entry_slots.1), (thread_entry, entry_slots.2)] } else { Vec::new() };
        let (regs, dirs, slots) = if pairs.is_empty() {
            (Vec::new(), Vec::new(), 0)
        } else {
            entry_bridges(&pairs.iter().map(|&(a, _)| a).collect::<Vec<_>>())
        };
        {
            let mut provs = lock(&PROVIDERS);
            if provs.len() as u32 != m.number {
                return Err(format!("{}: provider module {} where the parent's was {}", path, provs.len(), m.number));
            }
            provs.push(ProviderModule { file: format!("wasmprog:{}", path), regs, dirs, slots, memory_base: memory_base as u64 });
        }
        p.module_of[i] = m.number;
        p.slot_len[i] = m.slots;
        binds_put(m.number, m.binds.to_string());
        // The program's bytes are its image's; a library's are read again
        // when a Worker asks, as once they have gone in the parent. One whose
        // parent still held them is read now.
        let (wasm, key, gone) = if i == 0 {
            (wasm.to_vec(), None, None)
        } else if m.gone.is_some() {
            (Vec::new(), m.key, m.gone)
        } else {
            let b = read(&path).ok_or_else(|| format!("{}: gone since the fork", path))?;
            match super::wasmprog::shed(&path, &b) {
                Some(s) => (Vec::new(), Some(s.key), Some(s.gone)),
                None => (b, None, None),
            }
        };
        let len = if wasm.is_empty() { m.len } else { wasm.len() as u32 };
        lock(&PROGS).push(Prog { number: m.number, len, wasm, relocate: false, relocating: false, key, path: path.clone(), lowered: m.lowered, from_file: i != 0, gone });
        let (_, ok) = ftab(|f| f.publish_at(table_base, m.slots.max(1), PROVIDER | m.number, Tier::Unit, &pairs));
        if !ok {
            return Err(format!("{}: table range {}+{} is taken", path, table_base, m.slots));
        }
        if i == entry_slots.0 {
            libc_number = Some(m.number);
            CURRENT_BASE.store(table_base, Ordering::Relaxed);
        }
    }
    if std::env::var_os("GEX_WASM_TRACE").is_some() || crate::tracing() {
        for m in &list {
            let d = &p.link.mods[m.index];
            crate::warn_line(&format!("gex: wasm32 module {}: data {:#x}, table {} (as its parent bound it)\n", d.path, d.memory_base, d.table_base));
        }
    }
    libc_number.map(Some).ok_or_else(|| "no libc".into())
}

/// The program's modules are instantiated (JS, after catching up): fill
/// libc's registry if a fresh program is waiting for it. Returns 0, or -1
/// (the process ends) when that fails.
#[no_mangle]
pub extern "C" fn gex_wasmprog_ready() -> i32 {
    let Some((order, libc)) = lock(&REGISTRY).take() else { return 0 };
    let r = dy(|d| d.as_ref().map(|p| write_registry(&p.link, &order, libc)));
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
    lock(&BINDS).as_ref().and_then(|b| b.get(&number)).map_or(0, |t| put_str(&t.text, buf, cap))
}

/// Provider module `number`'s binding list's shape key ([`Binds`]): lane 0
/// its length plus one (0: no list), 1 and 2 its Fx hash's low and high
/// words, 3 and 4 its FNV-1a hash's.
#[no_mangle]
pub extern "C" fn gex_wasmprog_bind_key(number: u32, lane: u32) -> u32 {
    lock(&BINDS).as_ref().and_then(|b| b.get(&number)).map_or(0, |t| match lane {
        0 => t.shape as u32 + 1,
        1 => t.hash as u32,
        2 => (t.hash >> 32) as u32,
        3 => t.hash2 as u32,
        _ => (t.hash2 >> 32) as u32,
    })
}

/// Provider module `number`'s binding list's `V` numbers, little-endian
/// 32-bit words at `buf` (up to `cap` bytes); their bytes. With them and its
/// shape's plan (on the page: [`gex_wasmprog_bind_key`]) JS needs no text.
#[no_mangle]
pub extern "C" fn gex_wasmprog_bind_vals(number: u32, buf: u32, cap: u32) -> u32 {
    lock(&BINDS).as_ref().and_then(|b| b.get(&number)).map_or(0, |t| {
        let n = t.vals.len() * 4;
        if buf != 0 && cap as usize >= n {
            // a buffer from the shim's heap (put_str's)
            for (o, v) in crate::shim_slice_mut(buf, n as u32).chunks_exact_mut(4).zip(&t.vals) {
                o.copy_from_slice(&v.to_le_bytes());
            }
        }
        n as u32
    })
}

/// The process's link map as text, for a fork's child; 0 when the program
/// has no shared libraries.
#[no_mangle]
pub extern "C" fn gex_wasmprog_link(buf: u32, cap: u32) -> u32 {
    // Asked twice, its length and then its text: made once.
    if buf != 0 {
        if let Some(t) = LINK_TEXT.with(|l| l.borrow_mut().take()) {
            return put_str(&t, buf, cap);
        }
    }
    let Some(t) = dy(|d| d.as_ref().filter(|p| !p.link.mods[0].info.needed.is_empty()).map(link_text)) else { return 0 };
    let n = put_str(&t, buf, cap);
    if buf == 0 {
        LINK_TEXT.with(|l| *l.borrow_mut() = Some(t));
    }
    n
}

thread_local! {
    /// [`gex_wasmprog_link`]'s text between its two calls.
    static LINK_TEXT: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// The link map as a fork's child takes it: where each module's data and
/// table range are, then (`inherit`) libc's entry slots and, by provider
/// module number, each module's table slots, program record and binding
/// list, so the child binds nothing again ([`adopt_inherited`]).
fn link_text(p: &DyProc) -> String {
    use std::fmt::Write;
    let mut t = format!("origin\t{}\nldpath\t{}\nstartup\t{}\n", p.origin, p.ldpath.join(":"), p.startup);
    for m in &p.link.mods {
        let _ = writeln!(t, "{}\t{}\t{}\t{}", m.memory_base, m.table_base, m.name, m.path);
    }
    let Some((li, s0, s1)) = p.entry_slots else { return t };
    if p.slot_len.len() != p.link.mods.len() || p.module_of.len() != p.link.mods.len() {
        return t;
    }
    let binds = lock(&BINDS);
    let progs = lock(&PROGS);
    let mut order: Vec<usize> = (0..p.link.mods.len()).collect();
    order.sort_by_key(|&i| p.module_of[i]);
    let mut s = String::new();
    for i in order {
        let number = p.module_of[i];
        let (Some(b), Some(g)) = (binds.as_ref().and_then(|b| b.get(&number)).map(|b| &b.text), progs.iter().find(|g| g.number == number)) else { return t };
        let key = g.key.map_or("-".to_string(), |(a, c)| format!("{}:{}", a as i64, c as i64));
        let gone = g.gone.map_or("-".to_string(), |(a, m, c)| format!("{}:{m}:{}", a as i64, c as i64));
        let _ = writeln!(s, "P\t{i}\t{number}\t{}\t{}\t{}\t{key}\t{gone}\t{}", p.slot_len[i], g.lowered as u8, g.len, b.len());
        s.push_str(b);
    }
    let _ = writeln!(t, "inherit\t{li}\t{s0}\t{s1}");
    t.push_str(&s);
    t
}

/// A fork's child: its parent's link map ([`gex_wasmprog_link`]).
#[no_mangle]
pub extern "C" fn gex_wasmprog_adopt_link(ptr: u32, len: u32) {
    let b = crate::shim_slice_mut(ptr, len).to_vec();
    *lock(&ADOPT) = Some(String::from_utf8_lossy(&b).into_owned());
}

/// libc's `__gex_dsos` in guest memory, filled ([`gex_dylink::Link::write_registry`]).
fn write_registry(link: &gex_dylink::Link, order: &[usize], libc: usize) -> gex_dylink::Result<()> {
    let mut mem = crate::guest();
    let at = link.dsos_at(libc)? as u64;
    let n = mem.read(at, 12).ok().and_then(gex_dylink::Link::dsos_len).unwrap_or(0);
    let dsos = mem.read_mut(at, n).unwrap_or(&mut []);
    link.write_registry(dsos, order, libc)
}

fn guest_str(ptr: u64) -> Option<String> {
    let s = crate::guest().read_cstr(ptr, 1 << 20).ok()?;
    Some(String::from_utf8_lossy(&s).into_owned())
}

fn dlopen(path: &str) -> Result<u32, String> {
    dy(|g| dlopen_in(g, path))
}

fn dlopen_in(g: &mut Option<DyProc>, path: &str) -> Result<u32, String> {
    let Some(p) = g.as_mut() else {
        return Err(format!("{path}: dlopen in a static program (link it with GEXCC_SHARED=1)"));
    };
    p.infos()?;
    if let Some(i) = p.link.loaded(path) {
        return Ok(HANDLE_BASE + i as u32 * 16);
    }
    let (o, lp) = (p.origin.clone(), p.ldpath.clone());
    let rp = p.link.dlopen_runpath();
    let skel = std::cell::RefCell::new(HashMap::new());
    let (found, b) = gex_dylink::search(path, &rp, &o, &lp, |path| read_lib(path, &skel)).map_err(|e| format!("{e:#}"))?;
    let info = Info::parse(&b).map_err(|_| format!("{found}: not a wasm shared library"))?;
    if !info.dylink {
        return Err(format!("{found}: not a wasm shared library"));
    }
    // On failure the link map is as it was: its new modules dropped again.
    // (A copy of the whole map made for that, every library's Info with
    // its exports, and a scope index built afresh for the copy, were most
    // of Plasma's shell's dlopens: 1.8 s to copy, 0.7 s to drop and 1.3 s
    // to index again across a boot's processes.)
    let from = p.link.mods.len();
    let (module_of, slot_len, entry_slots) = (p.module_of.len(), p.slot_len.len(), p.entry_slots);
    p.link.mods.push(Loaded { name: path.to_string(), path: found, info, memory_base: 0, table_base: 0 });
    let r = (|| {
        let shed = skel.borrow_mut().remove(&p.link.mods[from].path).or_else(|| super::wasmprog::shed(&p.link.mods[from].path, &b));
        let mut all = vec![if shed.is_some() { (Vec::new(), shed) } else { (b, None) }];
        all.extend(find_needed(p, from)?);
        publish(p, from, all, true, false, None)
    })();
    let order = match r {
        Ok((order, _)) => order,
        Err(e) => {
            p.link.truncate(from);
            p.module_of.truncate(module_of);
            p.slot_len.truncate(slot_len);
            p.entry_slots = entry_slots;
            return Err(e);
        }
    };
    DL_GEN.fetch_add(1, Ordering::Release);
    DL_NEW.with(|d| *d.borrow_mut() = order.iter().map(|&i| p.module_of[i]).collect());
    Ok(HANDLE_BASE + from as u32 * 16)
}

/// `gex.dlopen(path, err, errlen)` (libgex's dl.c), the shim's half: load a
/// library and what it needs; its handle, or 0 with the message in `err`.
/// JS then catches up (instantiating them) and runs their constructors
/// ([`gex_wasmprog_dl_new`]). Addresses are 64-bit for a memory64
/// program's (libgex built with GEX_MEMORY64); JS widens a 32-bit one's.
#[no_mangle]
pub extern "C" fn gex_wasmprog_dlopen(path: u64, err: u64, errlen: u32) -> u32 {
    DL_NEW.with(|d| d.borrow_mut().clear());
    let r = guest_str(path).ok_or_else(|| "dlopen: bad path".to_string()).and_then(|p| dlopen(&p));
    match r {
        Ok(h) => h,
        Err(msg) => {
            let n = msg.len().min(errlen.saturating_sub(1) as usize);
            if let Ok(d) = crate::guest().read_mut(err as u64, n + 1) {
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
    DL_NEW.with(|d| d.borrow().get(k as usize).map_or(-1, |&n| n as i32))
}

/// `gex.dlsym(handle, name)`: a function's table index or a datum's
/// address, 0 when not found (handle 0: the global scope).
#[no_mangle]
pub extern "C" fn gex_wasmprog_dlsym(handle: u32, name: u64) -> u32 {
    let Some(name) = guest_str(name) else { return 0 };
    dy(|g| {
        let Some(p) = g.as_mut() else { return 0 };
        if let Err(e) = p.infos() {
            crate::warn_line(&format!("gex: dlsym {name}: {e}\n"));
            return 0;
        }
        p.link.dlsym(handle, &name)
    })
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
    dy(|g| g.as_ref().and_then(|p| p.module_of.iter().position(|&n| n == number)).map_or(-1, |i| i as i32))
}

/// Whether this process is a fork's child whose libraries it took from its
/// parent's link ([`adopt_inherited`]) and has not parsed since
/// (`GEX_LOG=forklink`).
#[no_mangle]
pub extern "C" fn gex_wasmprog_inherited() -> i32 {
    dy(|g| g.as_ref().is_some_and(|p| p.lazy)) as i32
}

/// [`DL_GEN`], for a Worker to compare after a system call.
#[no_mangle]
pub extern "C" fn gex_wasmprog_dl_gen() -> u32 {
    DL_GEN.load(Ordering::Acquire)
}
