//! Shared libraries for native wasm32 programs, native half (docs/gexcc.md
//! §7; the shim-independent half is the `gex-dylink` crate).
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
//! A fork's child shares the process's registry and function map
//! natively, so it instantiates the same modules at the same table bases;
//! the data, relocated, is the parent's copy.

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
    let trace = std::env::var_os("GEX_WASM_TRACE").is_some();
    let env = initial_env(store, memory);
    let ldpath: Vec<String> = env
        .iter()
        .find_map(|e| e.strip_prefix("LD_LIBRARY_PATH="))
        .map(|v| v.split(':').filter(|s| !s.is_empty()).map(str::to_string).collect())
        .unwrap_or_default();
    let origin = path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_else(|| ".".into());
    let mut link = Link {
        mods: vec![Loaded {
            name: path.to_string(),
            path: path.to_string(),
            info,
            memory_base: mem_base as u32,
            table_base: 0,
        }],
    };
    let libs = {
        let state = store.data_mut();
        link.needed_of(0, |name, rp| {
            gex_dylink::search(name, rp, &origin, &ldpath, |p| state.with_kernel(|k| k.read_file(p)))
        })?
    };
    let mut bytes: Vec<Vec<u8>> = vec![wasm];
    for (name, p, b, info) in libs {
        // its data, where its own mmap would put it
        let align = 1u64 << info.mem_align.min(16);
        let size = (info.mem_size as u64).max(1) + if align > 0x1000 { align } else { 0 };
        let at = crate::libprov::alloc(store.data_mut(), size, &p)?;
        let base = (at + align - 1) & !(align - 1);
        if base > u32::MAX as u64 {
            bail!("{p}: data mapped above 4 GiB");
        }
        link.mods.push(Loaded {
            name,
            path: p,
            info,
            memory_base: base as u32,
            table_base: 0,
        });
        bytes.push(b);
    }
    // compiled before the table is touched (nothing to undo on failure)
    let engine = store.engine().clone();
    let modules: Vec<Module> = bytes
        .iter()
        .zip(&link.mods)
        .map(|(b, m)| gex_cache::compile_cached(&engine, b).with_context(|| format!("compiling {}", m.path)))
        .collect::<Result<_>>()?;
    let shared = store
        .data()
        .jit
        .as_ref()
        .map(|j| Arc::clone(&j.shared))
        .ok_or_else(|| anyhow!("wasm32 program: the function table is off (--no-jit?)"))?;
    let order = link.init_order(0);
    // libc: where gex enters threads, and the registry
    let libc = link
        .lookup("__gex_start", Kind::Func)
        .ok_or_else(|| anyhow!("{path}: no library defines __gex_start (libc.so)"))?;
    {
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        s.wasm_program = true;
        // Table bases: consecutive ranges from the map's end, in order.
        let mut next = s.ftab.map.size();
        for &i in &order {
            let (len, _) = link.mods[i].info.slots();
            link.mods[i].table_base = next;
            next += len.max(1);
        }
        let first_module = s.ftab.modules.len() as u32;
        let module_of = |i: usize| first_module + order.iter().position(|&j| j == i).unwrap() as u32;
        let mut done = HashSet::new();
        for (k, &i) in order.iter().enumerate() {
            let m = &link.mods[i];
            let binds = link.bind(i, &done)?;
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
                        Bind::Export { module, name } => NBind::Export {
                            module: module_of(module),
                            name,
                        },
                        Bind::Late { slot } => NBind::Late { slot },
                        Bind::Missing(msg) => NBind::Missing(msg),
                    };
                    (imp.module.clone(), imp.name.clone(), nb)
                })
                .collect();
            let extra = m.info.extra_slots().into_iter().map(|(s, n)| (m.table_base + s, n)).collect();
            let (len, at) = m.info.slots();
            let pairs: Vec<(u64, u32)> = if i == libc {
                let slot = |n: &str| at[&m.info.exports[n].index];
                vec![(entry, slot("__gex_start")), (thread_entry, slot("__gex_thread_start"))]
            } else {
                Vec::new()
            };
            let module = first_module + k as u32;
            debug_assert_eq!(module, module_of(i));
            s.ftab.stateful.insert(
                module,
                crate::libprov::Extras {
                    memory_base: m.memory_base as u64,
                },
            );
            s.ftab.dylink.insert(
                module,
                Arc::new(Binds {
                    path: m.path.clone(),
                    imports,
                    extra,
                }),
            );
            s.ftab.modules.push(modules[i].clone());
            let (n, ok) = s
                .ftab
                .map
                .publish_at(m.table_base, len, module, gex_kernel::ftab::Tier::Unit, &pairs);
            if !ok {
                bail!("{}: its table range moved while loading", m.path);
            }
            s.ftab.providers.insert(n);
        }
    }
    {
        let data = memory.bytes();
        for a in [entry, thread_entry] {
            gex_kernel::ftab::fcache_clear(data, a);
        }
    }
    crate::ftab::publish_bridges(&shared, &[entry, thread_entry], &[])?;
    store.data_mut().with_kernel(|k| k.wasm32 = true);
    crate::ftab::catch_up(&mut *store)?;
    write_registry(memory, &link, &order, libc)?;
    if trace {
        for &i in &order {
            let m = &link.mods[i];
            eprintln!(
                "gex: wasm32 module {}: data {:#x}+{:#x}, table {}",
                m.path, m.memory_base, m.info.mem_size, m.table_base
            );
        }
        eprintln!(
            "gex: wasm32 program {path}: {} modules loaded in {:.3}s",
            link.mods.len(),
            t0.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

/// libc's `__gex_dsos` (gex_dso.h): every module but libc, in init order,
/// with its TLS image and functions (table indices); the program's
/// constructors are crt1's to run.
fn write_registry(memory: &gex_mem::GuestMemory, link: &Link, order: &[usize], libc: usize) -> Result<()> {
    let lc = &link.mods[libc];
    let Some(off) = lc.info.data_offset("__gex_dsos") else {
        bail!("{}: no __gex_dsos (a libc.so older than gex's loader)", lc.path);
    };
    let at = (lc.memory_base + off) as usize;
    let b = memory.bytes();
    let rd = |b: &[u8], a: usize| u32::from_le_bytes(b[a..a + 4].try_into().unwrap());
    let (version, max) = (rd(b, at), rd(b, at + 8));
    if version != 1 {
        bail!("{}: __gex_dsos version {version} (want 1)", lc.path);
    }
    let own = |m: &Loaded, n: &str| -> u32 {
        let (_, slots) = m.info.slots();
        m.info
            .exports
            .get(n)
            .filter(|e| e.kind == Kind::Func)
            .and_then(|e| slots.get(&e.index))
            .map_or(0, |&s| m.table_base + s)
    };
    let value = |m: &Loaded, n: &str| -> u32 {
        m.info
            .exports
            .get(n)
            .filter(|e| e.kind == Kind::Global && e.index >= m.info.global_imports)
            .and_then(|e| {
                m.info
                    .global_init
                    .get((e.index - m.info.global_imports) as usize)
                    .copied()
                    .flatten()
            })
            .unwrap_or(0)
    };
    let mut k = 0usize;
    for &i in order {
        if i == libc {
            continue;
        }
        if k as u32 >= max {
            bail!("more than {max} shared libraries");
        }
        let m = &link.mods[i];
        let tls_size = value(m, "__tls_size");
        let e = at + 12 + k * 24;
        let words = [
            tls_size,
            value(m, "__tls_align").max(1),
            if tls_size != 0 { own(m, "__wasm_init_tls") } else { 0 },
            if tls_size != 0 { own(m, "__gex_set_tls_base") } else { 0 },
            if i == 0 { 0 } else { own(m, "__wasm_call_ctors") },
            0,
        ];
        if tls_size != 0 && (words[2] == 0 || words[3] == 0) {
            bail!(
                "{}: has thread-local data but does not export __wasm_init_tls and __gex_set_tls_base",
                m.path
            );
        }
        for (j, w) in words.iter().enumerate() {
            b[e + j * 4..e + j * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        k += 1;
    }
    // libc's own constructors first (it is initialised before anything it
    // serves): it goes at the head
    let lctors = own(lc, "__wasm_call_ctors");
    if lctors != 0 {
        if k as u32 >= max {
            bail!("more than {max} shared libraries");
        }
        b.copy_within(at + 12..at + 12 + k * 24, at + 12 + 24);
        let w = [0u32, 1, 0, 0, lctors, 0];
        for (j, x) in w.iter().enumerate() {
            b[at + 12 + j * 4..at + 16 + j * 4].copy_from_slice(&x.to_le_bytes());
        }
        k += 1;
    }
    b[at + 4..at + 8].copy_from_slice(&(k as u32).to_le_bytes());
    Ok(())
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
    Ok(())
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
