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
//! The probe is the one every emitter inlines: `fcache_slot(t)`'s entry,
//! hit when its low half is `t`, and index 0 otherwise.

use gex_abi::{ctx, ftab};
use wasm_encoder::{
    BlockType, CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection, ImportSection,
    Instruction as I, MemArg, MemoryType, Module, RefType, TableType, TypeSection, ValType,
};

/// Export names.
pub const RESOLVE: &str = "resolve";
pub const ROOT: &str = "root";

/// The `F_body` type, as every emitter declares it.
pub fn body_params() -> [ValType; 4] {
    [ValType::I32, ValType::I32, ValType::I32, ValType::I64]
}

/// The process's table, as a module imports it.
pub fn ftab_type() -> TableType {
    TableType { element_type: RefType::FUNCREF, minimum: 0, maximum: None, table64: false, shared: false }
}

/// The shared guest memory, as a module imports it.
pub fn memory_type() -> MemoryType {
    MemoryType { minimum: 0, maximum: Some(65536), memory64: false, shared: true, page_size_log2: None }
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

/// The runtime module's bytes.
pub fn module() -> Vec<u8> {
    let mut m = Module::new();
    let mut types = TypeSection::new();
    types.ty().function(body_params(), [ValType::I32]); // 0: F_body
    types.ty().function([ValType::I32, ValType::I64], [ValType::I32]); // 1: lookup
    types.ty().function([ValType::I32, ValType::I64], []); // 2: safepoint
    types.ty().function([ValType::I32], []); // 3: root
    m.section(&types);
    let mut imports = ImportSection::new();
    imports.import(gex_abi::IMPORT_MODULE, gex_abi::exports::MEMORY, EntityType::Memory(memory_type()));
    imports.import(gex_abi::IMPORT_MODULE, ftab::TABLE, EntityType::Table(ftab_type()));
    imports.import(gex_abi::IMPORT_MODULE, ftab::LOOKUP, EntityType::Function(1));
    imports.import(gex_abi::IMPORT_MODULE, gex_abi::imports::SAFEPOINT, EntityType::Function(2));
    m.section(&imports);
    const LOOKUP: u32 = 0;
    const SAFEPOINT: u32 = 1;
    let mut funcs = FunctionSection::new();
    funcs.function(0);
    funcs.function(3);
    m.section(&funcs);
    let mut exports = ExportSection::new();
    exports.export(RESOLVE, ExportKind::Func, 2);
    exports.export(ROOT, ExportKind::Func, 3);
    m.section(&exports);
    let ctxm = |off: usize, align: u32| MemArg { offset: off as u64, align, memory_index: 0 };

    let mut code = CodeSection::new();
    // resolve(ctx, depth, need, ret): return_call_indirect ftab[lookup(LAST_TARGET)].
    let mut f = Function::new([]);
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::LocalGet(2));
    f.instruction(&I::LocalGet(3));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::I64Load(ctxm(ctx::LAST_TARGET, 3)));
    f.instruction(&I::Call(LOOKUP));
    f.instruction(&I::ReturnCallIndirect { type_index: 0, table_index: 0 });
    f.instruction(&I::End);
    code.function(&f);

    // root(ctx). Locals: 1 t (i64), 2 e (i64), 3 sc (i32).
    let mut f = Function::new([(2, ValType::I64), (1, ValType::I32)]);
    f.instruction(&I::Loop(BlockType::Empty));
    // t = PC; LAST_TARGET = t
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::I64Load(ctxm(ctx::PC, 3)));
    f.instruction(&I::LocalSet(1));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::I64Store(ctxm(ctx::LAST_TARGET, 3)));
    // r = ftab[probe(t)](ctx, 0, NEED_ALL, NO_RETURN)
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::I32Const(0));
    f.instruction(&I::I32Const(ftab::NEED_ALL as i32));
    f.instruction(&I::I64Const(ftab::NO_RETURN as i64));
    emit_probe(&mut f, 1, 2);
    f.instruction(&I::CallIndirect { type_index: 0, table_index: 0 });
    // A predicted return to nowhere: say so in PC (the next lookup faults).
    f.instruction(&I::If(BlockType::Empty));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::I64Const(ftab::NO_RETURN as i64));
    f.instruction(&I::I64Store(ctxm(ctx::PC, 3)));
    f.instruction(&I::End);
    // The safepoint countdown, as tier-1's.
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::I32Load(ctxm(ctx::SAFEPOINT_COUNTER, 2)));
    f.instruction(&I::I32Const(1));
    f.instruction(&I::I32Sub);
    f.instruction(&I::LocalTee(3));
    f.instruction(&I::I32Store(ctxm(ctx::SAFEPOINT_COUNTER, 2)));
    f.instruction(&I::LocalGet(3));
    f.instruction(&I::I32Const(0));
    f.instruction(&I::I32LeS);
    f.instruction(&I::If(BlockType::Empty));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::I64Load(ctxm(ctx::PC, 3)));
    f.instruction(&I::Call(SAFEPOINT));
    f.instruction(&I::End);
    f.instruction(&I::Br(0));
    f.instruction(&I::End);
    f.instruction(&I::End);
    code.function(&f);
    m.section(&code);
    m.finish()
}

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_module_validates() {
        let w = super::module();
        let mut v = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
        v.validate_all(&w).expect("the runtime module validates");
    }
}
