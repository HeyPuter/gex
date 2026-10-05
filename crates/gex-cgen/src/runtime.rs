//! The function table's runtime functions (docs/merge-plan.md §9.8): a small
//! module every store (every Worker) instantiates once, against the process's
//! memory and its own `gex.ftab`.
//!
//! * `resolve`, the `F_body` at index 0 ([`gex_abi::ftab::RESOLVE`]): reads
//!   `ctx::LAST_TARGET`, asks `gex.lookup` for the index (which translates on
//!   demand and writes the probe cache), and tail-calls it with its own
//!   arguments. It is also what every slot a table has not filled yet holds,
//!   and what a retired slot is pointed back at, so a call that finds either
//!   still arrives.
//! * `root(ctx)`, the run loop: `call_indirect ftab[probe(PC)]` with no
//!   predicted return, and when that comes back (a function left, `ctx::PC`
//!   says where), poll the safepoint countdown and go round again. It never
//!   returns: the host stops it by trapping out, as tier-1's `enter`.
//!
//! * `bridge(ctx)`, what tier-1's dispatch slot holds for an address a
//!   function body serves (while tier-1 is the fallback, §9.4 step 3):
//!   `(ctx) -> ()` like a block, it calls the function for
//!   `ctx::LAST_TARGET` with no predicted return, then dispatches on
//!   `ctx::PC` as the old nest roots did. An address in the slot that no
//!   function serves (x86 slots cover four bytes) is a slot conflict, handed
//!   to tier-1's miss path exactly as a root's target check did.
//!
//! The probe is the one every emitter inlines: `fcache_slot(t)`'s entry,
//! hit when its low half is `t`, and index 0 otherwise.

use gex_abi::{ctx, ftab};
use wasm_encoder::Instruction;
use wasm_encoder::{
    BlockType, CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection, ImportSection,
    Instruction as I, MemArg, MemoryType, Module, RefType, TableType, TypeSection, ValType,
};

/// Export names.
pub const RESOLVE: &str = "resolve";
pub const ROOT: &str = "root";
pub const BRIDGE: &str = "bridge";

/// The `F_body` type, as every emitter declares it.
pub fn body_params() -> [ValType; 4] {
    [ValType::I32, ValType::I32, ValType::I32, ValType::I64]
}

/// The process's table, as a module imports it: a table64 in a process with
/// a memory64 guest memory, as clang's wasm64 units import it
/// (docs/mem64.md §4).
pub fn ftab_type() -> TableType {
    TableType { element_type: RefType::FUNCREF, minimum: 0, maximum: None, table64: gex_abi::mem64(), shared: false }
}

/// A tier-1 dispatch table, as a module imports it: 32-bit in every process.
pub fn dispatch_table_type() -> TableType {
    TableType { element_type: RefType::FUNCREF, minimum: 0, maximum: None, table64: false, shared: false }
}

/// The shared guest memory, as a module imports it.
pub fn memory_type() -> MemoryType {
    if gex_abi::mem64() {
        MemoryType { minimum: 0, maximum: Some(gex_abi::LIMIT64_PAGES), memory64: true, shared: true, page_size_log2: None }
    } else {
        MemoryType { minimum: 0, maximum: Some(65536), memory64: false, shared: true, page_size_log2: None }
    }
}

/// `env.__table_base`'s type: i64 beside a table64 `ftab`, as wasm64 PIC
/// modules (clang's units) import it.
pub fn table_base_type() -> ValType {
    if gex_abi::mem64() {
        ValType::I64
    } else {
        ValType::I32
    }
}

/// Make the i32 `ftab` index on the stack the table's index type.
pub fn ftab_index(f: &mut Function) {
    if gex_abi::mem64() {
        f.instruction(&Instruction::I64ExtendI32U);
    }
}

/// Push guest address 0 as the memory's index type (a base for an
/// absolute offset).
fn addr_zero(f: &mut Function) {
    if gex_abi::mem64() {
        f.instruction(&Instruction::I64Const(0));
    } else {
        f.instruction(&Instruction::I32Const(0));
    }
}

/// Push the probe for the target in local `t` (i64): leaves the index on the
/// stack (0 on a miss). Uses i64 local `e`.
pub fn emit_probe(f: &mut Function, t: u32, e: u32) {
    let m = MemArg { offset: gex_abi::FCACHE, align: 3, memory_index: 0 };
    // e = cache[(t32 * MUL) >> (32 - BITS)]
    f.instruction(&I::LocalGet(t));
    f.instruction(&I::I32WrapI64);
    f.instruction(&I::I32Const(gex_abi::FCACHE_MUL as i32));
    f.instruction(&I::I32Mul);
    f.instruction(&I::I32Const(32 - gex_abi::FCACHE_BITS as i32));
    f.instruction(&I::I32ShrU);
    f.instruction(&I::I32Const(3));
    f.instruction(&I::I32Shl);
    crate::splice::widen_addr(f);
    f.instruction(&I::I64Load(m));
    f.instruction(&I::LocalTee(e));
    // hit ? e >> 32 : 0, where hit compares the whole target with the low
    // half (a target past 4 GiB never hits).
    f.instruction(&I::I64Const(32));
    f.instruction(&I::I64ShrU);
    f.instruction(&I::I32WrapI64);
    f.instruction(&I::I32Const(ftab::RESOLVE as i32));
    f.instruction(&I::LocalGet(e));
    f.instruction(&I::I64Const(0xffff_ffff));
    f.instruction(&I::I64And);
    f.instruction(&I::LocalGet(t));
    f.instruction(&I::I64Eq);
    f.instruction(&I::Select);
}

/// The runtime module's bytes. Imports, in order: the memory, `gex.ftab`
/// (table 0), the dispatch tables (tables 1..), `gex.lookup`,
/// `gex.safepoint`, `gex.cache_op`, `gex.dispatch_miss`.
pub fn module() -> Vec<u8> {
    let mut m = Module::new();
    let mut types = TypeSection::new();
    types.ty().function(body_params(), [ValType::I32]); // 0: F_body
    types.ty().function([ValType::I32, ValType::I64], [ValType::I32]); // 1: lookup, dispatch_miss
    types.ty().function([ValType::I32, ValType::I64], []); // 2: safepoint
    types.ty().function([ValType::I32], []); // 3: root, bridge, a block
    types.ty().function([ValType::I32, ValType::I64, ValType::I32, ValType::I64], []); // 4: cache_op
    m.section(&types);
    let mut imports = ImportSection::new();
    imports.import(gex_abi::IMPORT_MODULE, gex_abi::exports::MEMORY, EntityType::Memory(memory_type()));
    imports.import(gex_abi::IMPORT_MODULE, ftab::TABLE, EntityType::Table(ftab_type()));
    for i in 0..gex_abi::DISPATCH_TABLES {
        imports.import(gex_abi::IMPORT_MODULE, gex_abi::exports::dispatch_table(i), EntityType::Table(dispatch_table_type()));
    }
    imports.import(gex_abi::IMPORT_MODULE, ftab::LOOKUP, EntityType::Function(1));
    imports.import(gex_abi::IMPORT_MODULE, gex_abi::imports::SAFEPOINT, EntityType::Function(2));
    imports.import(gex_abi::IMPORT_MODULE, gex_abi::imports::CACHE_OP, EntityType::Function(4));
    imports.import(gex_abi::IMPORT_MODULE, gex_abi::imports::DISPATCH_MISS, EntityType::Function(1));
    m.section(&imports);
    const LOOKUP: u32 = 0;
    const SAFEPOINT: u32 = 1;
    const CACHE_OP: u32 = 2;
    const DISPATCH_MISS: u32 = 3;
    const DISPATCH: u32 = 7;
    let mut funcs = FunctionSection::new();
    funcs.function(0); // 4 resolve
    funcs.function(3); // 5 root
    funcs.function(3); // 6 bridge
    funcs.function(3); // 7 dispatch
    m.section(&funcs);
    let mut exports = ExportSection::new();
    exports.export(RESOLVE, ExportKind::Func, 4);
    exports.export(ROOT, ExportKind::Func, 5);
    exports.export(BRIDGE, ExportKind::Func, 6);
    m.section(&exports);
    let ctxm = |off: usize, align: u32| MemArg { offset: off as u64, align, memory_index: 0 };
    // ctx = local 0; the safepoint countdown, as tier-1's, polling at PC.
    let poll = |f: &mut Function, sc: u32| {
        f.instruction(&I::LocalGet(0));
        crate::splice::widen_addr(f);
        f.instruction(&I::LocalGet(0));
        crate::splice::widen_addr(f);
        f.instruction(&I::I32Load(ctxm(ctx::SAFEPOINT_COUNTER, 2)));
        f.instruction(&I::I32Const(1));
        f.instruction(&I::I32Sub);
        f.instruction(&I::LocalTee(sc));
        f.instruction(&I::I32Store(ctxm(ctx::SAFEPOINT_COUNTER, 2)));
        f.instruction(&I::LocalGet(sc));
        f.instruction(&I::I32Const(0));
        f.instruction(&I::I32LeS);
        f.instruction(&I::If(BlockType::Empty));
        f.instruction(&I::LocalGet(0));
        f.instruction(&I::LocalGet(0));
        crate::splice::widen_addr(f);
        f.instruction(&I::I64Load(ctxm(ctx::PC, 3)));
        f.instruction(&I::Call(SAFEPOINT));
        f.instruction(&I::End);
    };
    // A predicted return to nowhere: say so in PC (the next lookup faults).
    let no_return = |f: &mut Function| {
        f.instruction(&I::If(BlockType::Empty));
        f.instruction(&I::LocalGet(0));
        crate::splice::widen_addr(f);
        f.instruction(&I::I64Const(ftab::NO_RETURN as i64));
        f.instruction(&I::I64Store(ctxm(ctx::PC, 3)));
        f.instruction(&I::End);
    };
    // Call ftab[index on the stack] as a root: no caller to predict.
    let call_root = |f: &mut Function| {
        ftab_index(f);
        f.instruction(&I::CallIndirect { type_index: 0, table_index: 0 });
    };

    let mut code = CodeSection::new();
    // resolve(ctx, depth, need, ret): return_call_indirect ftab[lookup(LAST_TARGET)].
    let mut f = Function::new([]);
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::LocalGet(2));
    f.instruction(&I::LocalGet(3));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::LocalGet(0));
    crate::splice::widen_addr(&mut f);
    f.instruction(&I::I64Load(ctxm(ctx::LAST_TARGET, 3)));
    f.instruction(&I::Call(LOOKUP));
    ftab_index(&mut f);
    f.instruction(&I::ReturnCallIndirect { type_index: 0, table_index: 0 });
    f.instruction(&I::End);
    code.function(&f);

    // root(ctx). Locals: 1 t (i64), 2 e (i64), 3 sc (i32).
    let mut f = Function::new([(2, ValType::I64), (1, ValType::I32)]);
    f.instruction(&I::Loop(BlockType::Empty));
    f.instruction(&I::LocalGet(0));
    crate::splice::widen_addr(&mut f);
    f.instruction(&I::I64Load(ctxm(ctx::PC, 3)));
    f.instruction(&I::LocalSet(1));
    f.instruction(&I::LocalGet(0));
    crate::splice::widen_addr(&mut f);
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::I64Store(ctxm(ctx::LAST_TARGET, 3)));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::I32Const(0));
    f.instruction(&I::I32Const(ftab::NEED_ALL as i32));
    f.instruction(&I::I64Const(ftab::NO_RETURN as i64));
    emit_probe(&mut f, 1, 2);
    call_root(&mut f);
    no_return(&mut f);
    poll(&mut f, 3);
    f.instruction(&I::Br(0));
    f.instruction(&I::End);
    f.instruction(&I::End);
    code.function(&f);

    // bridge(ctx). Locals: 1 t (i64), 2 e (i64), 3 idx (i32), 4 sc (i32).
    let mut f = Function::new([(2, ValType::I64), (2, ValType::I32)]);
    f.instruction(&I::LocalGet(0));
    crate::splice::widen_addr(&mut f);
    f.instruction(&I::I64Load(ctxm(ctx::LAST_TARGET, 3)));
    f.instruction(&I::LocalSet(1));
    emit_probe(&mut f, 1, 2);
    f.instruction(&I::LocalTee(3));
    f.instruction(&I::I32Eqz);
    f.instruction(&I::If(BlockType::Empty));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::Call(LOOKUP));
    f.instruction(&I::LocalSet(3));
    f.instruction(&I::End);
    // No function for this address: a neighbour in the slot. Tier-1's miss
    // path, as a nest root's target check did.
    f.instruction(&I::LocalGet(3));
    f.instruction(&I::I32Const(ftab::FALLBACK as i32));
    f.instruction(&I::I32Eq);
    f.instruction(&I::If(BlockType::Empty));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::I32Const(gex_abi::cache_op::SLOT_CONFLICT));
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::Call(CACHE_OP));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::Call(DISPATCH_MISS));
    f.instruction(&I::Drop);
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::ReturnCall(DISPATCH));
    f.instruction(&I::End);
    // NEST_EXITS += 1 (a statistic, not atomic); every 4096th says where
    // it went once the function is back (`cache_op::NEST_EXIT_SAMPLE`).
    let abs = |off: u64| MemArg { offset: off, align: 3, memory_index: 0 };
    addr_zero(&mut f);
    addr_zero(&mut f);
    f.instruction(&I::I64Load(abs(gex_abi::NEST_EXITS)));
    f.instruction(&I::LocalTee(2));
    f.instruction(&I::I64Const(1));
    f.instruction(&I::I64Add);
    f.instruction(&I::I64Store(abs(gex_abi::NEST_EXITS)));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::I32Const(0));
    f.instruction(&I::I32Const(ftab::NEED_ALL as i32));
    f.instruction(&I::I64Const(ftab::NO_RETURN as i64));
    f.instruction(&I::LocalGet(3));
    call_root(&mut f);
    no_return(&mut f);
    f.instruction(&I::LocalGet(2));
    f.instruction(&I::I64Const(4095));
    f.instruction(&I::I64And);
    f.instruction(&I::I64Eqz);
    f.instruction(&I::If(BlockType::Empty));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::I32Const(gex_abi::cache_op::NEST_EXIT_SAMPLE));
    f.instruction(&I::LocalGet(0));
    crate::splice::widen_addr(&mut f);
    f.instruction(&I::I64Load(ctxm(ctx::PC, 3)));
    f.instruction(&I::Call(CACHE_OP));
    f.instruction(&I::End);
    // Left for this very address, having run nothing: no body serves it
    // (index 1 behind a translation, say). Tier-1 takes the slot over.
    f.instruction(&I::LocalGet(0));
    crate::splice::widen_addr(&mut f);
    f.instruction(&I::I64Load(ctxm(ctx::PC, 3)));
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::I64Eq);
    f.instruction(&I::If(BlockType::Empty));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::I32Const(gex_abi::cache_op::FTAB_TIER1));
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::Call(CACHE_OP));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::Call(DISPATCH_MISS));
    f.instruction(&I::Drop);
    f.instruction(&I::LocalGet(0));
    crate::splice::widen_addr(&mut f);
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::I64Store(ctxm(ctx::LAST_TARGET, 3)));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::ReturnCall(DISPATCH));
    f.instruction(&I::End);
    poll(&mut f, 4);
    f.instruction(&I::LocalGet(0));
    crate::splice::widen_addr(&mut f);
    f.instruction(&I::LocalGet(0));
    crate::splice::widen_addr(&mut f);
    f.instruction(&I::I64Load(ctxm(ctx::PC, 3)));
    f.instruction(&I::I64Store(ctxm(ctx::LAST_TARGET, 3)));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::ReturnCall(DISPATCH));
    f.instruction(&I::End);
    code.function(&f);

    // dispatch(ctx): tier-1's indirect dispatch on LAST_TARGET.
    code.function(&crate::splice::dispatch_function_at(3, 1));
    m.section(&code);
    m.finish()
}

/// A module in the shape every run-time translation has (§9.8.1), as a
/// reference and for testing the host's half: one `F_body` per address of
/// `addrs`, each re-entering the table's fallback (index 1) with
/// `ctx::LAST_TARGET` = its address; its element segment fills the table
/// from the imported `env.__table_base`, and `gex.funcs` lists the
/// addresses. Anything that runs through it runs exactly as it would on the
/// fallback, one call deeper.
pub fn forwarder_module(addrs: &[u64]) -> Vec<u8> {
    use wasm_encoder::{ConstExpr, CustomSection, ElementSection, Elements, GlobalType};
    let mut m = Module::new();
    let mut types = TypeSection::new();
    types.ty().function(body_params(), [ValType::I32]);
    m.section(&types);
    let mut imports = ImportSection::new();
    imports.import(gex_abi::IMPORT_MODULE, gex_abi::exports::MEMORY, EntityType::Memory(memory_type()));
    imports.import(gex_abi::IMPORT_MODULE, ftab::TABLE, EntityType::Table(ftab_type()));
    imports.import(
        ftab::PIC_MODULE,
        ftab::PIC_TABLE_BASE,
        EntityType::Global(GlobalType { val_type: table_base_type(), mutable: false, shared: false }),
    );
    m.section(&imports);
    let mut funcs = FunctionSection::new();
    for _ in addrs {
        funcs.function(0);
    }
    m.section(&funcs);
    let mut elems = ElementSection::new();
    let idx: Vec<u32> = (0..addrs.len() as u32).collect();
    elems.active(Some(0), &ConstExpr::global_get(0), Elements::Functions(std::borrow::Cow::Borrowed(&idx)));
    m.section(&elems);
    let mut code = CodeSection::new();
    for &a in addrs {
        let mut f = Function::new([]);
        f.instruction(&I::LocalGet(0));
        crate::splice::widen_addr(&mut f);
        f.instruction(&I::I64Const(a as i64));
        f.instruction(&I::I64Store(MemArg { offset: ctx::LAST_TARGET as u64, align: 3, memory_index: 0 }));
        for i in 0..4 {
            f.instruction(&I::LocalGet(i));
        }
        f.instruction(&I::I32Const(ftab::FALLBACK as i32));
        ftab_index(&mut f);
        f.instruction(&I::ReturnCallIndirect { type_index: 0, table_index: 0 });
        f.instruction(&I::End);
        code.function(&f);
    }
    m.section(&code);
    let mut desc = (addrs.len() as u32).to_le_bytes().to_vec();
    for a in addrs {
        desc.extend_from_slice(&a.to_le_bytes());
    }
    m.section(&CustomSection { name: ftab::FUNCS_SECTION.into(), data: desc.into() });
    m.finish()
}

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_module_validates() {
        let w = super::module();
        let mut v = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
        v.validate_all(&w).expect("the runtime module validates");
        let w = super::forwarder_module(&[0x1000, 0x2000]);
        v.reset();
        v.validate_all(&w).expect("the forwarder module validates");
        assert_eq!(gex_kernel_funcs(&w), vec![0x1000, 0x2000]);
    }

    /// `gex_kernel::ftab::module_funcs`, which gex-cgen cannot depend on.
    fn gex_kernel_funcs(w: &[u8]) -> Vec<u64> {
        let p = wasmparser::Parser::new(0);
        for pl in p.parse_all(w) {
            if let Ok(wasmparser::Payload::CustomSection(c)) = pl {
                if c.name() == gex_abi::ftab::FUNCS_SECTION {
                    let d = c.data();
                    let n = u32::from_le_bytes(d[..4].try_into().unwrap()) as usize;
                    return (0..n).map(|k| u64::from_le_bytes(d[4 + 8 * k..12 + 8 * k].try_into().unwrap())).collect();
                }
            }
        }
        Vec::new()
    }
}
