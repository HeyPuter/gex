//! The host half of a stateful provider's own JIT (docs/unity-dotnet.md P5):
//! Mono's runtime wasm JIT (r58Playz/dotnet-runtime, `mini-wasm.c`) compiles
//! a managed method to a wasm module at run time and, in the browser,
//! instantiates it on each worker with `new WebAssembly.Instance`, then
//! writes its exports into that worker's function table. Under gex a guest
//! thread is a store, so the same three operations are host imports
//! (`mini-wasm-gex.h`):
//!
//! * `provider_jit_instantiate`: compile (once per process, by content) and
//!   instantiate into this store; install named exports at table slots;
//! * `provider_jit_table`: what this store's table holds at a slot (the
//!   browser's `__wjSlotFn` bookkeeping);
//! * `provider_jit_reserve`: a range of the process's function table for the
//!   JIT's slots, every store's filled with a placeholder.
//!
//! The provider and its modules share the process's `ftab` (a table64 beside
//! a 64-bit memory), so a slot index is the same in every store.

use super::*;
use std::cell::RefCell;
use std::collections::HashMap;
use wasmtime::AsContextMut;

/// Compiled JIT modules, by a hash of their bytes: each thread instantiates
/// the same method, and compiling once is what the browser's per-worker
/// `WebAssembly.Module` would like to have done.
static MODULES: Mutex<Option<HashMap<u64, Module>>> = Mutex::new(None);

thread_local! {
    /// The functions this store installed, by slot: `provider_jit_table`'s
    /// "is it still what we put there" (one store per guest thread).
    static INSTALLED: RefCell<HashMap<u64, wasmtime::Func>> = RefCell::new(HashMap::new());
}

/// `GEX_WASMJIT_TRACE=1`: one line per instantiation and reservation.
fn trace() -> bool {
    static T: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *T.get_or_init(|| std::env::var_os("GEX_WASMJIT_TRACE").is_some())
}

fn bytes_of(caller: &Caller<'_, HostState>, ptr: i64, len: i64) -> Result<Vec<u8>> {
    let m = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let b = m.bytes();
    Ok(b.get(ptr as usize..(ptr as usize).saturating_add(len as usize)).ok_or_else(|| anyhow!("wasmjit: range {ptr:#x}+{len} outside memory"))?.to_vec())
}

fn write_err(caller: &Caller<'_, HostState>, buf: i64, cap: i64, msg: &str) {
    if buf == 0 || cap <= 0 {
        return;
    }
    if let Some(m) = caller.data().memory.clone() {
        let n = (cap as usize - 1).min(msg.len());
        let mut v = msg.as_bytes()[..n].to_vec();
        v.push(0);
        if let Some(d) = m.bytes().get_mut(buf as usize..buf as usize + v.len()) {
            d.copy_from_slice(&v);
        }
    }
}

fn table_of(caller: &Caller<'_, HostState>) -> Result<wasmtime::Table> {
    caller.data().jit.as_ref().and_then(|j| j.ftab.as_ref()).map(|s| s.table).ok_or_else(|| anyhow!("wasmjit: no function table"))
}

fn table_get(caller: &mut Caller<'_, HostState>, t: wasmtime::Table, i: u64) -> Option<wasmtime::Func> {
    match t.get(&mut *caller, i) {
        Some(wasmtime::Ref::Func(Some(f))) => Some(f),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn instantiate(
    caller: &mut Caller<'_, HostState>,
    bytes: i64,
    len: i64,
    cnames: i64,
    consts: i64,
    nconsts: i64,
    slots: i64,
    names: i64,
    n: i64,
) -> Result<()> {
    crate::ftab::catch_up(&mut *caller)?;
    let wasm = bytes_of(caller, bytes, len)?;
    let engine = caller.data().jit.as_ref().map(|j| j.engine.clone()).ok_or_else(|| anyhow!("wasmjit: no engine"))?;
    let module = {
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            wasm.hash(&mut h);
            h.finish()
        };
        let mut g = MODULES.lock().unwrap_or_else(|e| e.into_inner());
        let map = g.get_or_insert_with(HashMap::new);
        match map.get(&key) {
            Some(m) => m.clone(),
            None => {
                let m = gex_cache::compile(&engine, &wasm).map_err(|e| anyhow!("compile: {e:#}"))?;
                map.insert(key, m.clone());
                m
            }
        }
    };
    // The s.* constants, by name.
    let mut cv: HashMap<String, u64> = HashMap::new();
    if nconsts > 0 {
        let nb = bytes_of(caller, cnames, 256)?;
        let vb = bytes_of(caller, consts, nconsts * 8)?;
        for (k, name) in nb.split(|&c| c == 0).take(nconsts as usize).enumerate() {
            let v = u64::from_le_bytes(vb[k * 8..k * 8 + 8].try_into().unwrap());
            cv.insert(String::from_utf8_lossy(name).into_owned(), v);
        }
    }
    let table = table_of(caller)?;
    let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let mut externs: Vec<wasmtime::Extern> = Vec::new();
    for imp in module.imports() {
        let e: wasmtime::Extern = match (imp.module(), imp.name()) {
            ("m", "h") => memory.shared().clone().into(),
            ("f", "f") => table.into(),
            ("s", "p") => {
                let mut cx = caller.as_context_mut();
                crate::libprov::stack(&mut cx)?.into()
            }
            ("s", name) => {
                let v = *cv.get(name).ok_or_else(|| anyhow!("no value for import s.{name}"))?;
                let wasmtime::ExternType::Global(gt) = imp.ty() else { bail!("import s.{name} is not a global") };
                let val = match gt.content() {
                    wasmtime::ValType::I64 => Val::I64(v as i64),
                    _ => Val::I32(v as u32 as i32),
                };
                wasmtime::Global::new(&mut *caller, wasmtime::GlobalType::new(gt.content().clone(), gt.mutability()), val)?.into()
            }
            ("h", idx) => {
                let i: u64 = idx.parse().map_err(|_| anyhow!("import h.{idx}: not a table index"))?;
                table_get(caller, table, i).ok_or_else(|| anyhow!("import h.{i}: table slot holds no function"))?.into()
            }
            (m, name) => bail!("unexpected import {m}.{name}"),
        };
        externs.push(e);
    }
    let inst = wasmtime::Instance::new(&mut *caller, &module, &externs).map_err(|e| anyhow!("instantiate: {e:#}"))?;
    if n > 0 {
        let sb = bytes_of(caller, slots, n * 8)?;
        let nb = bytes_of(caller, names, 64 * n)?;
        let names: Vec<String> = nb.split(|&c| c == 0).take(n as usize).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
        // Resolve everything before installing anything: a failure installs nothing.
        let mut todo = Vec::new();
        for k in 0..n as usize {
            let slot = u64::from_le_bytes(sb[k * 8..k * 8 + 8].try_into().unwrap());
            let f = inst.get_func(&mut *caller, &names[k]).ok_or_else(|| anyhow!("no export {}", names[k]))?;
            todo.push((slot, f));
        }
        for (slot, f) in todo {
            table.set(&mut *caller, slot, wasmtime::Ref::Func(Some(f)))?;
            INSTALLED.with(|m| m.borrow_mut().insert(slot, f));
        }
    }
    caller.data_mut().jit.as_mut().unwrap().instances.push(inst);
    Ok(())
}

/// A module of `n` copies of the JIT-call placeholder (the browser's
/// `mono_jiterp_placeholder_jit_call`, `(ret_sp, sp, ftndesc, thrown)`,
/// which says "not here yet" by storing 999 through `thrown`), filling the
/// function table from `__table_base`: what [`crate::ftab::publish_pairs`]
/// installs in every store for a reservation.
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

/// The imports (64-bit memories only: the JIT runs in a wasm64 provider).
pub(crate) fn register64(linker: &mut Linker<HostState>) -> Result<()> {
    linker.func_wrap(
        IMPORT_MODULE,
        "provider_jit_instantiate",
        |mut caller: Caller<'_, HostState>,
         bytes: i64,
         len: i64,
         cnames: i64,
         consts: i64,
         nconsts: i64,
         slots: i64,
         names: i64,
         n: i64,
         errbuf: i64,
         errcap: i64| -> wasmtime::Result<i32> { crate::host((|| -> anyhow::Result<i32> {
            match instantiate(&mut caller, bytes, len, cnames, consts, nconsts, slots, names, n) {
                Ok(()) => {
                    if trace() {
                        eprintln!("gex-wasmjit: instantiated {len} bytes, {n} slots (thread {})", caller.data().thread.tid);
                    }
                    Ok(1)
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    if trace() {
                        eprintln!("gex-wasmjit: {msg}");
                    }
                    write_err(&caller, errbuf, errcap, &msg);
                    Ok(0)
                }
            }
        })()) },
    )?;
    linker.func_wrap(IMPORT_MODULE, "provider_jit_table", |mut caller: Caller<'_, HostState>, op: i32, a: i64, b: i64| -> wasmtime::Result<i64> { crate::host((|| -> anyhow::Result<i64> {
        crate::ftab::catch_up(&mut caller)?;
        let table = table_of(&caller)?;
        let (a, b) = (a as u64, b as u64);
        match op {
            // REAL: 1 if this store installed something at a (written again, which repairs any
            // overwrite: a Func has no identity to compare without unsafe code), 0 if nothing was.
            0 => {
                let Some(want) = INSTALLED.with(|m| m.borrow().get(&a).copied()) else { return Ok(0) };
                table.set(&mut caller, a, wasmtime::Ref::Func(Some(want)))?;
                Ok(1)
            }
            // ALIAS: table[a] = table[b].
            1 => {
                let f = table_get(&mut caller, table, b).ok_or_else(|| anyhow!("wasmjit: alias of an empty slot {b}"))?;
                table.set(&mut caller, a, wasmtime::Ref::Func(Some(f)))?;
                INSTALLED.with(|m| m.borrow_mut().insert(a, f));
                Ok(0)
            }
            // ARITY: table[a]'s parameter count, -1 if empty.
            2 => {
                let Some(f) = table_get(&mut caller, table, a) else { return Ok(-1) };
                Ok(f.ty(&caller).params().len() as i64)
            }
            _ => bail!("provider_jit_table: unknown op {op}"),
        }
    })()) })?;
    linker.func_wrap(IMPORT_MODULE, "provider_jit_reserve", |mut caller: Caller<'_, HostState>, n: i64, _fill: i64| -> wasmtime::Result<i64> { crate::host((|| -> anyhow::Result<i64> {
        let (shared, engine) = {
            let jit = caller.data().jit.as_ref().ok_or_else(|| anyhow!("wasmjit: no function table"))?;
            (Arc::clone(&jit.shared), jit.engine.clone())
        };
        let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
        let module = gex_cache::compile(&engine, &placeholder_module(n as u32))?;
        let install = crate::ftab::publish_pairs(&shared, &memory, module, n as u32, &[], gex_kernel::ftab::Tier::Unit)?;
        let base = shared.lock().unwrap_or_else(|e| e.into_inner()).ftab.map.install(install).base;
        crate::ftab::catch_up(&mut caller)?;
        if trace() {
            eprintln!("gex-wasmjit: reserved {n} table slots at {base}");
        }
        Ok(base as i64)
    })()) })?;
    Ok(())
}
