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
        ic,
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
    f.local_get(1);
    f.i64_const(HASH as i64);
    f.byte(0x7e); // i64.mul
    f.i64_const(64 - bits as i64);
    f.byte(0x88); // i64.shr_u
    f.byte(0xa7); // i32.wrap_i64
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

const HASH: u64 = 0x9e37_79b9_7f4a_7c15;

fn bucket(a: u64, bits: u32) -> usize {
    (a.wrapping_mul(HASH) >> (64 - bits)) as usize
}
