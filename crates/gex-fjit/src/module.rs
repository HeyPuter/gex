//! A module of JIT-emitted functions.
//!
//! Its imports are tier-1's host functions by name and type, the guest
//! memory, and `gexc.dispatch` (the roots' tail call into dispatch), so a
//! whole-image module can be spliced into the tier-1 module built from the
//! same image (`gex_cgen::splice`, as the clang tier's is). Each function
//! `F` is
//!
//! * `f_F`, its body (`F_body`: `(ctx, depth, need, ret) -> i32`), and its
//!   loop functions;
//! * `e_F`, exported: the root dispatch reaches it at, `(ctx) -> ()`. It
//!   calls the body with no caller to predict and dispatches on `ctx::PC`.
//!
//! An indirect call goes through `ic`, a hash of the target and a
//! `br_table` over buckets of direct calls: no table, no data.

use crate::emit::{self, Emitted, ModuleCx, Stats, NEED_ALL};
use anyhow::Result;
use gex_abi::{ctx, exports, imports, IMPORT_MODULE};
use gex_cgen::func::{self as cf, Loop};
use gex_emit::tier1::Context;
use gex_emit::wasm::{ExportKind, FuncBody, FuncType, Module, ValType};
use gex_ir::func::{Function, Summary};
use std::collections::HashMap;

pub fn root_name(entry: u64) -> String {
    format!("e_{entry:x}")
}

/// Import indices and types of a JIT module.
struct Imports {
    syscall: u32,
    dispatch_miss: u32,
    illegal: u32,
    breakpoint: u32,
    sysreg_read: u32,
    sysreg_write: u32,
    cache_op: u32,
    safepoint: u32,
    fma: u32,
    x87: u32,
    dispatch: u32,
    block_ty: u32,
}

/// tier-1's host imports, by the names and types `gex_emit::module` gives
/// them (the splice checks each type against the tier-1 module's).
fn host_imports(m: &mut Module) -> Imports {
    let p = ValType::I32;
    let block_ty = m.func_type(FuncType::new(vec![p], vec![]));
    let t_syscall = m.func_type(FuncType::new(vec![p, ValType::I64], vec![]));
    let t_miss = m.func_type(FuncType::new(vec![p, ValType::I64], vec![ValType::I32]));
    let t_pc_i32 = m.func_type(FuncType::new(vec![p, ValType::I64, ValType::I32], vec![]));
    let t_sysreg_read = m.func_type(FuncType::new(vec![p, ValType::I64, ValType::I32], vec![ValType::I64]));
    let t_pc_i32_i64 = m.func_type(FuncType::new(vec![p, ValType::I64, ValType::I32, ValType::I64], vec![]));
    let t_fma = m.func_type(FuncType::new(vec![p, ValType::I64, ValType::I64, ValType::I64, ValType::I32], vec![ValType::I64]));
    let t_x87 = m.func_type(FuncType::new(
        vec![p, ValType::I32, ValType::I64, ValType::I32, ValType::I64, ValType::I32, ValType::I32],
        vec![ValType::I64],
    ));
    let im = IMPORT_MODULE;
    let out = Imports {
        syscall: m.import_func(im, imports::SYSCALL, t_syscall),
        dispatch_miss: m.import_func(im, imports::DISPATCH_MISS, t_miss),
        illegal: m.import_func(im, imports::ILLEGAL, t_pc_i32),
        breakpoint: m.import_func(im, imports::BREAKPOINT, t_pc_i32),
        sysreg_read: m.import_func(im, imports::SYSREG_READ, t_sysreg_read),
        sysreg_write: m.import_func(im, imports::SYSREG_WRITE, t_pc_i32_i64),
        cache_op: m.import_func(im, imports::CACHE_OP, t_pc_i32_i64),
        safepoint: m.import_func(im, imports::SAFEPOINT, t_syscall),
        fma: m.import_func(im, imports::FMA, t_fma),
        x87: m.import_func(im, imports::X87, t_x87),
        dispatch: m.import_func(gex_cgen::splice::DISPATCH_MODULE, gex_cgen::splice::DISPATCH_NAME, block_ty),
        block_ty,
    };
    m.import_memory(IMPORT_MODULE, exports::MEMORY, gex_abi::GUEST_MEM_INITIAL_PAGES, gex_abi::GUEST_MEM_PAGES, true);
    out
}

/// The module for `funcs` (memory form, verified), with the entries it
/// exports a root for, in order. Functions with an irreducible CFG are
/// left out (tier-1 keeps them).
pub fn build_module(
    funcs: &[Function],
    sums: &HashMap<u64, Summary>,
    class32: bool,
    stats: &mut Stats,
) -> Result<(Vec<u8>, Vec<u64>)> {
    let mut m = Module::new();
    let im = host_imports(&mut m);
    let body_ty = m.func_type(FuncType::new(vec![ValType::I32, ValType::I32, ValType::I32, ValType::I64], vec![ValType::I32]));
    let loop_ty = m.func_type(FuncType::new(
        vec![ValType::I32, ValType::I32, ValType::I32, ValType::I64, ValType::I64],
        vec![ValType::I32],
    ));
    let ic_ty = m.func_type(FuncType::new(
        vec![ValType::I32, ValType::I64, ValType::I32, ValType::I32, ValType::I64],
        vec![ValType::I32],
    ));
    let outline = cf::outline_loops();
    let loops: Vec<Vec<Loop>> = funcs.iter().map(|f| if outline { cf::outer_loops(f) } else { Vec::new() }).collect();
    let mut bodies: HashMap<u64, u32> = HashMap::new();
    let mut lfns: HashMap<(u64, usize), u32> = HashMap::new();
    let mut roots: Vec<(u64, u32)> = Vec::new();
    for (f, ls) in funcs.iter().zip(&loops) {
        bodies.insert(f.entry, m.declare_func(body_ty));
        for k in 0..ls.len() {
            lfns.insert((f.entry, k), m.declare_func(loop_ty));
        }
    }
    let ic = m.declare_func(ic_ty);
    let trapfree = cf::trap_free(funcs, sums);
    let no_blocks = HashMap::new();
    let no_gir = HashMap::new();
    let cx = Context {
        syscall: im.syscall,
        illegal: im.illegal,
        breakpoint: im.breakpoint,
        sysreg_read: im.sysreg_read,
        sysreg_write: im.sysreg_write,
        cache_op: im.cache_op,
        dispatch_miss: im.dispatch_miss,
        safepoint: im.safepoint,
        fma: im.fma,
        x87: im.x87,
        block_ty: im.block_ty,
        blocks: &no_blocks,
        gir: &no_gir,
        profile: None,
        page_bases: None,
        module_id: 0,
    };
    let mcx = ModuleCx {
        cx: &cx,
        bodies: &bodies,
        loops: &lfns,
        sums,
        trapfree: &trapfree,
        ret_sp_delta: cf::ret_sp_delta(class32),
        linkage: crate::emit::Linkage::Image { ic },
        body_ty,
    };
    let mut emitted: Vec<(u64, Emitted)> = Vec::new();
    for (f, ls) in funcs.iter().zip(&loops) {
        match emit::emit(&mcx, f, ls, stats) {
            Some(e) => emitted.push((f.entry, e)),
            None => {
                stats.dropped += 1;
                if std::env::var_os("GEX_FJIT_STATS").is_some() {
                    eprintln!("[fjit] {:#x}{}: a loop function is irreducible; left to tier-1", f.entry, f.name.as_deref().map(|n| format!(" ({n})")).unwrap_or_default());
                }
            }
        }
    }
    // A function left out is not ours after all: its callers must leave
    // for it rather than call a body that is not there. Rare (irreducible
    // control flow), so emit the rest again without it.
    if emitted.len() != funcs.len() {
        let keep: std::collections::HashSet<u64> = emitted.iter().map(|(a, _)| *a).collect();
        let rest: Vec<Function> = funcs.iter().filter(|f| keep.contains(&f.entry)).cloned().collect();
        let dropped = stats.dropped;
        *stats = Stats::default();
        let r = build_module(&rest, sums, class32, stats)?;
        stats.dropped = dropped;
        return Ok(r);
    }
    for (entry, e) in emitted {
        m.define_func(bodies[&entry], e.body);
        for (k, l) in e.loops.into_iter().enumerate() {
            m.define_func(lfns[&(entry, k)], l);
        }
        let root = m.declare_func(im.block_ty);
        m.define_func(root, root_body(&im, entry, bodies[&entry]));
        m.export(&root_name(entry), ExportKind::Func, root);
        roots.push((entry, root));
    }
    let entries: Vec<(u64, u32)> = funcs.iter().map(|f| (f.entry, bodies[&f.entry])).collect();
    m.define_func(ic, ic_body(&entries));
    // `GEX_CGEN_NAMES=1`: a name section (the splice carries it over), so an
    // engine profile (`node --cpu-prof`) says where the time goes.
    let wasm = if std::env::var_os("GEX_CGEN_NAMES").is_some() {
        let mut names: Vec<(u32, String)> = Vec::new();
        for f in funcs {
            names.push((bodies[&f.entry], format!("f_{:x}{}", f.entry, f.name.as_deref().map(|n| format!("_{n}")).unwrap_or_default())));
        }
        for (&(a, k), &i) in &lfns {
            names.push((i, format!("l_{a:x}_{k}")));
        }
        for &(a, i) in &roots {
            names.push((i, root_name(a)));
        }
        names.push((ic, "ic".into()));
        names.sort();
        let mut sub = Vec::new();
        gex_emit::wasm::write_u32(&mut sub, names.len() as u32);
        for (i, n) in &names {
            gex_emit::wasm::write_u32(&mut sub, *i);
            gex_emit::wasm::write_u32(&mut sub, n.len() as u32);
            sub.extend_from_slice(n.as_bytes());
        }
        let mut sec = vec![1u8];
        gex_emit::wasm::write_u32(&mut sec, sub.len() as u32);
        sec.extend_from_slice(&sub);
        m.encode_with_custom(&[("name", sec)])
    } else {
        m.encode()
    };
    Ok((wasm, roots.into_iter().map(|(a, _)| a).collect()))
}

/// `e_F(ctx)`: the nest root (function-tier.md §7). x86 first checks that
/// dispatch meant this entry (a slot covers four bytes), as tier-1's blocks
/// do; then the body, with no caller to predict, and dispatch on `ctx::PC`.
fn root_body(im: &Imports, entry: u64, body: u32) -> Vec<u8> {
    let mut f = FuncBody::new(1);
    let tgt = f.add_local(ValType::I64);
    let sc = f.add_local(ValType::I32);
    let dispatch = |f: &mut FuncBody| {
        if gex_abi::X86 {
            f.local_get(0);
            f.i64_const(entry as i64);
            f.store(0x37, 3, ctx::LAST_SOURCE as u32);
        }
        f.local_get(0);
        f.return_call(im.dispatch);
    };
    if gex_abi::DISPATCH_SHIFT > gex_abi::INST_ALIGN_SHIFT {
        f.local_get(0);
        f.load(0x29, 3, ctx::LAST_TARGET as u32);
        f.i64_const(entry as i64);
        f.byte(0x52); // i64.ne
        f.if_void();
        f.local_get(0);
        f.i64_const(entry as i64);
        f.i32_const(gex_abi::cache_op::SLOT_CONFLICT);
        f.local_get(0);
        f.load(0x29, 3, ctx::LAST_TARGET as u32);
        f.call(im.cache_op);
        f.local_get(0);
        f.local_get(0);
        f.load(0x29, 3, ctx::LAST_TARGET as u32);
        f.call(im.dispatch_miss);
        f.drop_();
        dispatch(&mut f);
        f.end();
    }
    // No caller to predict: `ret` is an address no code has, so a return
    // writes everything back and sets PC.
    f.local_get(0);
    f.i32_const(0);
    f.i32_const(NEED_ALL);
    f.i64_const(-1);
    f.call(body);
    f.if_void();
    f.local_get(0);
    f.i64_const(-1);
    f.store(0x37, 3, ctx::PC as u32);
    f.end();
    f.local_get(0);
    f.load(0x29, 3, ctx::PC as u32);
    f.local_set(tgt);
    // tier-1's safepoint countdown, then dispatch on the target.
    f.local_get(0);
    f.local_get(0);
    f.load(0x28, 2, ctx::SAFEPOINT_COUNTER as u32);
    f.i32_const(1);
    f.byte(0x6b); // i32.sub
    f.local_tee(sc);
    f.store(0x36, 2, ctx::SAFEPOINT_COUNTER as u32);
    f.local_get(sc);
    f.i32_const(0);
    f.byte(0x4c); // i32.le_s
    f.if_void();
    f.local_get(0);
    f.local_get(tgt);
    f.call(im.safepoint);
    f.end();
    f.local_get(0);
    f.local_get(tgt);
    f.store(0x37, 3, ctx::LAST_TARGET as u32);
    dispatch(&mut f);
    f.finish()
}

/// `ic(ctx, target, depth, need, ret)`: the body for `target` among
/// `entries` (entry, body index), tail-called; -1 for none. A
/// multiplicative hash picks a bucket through one `br_table`, and a bucket
/// compares its few entries.
fn ic_body(entries: &[(u64, u32)]) -> Vec<u8> {
    let mut f = FuncBody::new(5);
    let bits = (entries.len() * 2).next_power_of_two().trailing_zeros().clamp(1, 12);
    let nb = 1usize << bits;
    let mut buckets: Vec<Vec<(u64, u32)>> = vec![Vec::new(); nb];
    for &(a, b) in entries {
        buckets[bucket(a, bits)].push((a, b));
    }
    for _ in 0..=nb {
        f.block_void();
    }
    // Inside the innermost block, bucket k's label is k deep, the miss nb.
    // bucket(t), in 32-bit arithmetic (`bucket`).
    let t32 = f.add_local(ValType::I32);
    f.local_get(1);
    f.byte(0xa7); // i32.wrap_i64
    f.local_tee(t32);
    f.i32_const(gex_abi::INST_ALIGN_SHIFT as i32);
    f.byte(0x76); // i32.shr_u
    f.local_get(t32);
    f.i32_const((gex_abi::INST_ALIGN_SHIFT + bits) as i32);
    f.byte(0x76); // i32.shr_u
    f.byte(0x73); // i32.xor
    f.i32_const((nb - 1) as i32);
    f.byte(0x71); // i32.and
    let targets: Vec<u32> = (0..nb as u32).collect();
    f.br_table(&targets, nb as u32);
    for b in &buckets {
        f.end();
        for &(a, body) in b {
            f.local_get(1);
            f.i64_const(a as i64);
            f.byte(0x51); // i64.eq
            f.if_void();
            f.local_get(0);
            f.local_get(2);
            f.local_get(3);
            f.local_get(4);
            f.return_call(body);
            f.end();
        }
        f.i32_const(-1);
        f.ret();
    }
    f.end();
    f.i32_const(-1);
    f.finish()
}

/// An address's bucket: its instruction index, folded with the bits above
/// it (a shift, a shift, a xor, a mask: cheaper than a multiply on the
/// call path).
fn bucket(a: u64, bits: u32) -> usize {
    let t = a as u32;
    (((t >> gex_abi::INST_ALIGN_SHIFT) ^ (t >> (gex_abi::INST_ALIGN_SHIFT + bits))) & ((1u32 << bits) - 1)) as usize
}

/// The module of a run-time translation (docs/merge-plan.md §9.8.1): `funcs`
/// (memory form, prepared), installed in the process's function table. It
/// imports the guest memory, `gex.ftab`, `env.__table_base` and tier-1's
/// host functions; one active element segment puts each function's body at
/// `__table_base + k` in the order of `gex.funcs`, which lists their guest
/// addresses. Calls inside the module are `call`s, anything else goes
/// through the table. `pages` are the `PAGE_GEN` values each function
/// checks on entry, by entry. Functions whose loops would not structure
/// are left out (the caller gets the entries that are in).
pub fn build_runtime(
    funcs: &[Function],
    sums: &HashMap<u64, Summary>,
    class32: bool,
    pages: &HashMap<u64, Vec<(u64, u32)>>,
    stats: &mut Stats,
) -> Result<(Vec<u8>, Vec<u64>)> {
    use wasm_encoder::{
        CodeSection, ConstExpr, CustomSection, ElementSection, Elements, EntityType, FunctionSection, GlobalType,
        ImportSection, MemoryType, RefType, TableType, TypeSection, ValType as V,
    };
    let outline = cf::outline_loops();
    let mut funcs: Vec<Function> = funcs.to_vec();
    loop {
        let loops: Vec<Vec<Loop>> = funcs.iter().map(|f| if outline { cf::outer_loops(f) } else { Vec::new() }).collect();
        // Imports: tier-1's ten host functions, in `host_imports`' order.
        const N_IMPORTS: u32 = 10;
        let (block_ty, t_syscall, t_miss, t_pc_i32, t_sysreg_read, t_pc_i32_i64, t_fma, t_x87, body_ty, loop_ty) =
            (0u32, 1, 2, 3, 4, 5, 6, 7, 8, 9);
        let mut next = N_IMPORTS;
        let mut bodies: HashMap<u64, u32> = HashMap::new();
        let mut lfns: HashMap<(u64, usize), u32> = HashMap::new();
        let mut order: Vec<(u32, u32)> = Vec::new(); // (function index, type)
        for (f, ls) in funcs.iter().zip(&loops) {
            bodies.insert(f.entry, next);
            order.push((next, body_ty));
            next += 1;
            for k in 0..ls.len() {
                lfns.insert((f.entry, k), next);
                order.push((next, loop_ty));
                next += 1;
            }
        }
        let trapfree = cf::trap_free(&funcs, sums);
        let no_blocks = HashMap::new();
        let no_gir = HashMap::new();
        let cx = Context {
            syscall: 0,
            dispatch_miss: 1,
            illegal: 2,
            breakpoint: 3,
            sysreg_read: 4,
            sysreg_write: 5,
            cache_op: 6,
            safepoint: 7,
            fma: 8,
            x87: 9,
            block_ty,
            blocks: &no_blocks,
            gir: &no_gir,
            profile: None,
            page_bases: None,
            module_id: 0,
        };
        let mcx = ModuleCx {
            cx: &cx,
            bodies: &bodies,
            loops: &lfns,
            sums,
            trapfree: &trapfree,
            ret_sp_delta: cf::ret_sp_delta(class32),
            linkage: crate::emit::Linkage::Runtime { table: 0, pages },
            body_ty,
        };
        let mut emitted: Vec<(u64, Emitted)> = Vec::new();
        let mut dropped = Vec::new();
        for (f, ls) in funcs.iter().zip(&loops) {
            match emit::emit(&mcx, f, ls, stats) {
                Some(e) => emitted.push((f.entry, e)),
                None => dropped.push(f.entry),
            }
        }
        if !dropped.is_empty() {
            stats.dropped += dropped.len();
            funcs.retain(|f| !dropped.contains(&f.entry));
            if funcs.is_empty() {
                anyhow::bail!("no function of the translation structures");
            }
            continue;
        }
        let mut m = wasm_encoder::Module::new();
        let mut types = TypeSection::new();
        let i32_ = V::I32;
        let i64_ = V::I64;
        types.ty().function([i32_], []);
        types.ty().function([i32_, i64_], []);
        types.ty().function([i32_, i64_], [i32_]);
        types.ty().function([i32_, i64_, i32_], []);
        types.ty().function([i32_, i64_, i32_], [i64_]);
        types.ty().function([i32_, i64_, i32_, i64_], []);
        types.ty().function([i32_, i64_, i64_, i64_, i32_], [i64_]);
        types.ty().function([i32_, i32_, i64_, i32_, i64_, i32_, i32_], [i64_]);
        types.ty().function([i32_, i32_, i32_, i64_], [i32_]);
        types.ty().function([i32_, i32_, i32_, i64_, i64_], [i32_]);
        m.section(&types);
        let mut imports = ImportSection::new();
        let im = gex_abi::IMPORT_MODULE;
        for (name, ty) in [
            (imports::SYSCALL, t_syscall),
            (imports::DISPATCH_MISS, t_miss),
            (imports::ILLEGAL, t_pc_i32),
            (imports::BREAKPOINT, t_pc_i32),
            (imports::SYSREG_READ, t_sysreg_read),
            (imports::SYSREG_WRITE, t_pc_i32_i64),
            (imports::CACHE_OP, t_pc_i32_i64),
            (imports::SAFEPOINT, t_syscall),
            (imports::FMA, t_fma),
            (imports::X87, t_x87),
        ] {
            imports.import(im, name, EntityType::Function(ty));
        }
        imports.import(
            im,
            exports::MEMORY,
            EntityType::Memory(MemoryType { minimum: 0, maximum: Some(65536), memory64: false, shared: true, page_size_log2: None }),
        );
        imports.import(
            im,
            gex_abi::ftab::TABLE,
            EntityType::Table(TableType { element_type: RefType::FUNCREF, minimum: 0, maximum: None, table64: false, shared: false }),
        );
        imports.import(
            gex_abi::ftab::PIC_MODULE,
            gex_abi::ftab::PIC_TABLE_BASE,
            EntityType::Global(GlobalType { val_type: i32_, mutable: false, shared: false }),
        );
        m.section(&imports);
        let mut fsec = FunctionSection::new();
        for &(_, ty) in &order {
            fsec.function(ty);
        }
        m.section(&fsec);
        let entries: Vec<u64> = funcs.iter().map(|f| f.entry).collect();
        let slots: Vec<u32> = entries.iter().map(|a| bodies[a]).collect();
        let mut elems = ElementSection::new();
        elems.active(Some(0), &ConstExpr::global_get(0), Elements::Functions(std::borrow::Cow::Borrowed(&slots)));
        m.section(&elems);
        let mut by_index: HashMap<u32, Vec<u8>> = HashMap::new();
        for (entry, e) in emitted {
            by_index.insert(bodies[&entry], e.body);
            for (k, l) in e.loops.into_iter().enumerate() {
                by_index.insert(lfns[&(entry, k)], l);
            }
        }
        let mut code = CodeSection::new();
        for &(i, _) in &order {
            code.raw(strip_size(&by_index[&i]));
        }
        m.section(&code);
        let mut desc = (entries.len() as u32).to_le_bytes().to_vec();
        for a in &entries {
            desc.extend_from_slice(&a.to_le_bytes());
        }
        m.section(&CustomSection { name: gex_abi::ftab::FUNCS_SECTION.into(), data: desc.into() });
        if std::env::var_os("GEX_CGEN_NAMES").is_some() {
            let mut names = wasm_encoder::NameMap::new();
            let mut all: Vec<(u32, String)> = funcs.iter().map(|f| (bodies[&f.entry], format!("f_{:x}", f.entry))).collect();
            all.extend(lfns.iter().map(|(&(a, k), &i)| (i, format!("l_{a:x}_{k}"))));
            all.sort();
            for (i, n) in &all {
                names.append(*i, n);
            }
            let mut ns = wasm_encoder::NameSection::new();
            ns.functions(&names);
            m.section(&ns);
        }
        return Ok((m.finish(), entries));
    }
}

/// A body as `FuncBody::finish` writes it, without its size prefix.
fn strip_size(body: &[u8]) -> &[u8] {
    let mut at = 0;
    while body[at] & 0x80 != 0 {
        at += 1;
    }
    &body[at + 1..]
}
