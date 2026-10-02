//! Splicing a clang-built module into the tier-1 module built from the same
//! blocks (`docs/function-tier.md` §1).
//!
//! The tier-1 module keeps everything the hosts rely on -- imports, exports,
//! tables, element layout, data, `gex.meta` -- and gains the C side's
//! functions. Each block the C side owns is retargeted: its tier-1 body
//! becomes `return_call $c`, and element entries naming it name `$c`. The one
//! synthetic import the C side uses, `gexc.dispatch`, becomes a local copy of
//! tier-1's indirect-dispatch path.

use anyhow::{anyhow, bail, Context, Result};
use gex_abi::ctx;
use std::collections::{BTreeSet, HashMap};
use wasm_encoder::reencode::{Reencode, RoundtripReencoder};
use wasm_encoder::{
    BlockType, CodeSection, CustomSection, ElementSection, Function, FunctionSection, Instruction,
    MemArg, Module, RawSection, TypeSection, ValType,
};
use wasmparser::{ExternalKind, Operator, Parser, Payload, TypeRef};

/// Import module and name of the synthetic dispatch function C tail-calls.
pub const DISPATCH_MODULE: &str = "gexc";
pub const DISPATCH_NAME: &str = "dispatch";

/// What the splice did, for the build's report.
#[derive(Debug, Default, Clone)]
pub struct SpliceStats {
    pub owned: usize,
    pub c_functions: usize,
    pub c_code_bytes: usize,
}

/// The parts of the clang module the splice needs.
struct CModule<'a> {
    types: Vec<wasmparser::FuncType>,
    /// Function imports, in order: `(module, name, type)`.
    func_imports: Vec<(String, String, u32)>,
    /// Type index of each defined function.
    funcs: Vec<u32>,
    bodies: Vec<wasmparser::FunctionBody<'a>>,
    exports: HashMap<String, u32>,
    /// Function names from wasm-ld's name section, when it kept one.
    names: HashMap<u32, String>,
}

fn parse_c(wasm: &[u8]) -> Result<CModule<'_>> {
    parse_c_as(wasm, false)
}

/// [`parse_c`]; `image`: a whole-image unit (docs/function-tier.md §11),
/// which has a function table of its own holding its entries.
fn parse_c_as(wasm: &[u8], image: bool) -> Result<CModule<'_>> {
    let mut m = CModule {
        types: Vec::new(),
        func_imports: Vec::new(),
        funcs: Vec::new(),
        bodies: Vec::new(),
        exports: HashMap::new(),
        names: HashMap::new(),
    };
    let mut c_memories = 0u32;
    for payload in Parser::new(0).parse_all(wasm) {
        match payload? {
            Payload::MemorySection(r) if r.count() > 0 => bail!(
                "the C module defines {} memories of its own; it must import the guest's only",
                r.count()
            ),
            Payload::TypeSection(r) => {
                for t in r.into_iter_err_on_gc_types() {
                    m.types.push(t?);
                }
            }
            Payload::ImportSection(r) => {
                for i in r {
                    let i = i?;
                    match i.ty {
                        TypeRef::Func(t) => {
                            m.func_imports.push((i.module.to_string(), i.name.to_string(), t))
                        }
                        TypeRef::Memory(_) => {
                            c_memories += 1;
                            if c_memories > 1 {
                                bail!(
                                    "the C module imports more than one memory ({}.{}); the \
                                     splice maps exactly one onto the guest memory",
                                    i.module,
                                    i.name
                                );
                            }
                        }
                        other => bail!(
                            "the C module imports {}.{} ({other:?}); only functions and the \
                             memory can be spliced",
                            i.module,
                            i.name
                        ),
                    }
                }
            }
            Payload::FunctionSection(r) => {
                for f in r {
                    m.funcs.push(f?);
                }
            }
            Payload::ExportSection(r) => {
                for e in r {
                    let e = e?;
                    if e.kind == ExternalKind::Func {
                        m.exports.insert(e.name.to_string(), e.index);
                    }
                }
            }
            Payload::CodeSectionEntry(b) => m.bodies.push(b),
            Payload::CustomSection(r) if r.name() == "name" => {
                let reader = wasmparser::NameSectionReader::new(wasmparser::BinaryReader::new(r.data(), r.data_offset()));
                for n in reader.flatten() {
                    if let wasmparser::Name::Function(map) = n {
                        for naming in map.into_iter().flatten() {
                            m.names.insert(naming.index, naming.name.to_string());
                        }
                    }
                }
            }
            Payload::DataSection(r) => {
                if r.count() > 0 {
                    bail!(
                        "the C module has {} data segments; generated C must not have static \
                         data (it would land in guest memory)",
                        r.count()
                    );
                }
            }
            Payload::ElementSection(r) => {
                if r.count() > 0 && !image {
                    bail!("the C module has element segments (address-taken functions)");
                }
            }
            _ => {}
        }
    }
    Ok(m)
}

/// Per-function checks from `docs/function-tier.md` §1 and §4, and the calls
/// each function makes (for reachability).
fn scan(
    m: &CModule<'_>,
    f: u32,
    name: &dyn Fn(u32) -> String,
    phase_a: bool,
    blocks: &BTreeSet<u32>,
    dispatch_import: Option<u32>,
) -> Result<Vec<u32>> {
    let nimp = m.func_imports.len() as u32;
    let body = &m.bodies[(f - nimp) as usize];
    let mut calls = Vec::new();
    let mut ops = body.get_operators_reader()?;
    while !ops.eof() {
        let op = ops.read()?;
        let bad = |what: &str| -> anyhow::Error {
            anyhow!(
                "C function {} {what}; the function tier cannot splice it (docs/function-tier.md)",
                name(f)
            )
        };
        match op {
            Operator::GlobalGet { .. } | Operator::GlobalSet { .. } => {
                return Err(bad("accesses a global (clang's shadow stack?)"))
            }
            Operator::CallIndirect { .. }
            | Operator::ReturnCallIndirect { .. }
            | Operator::TableGet { .. }
            | Operator::TableSet { .. }
            | Operator::TableSize { .. }
            | Operator::TableGrow { .. }
            | Operator::TableFill { .. }
            | Operator::TableCopy { .. }
            | Operator::TableInit { .. }
            | Operator::ElemDrop { .. }
            | Operator::RefFunc { .. } => return Err(bad("uses a table")),
            Operator::MemoryInit { .. } | Operator::DataDrop { .. } => {
                return Err(bad("uses a data segment"))
            }
            Operator::MemoryGrow { .. } | Operator::MemorySize { .. } => {
                return Err(bad("grows or sizes the memory"))
            }
            Operator::Call { function_index } => {
                if phase_a
                    && (blocks.contains(&function_index)
                        || Some(function_index) == dispatch_import)
                {
                    return Err(bad(&format!(
                        "calls {} without a tail call (a C frame would nest)",
                        name(function_index)
                    )));
                }
                if phase_a && function_index >= nimp {
                    return Err(bad(&format!(
                        "calls the defined function {} (block functions must not nest)",
                        name(function_index)
                    )));
                }
                calls.push(function_index);
            }
            Operator::ReturnCall { function_index } => calls.push(function_index),
            _ => {}
        }
    }
    Ok(calls)
}

/// Rewrites a clang function's indices into the spliced module's.
struct CMap {
    types_base: u32,
    funcs: HashMap<u32, u32>,
}

impl Reencode for CMap {
    type Error = std::convert::Infallible;
    fn function_index(&mut self, f: u32) -> u32 {
        *self.funcs.get(&f).expect("unmapped C function index")
    }
    fn type_index(&mut self, t: u32) -> u32 {
        self.types_base + t
    }
}

/// Points element entries naming a retargeted tier-1 block at its C function.
struct ElemMap<'m>(&'m HashMap<u32, u32>);

impl Reencode for ElemMap<'_> {
    type Error = std::convert::Infallible;
    fn function_index(&mut self, f: u32) -> u32 {
        self.0.get(&f).copied().unwrap_or(f)
    }
}

fn val(v: wasmparser::ValType) -> Result<ValType> {
    RoundtripReencoder
        .val_type(v)
        .map_err(|e| anyhow!("value type: {e:?}"))
}

/// Splice `cwasm` (clang's module) into `tier1`. `owned` is each block the C
/// side takes over: its guest address and its tier-1 function index. The C
/// function for a block is the export `name_of(addr)`.
pub fn splice(
    tier1: &[u8],
    cwasm: &[u8],
    owned: &[(u64, u32)],
    name_of: &dyn Fn(u64) -> String,
    phase_a: bool,
) -> Result<(Vec<u8>, SpliceStats)> {
    let c = parse_c(cwasm).context("reading the C module")?;

    // The tier-1 module's shape.
    let mut t_types: Vec<wasmparser::FuncType> = Vec::new();
    let mut t_imports: HashMap<(String, String), (u32, u32)> = HashMap::new();
    let mut t_nimp = 0u32;
    let mut t_ndef = 0u32;
    let mut t_memories = 0u32;
    for payload in Parser::new(0).parse_all(tier1) {
        match payload? {
            Payload::TypeSection(r) => {
                for t in r.into_iter_err_on_gc_types() {
                    t_types.push(t?);
                }
            }
            Payload::ImportSection(r) => {
                for i in r {
                    let i = i?;
                    if let TypeRef::Func(t) = i.ty {
                        t_imports.insert((i.module.to_string(), i.name.to_string()), (t_nimp, t));
                        t_nimp += 1;
                    }
                    if let TypeRef::Memory(_) = i.ty {
                        t_memories += 1;
                    }
                }
            }
            Payload::FunctionSection(r) => t_ndef = r.count(),
            Payload::MemorySection(r) => t_memories += r.count(),
            _ => {}
        }
    }
    if t_memories != 1 {
        bail!(
            "the tier-1 module has {t_memories} memories; the splice handles exactly one \
             (profiling and --cgen do not mix yet)"
        );
    }
    let block_ty = t_types
        .iter()
        .position(|t| t.params() == [wasmparser::ValType::I32] && t.results().is_empty())
        .ok_or_else(|| anyhow!("the tier-1 module has no (i32) -> () type"))? as u32;

    // New function numbering: tier-1's, then dispatch, then the C functions
    // reachable from the owned blocks.
    let dispatch_idx = t_nimp + t_ndef;
    let c_nimp = c.func_imports.len() as u32;
    let c_dispatch_import = c
        .func_imports
        .iter()
        .position(|(m, n, _)| m == DISPATCH_MODULE && n == DISPATCH_NAME)
        .map(|i| i as u32);
    let c_name = |f: u32| -> String {
        if f < c_nimp {
            let (m, n, _) = &c.func_imports[f as usize];
            format!("{m}.{n}")
        } else {
            c.exports
                .iter()
                .find(|(_, &i)| i == f)
                .map(|(n, _)| n.clone())
                .unwrap_or_else(|| format!("#{f}"))
        }
    };
    let mut roots = Vec::new();
    for (addr, _) in owned {
        let n = name_of(*addr);
        let f = *c
            .exports
            .get(&n)
            .ok_or_else(|| anyhow!("the C module does not export {n}"))?;
        roots.push(f);
    }
    let block_fns: BTreeSet<u32> = roots.iter().copied().collect();
    let mut reach: BTreeSet<u32> = BTreeSet::new();
    let mut stack = roots.clone();
    while let Some(f) = stack.pop() {
        if f < c_nimp || !reach.insert(f) {
            continue;
        }
        for callee in scan(&c, f, &c_name, phase_a, &block_fns, c_dispatch_import)? {
            stack.push(callee);
        }
    }

    let mut fmap: HashMap<u32, u32> = HashMap::new();
    for (i, (m, n, ty)) in c.func_imports.iter().enumerate() {
        let i = i as u32;
        if m == DISPATCH_MODULE && n == DISPATCH_NAME {
            fmap.insert(i, dispatch_idx);
            continue;
        }
        let (idx, tty) = *t_imports
            .get(&(m.clone(), n.clone()))
            .ok_or_else(|| anyhow!("the C module imports {m}.{n}, which the tier-1 module does not (a libcall?)"))?;
        let (a, b) = (&c.types[*ty as usize], &t_types[tty as usize]);
        if a.params() != b.params() || a.results() != b.results() {
            bail!("{m}.{n} is declared {a:?} in C and {b:?} in the tier-1 module");
        }
        fmap.insert(i, idx);
    }
    let reach: Vec<u32> = reach.into_iter().collect();
    for (k, f) in reach.iter().enumerate() {
        fmap.insert(*f, dispatch_idx + 1 + k as u32);
    }
    // tier-1 block function -> its C replacement.
    let mut retarget: HashMap<u32, u32> = HashMap::new();
    for ((_, t1), croot) in owned.iter().zip(&roots) {
        retarget.insert(*t1, fmap[croot]);
    }

    let types_base = t_types.len() as u32;
    let mut cmap = CMap {
        types_base,
        funcs: fmap,
    };
    let mut stats = SpliceStats {
        owned: owned.len(),
        c_functions: reach.len(),
        c_code_bytes: 0,
    };

    // Re-emit the tier-1 module section by section, changing four.
    let mut out = Module::new();
    let mut code = CodeSection::new();
    let mut in_code = false;
    let mut t_def_seen = 0u32;
    let mut emit_code_tail = |code: &mut CodeSection, stats: &mut SpliceStats| -> Result<()> {
        code.function(&dispatch_function(block_ty));
        for f in &reach {
            let body = c.bodies[(*f - c_nimp) as usize].clone();
            stats.c_code_bytes += body.as_bytes().len();
            cmap.parse_function_body(code, body)
                .map_err(|e| anyhow!("re-encoding a C function: {e:?}"))?;
        }
        Ok(())
    };
    for payload in Parser::new(0).parse_all(tier1) {
        let payload = payload?;
        match &payload {
            Payload::TypeSection(_) => {
                let mut types = TypeSection::new();
                for t in &t_types {
                    let p: Result<Vec<_>> = t.params().iter().map(|v| val(*v)).collect();
                    let r: Result<Vec<_>> = t.results().iter().map(|v| val(*v)).collect();
                    types.ty().function(p?, r?);
                }
                for t in &c.types {
                    let p: Result<Vec<_>> = t.params().iter().map(|v| val(*v)).collect();
                    let r: Result<Vec<_>> = t.results().iter().map(|v| val(*v)).collect();
                    types.ty().function(p?, r?);
                }
                out.section(&types);
            }
            Payload::FunctionSection(r) => {
                let mut funcs = FunctionSection::new();
                for f in r.clone() {
                    funcs.function(f?);
                }
                funcs.function(block_ty);
                for f in &reach {
                    funcs.function(types_base + c.funcs[(*f - c_nimp) as usize]);
                }
                out.section(&funcs);
            }
            Payload::ElementSection(r) => {
                let mut elems = ElementSection::new();
                let mut em = ElemMap(&retarget);
                em.parse_element_section(&mut elems, r.clone())
                    .map_err(|e| anyhow!("re-encoding elements: {e:?}"))?;
                out.section(&elems);
            }
            Payload::CodeSectionStart { .. } => in_code = true,
            Payload::CodeSectionEntry(body) => {
                let idx = t_nimp + t_def_seen;
                t_def_seen += 1;
                if let Some(cf) = retarget.get(&idx) {
                    let mut f = Function::new([]);
                    f.instruction(&Instruction::LocalGet(0));
                    f.instruction(&Instruction::ReturnCall(*cf));
                    f.instruction(&Instruction::End);
                    code.function(&f);
                } else {
                    code.raw(body.as_bytes());
                }
                if t_def_seen == t_ndef {
                    emit_code_tail(&mut code, &mut stats)?;
                    out.section(&code);
                    in_code = false;
                }
            }
            Payload::CustomSection(r) => {
                if r.name() == "name" && names_wanted() {
                    continue; // replaced by ours, below
                }
                if r.name() == gex_emit::wasm::RELOC_SECTION {
                    // Offsets into bodies this splice rewrote: stale, and a
                    // spliced module is never merged.
                    continue;
                }
                out.section(&CustomSection {
                    name: r.name().into(),
                    data: r.data().into(),
                });
            }
            Payload::Version { .. } | Payload::End(_) => {}
            other => {
                if in_code {
                    bail!("unexpected payload inside the code section");
                }
                if let Some((id, range)) = other.as_section() {
                    out.section(&RawSection {
                        id,
                        data: &tier1[range],
                    });
                }
            }
        }
    }
    if t_ndef == 0 || t_def_seen != t_ndef {
        bail!("the tier-1 module's code section was not where it was expected");
    }
    if names_wanted() {
        // `GEX_CGEN_NAMES=1`: name the dispatch function and the C functions,
        // so an engine profile (`node --cpu-prof`) can say where time goes.
        let mut fnames = wasm_encoder::NameMap::new();
        fnames.append(dispatch_idx, "gexc_dispatch");
        for (k, f) in reach.iter().enumerate() {
            let n = c.names.get(f).cloned().unwrap_or_else(|| c_name(*f));
            fnames.append(dispatch_idx + 1 + k as u32, &n);
        }
        let mut names = wasm_encoder::NameSection::new();
        names.functions(&fnames);
        out.section(&names);
    }
    Ok((out.finish(), stats))
}

/// A module of one function, `dispatch : (i32) -> ()`, that does tier-1's
/// dispatch (the function below) over the process's imported dispatch tables
/// and memory: what a whole-image unit imports as `gexc.dispatch`
/// (docs/function-tier.md §11), instantiated once per store, so the unit's
/// own module is clang's as linked.
pub fn dispatcher_module() -> Vec<u8> {
    use wasm_encoder::{EntityType, ExportKind, ExportSection, ImportSection, MemoryType, RefType, TableType};
    let mut m = Module::new();
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32], []);
    // A body: (ctx, depth, need, ret) -> i32.
    types.ty().function([ValType::I32, ValType::I32, ValType::I32, ValType::I64], [ValType::I32]);
    // call_known: (ctx, target, depth, need, ret) -> i32.
    types.ty().function([ValType::I32, ValType::I64, ValType::I32, ValType::I32, ValType::I64], [ValType::I32]);
    m.section(&types);
    let mut imports = ImportSection::new();
    imports.import(
        gex_abi::IMPORT_MODULE,
        gex_abi::exports::MEMORY,
        EntityType::Memory(MemoryType {
            minimum: 0,
            maximum: Some(65536),
            memory64: false,
            shared: true,
            page_size_log2: None,
        }),
    );
    for i in 0..gex_abi::DISPATCH_TABLES {
        imports.import(
            gex_abi::IMPORT_MODULE,
            &gex_abi::exports::dispatch_table(i),
            EntityType::Table(TableType {
                element_type: RefType::FUNCREF,
                minimum: 0,
                maximum: None,
                table64: false,
                shared: false,
            }),
        );
    }
    // The body table, after the dispatch tables.
    imports.import(
        DISPATCH_MODULE,
        BODY_TABLE,
        EntityType::Table(TableType {
            element_type: RefType::FUNCREF,
            minimum: 0,
            maximum: None,
            table64: false,
            shared: false,
        }),
    );
    m.section(&imports);
    let mut funcs = FunctionSection::new();
    funcs.function(0);
    funcs.function(2);
    m.section(&funcs);
    let mut exports = ExportSection::new();
    exports.export(DISPATCH_NAME, ExportKind::Func, 0);
    exports.export(CALL_KNOWN_NAME, ExportKind::Func, 1);
    m.section(&exports);
    let mut code = CodeSection::new();
    code.function(&dispatch_function(0));
    code.function(&call_known_function(1, gex_abi::DISPATCH_TABLES as u32));
    m.section(&code);
    m.finish()
}

/// Import name, in [`DISPATCH_MODULE`], of the process's body table and of
/// the function a whole-image unit's indirect calls go through.
pub const BODY_TABLE: &str = "bodies";
pub const CALL_KNOWN_NAME: &str = "call_known";

/// `call_known(ctx, target, depth, need, ret) -> i32`: the body the body
/// table holds for `target`'s slot (numbered like dispatch: the page's
/// directory entry plus the slot within the page), tail-called with
/// `ctx::PC = target`; -1 (leave) for an address outside the directory, a
/// page outside table 0, or a slot past `gex_abi::BODY_LIMIT`.
fn call_known_function(body_ty: u32, body_table: u32) -> Function {
    use Instruction as I;
    // Params: 0 ctx, 1 target, 2 depth, 3 need, 4 ret. Locals: 5 entry, 6 slot.
    let mut f = Function::new([(2, ValType::I32)]);
    let m = |off: u64, align: u32| MemArg { offset: off, align, memory_index: 0 };
    // target >= the directory's reach: leave.
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::I64Const((gex_abi::DISPATCH_MAX_PAGES * gex_abi::DISPATCH_PAGE) as i64));
    f.instruction(&I::I64GeU);
    f.instruction(&I::If(BlockType::Empty));
    f.instruction(&I::I32Const(-1));
    f.instruction(&I::Return);
    f.instruction(&I::End);
    // entry = dir[target >> 12]
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::I64Const(gex_abi::DISPATCH_PAGE.trailing_zeros() as i64));
    f.instruction(&I::I64ShrU);
    f.instruction(&I::I32WrapI64);
    f.instruction(&I::I32Const(4));
    f.instruction(&I::I32Mul);
    f.instruction(&I::I32Load(m(gex_abi::DISPATCH_DIR, 2)));
    f.instruction(&I::LocalTee(5));
    // A table index in the top bits: not table 0, leave.
    f.instruction(&I::I32Const(gex_abi::DISPATCH_SLOT_BITS as i32));
    f.instruction(&I::I32ShrU);
    f.instruction(&I::If(BlockType::Empty));
    f.instruction(&I::I32Const(-1));
    f.instruction(&I::Return);
    f.instruction(&I::End);
    // slot = entry + (target & 0xfff) >> shift
    f.instruction(&I::LocalGet(5));
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::I64Const((gex_abi::DISPATCH_PAGE - 1) as i64));
    f.instruction(&I::I64And);
    if gex_abi::DISPATCH_SHIFT != 0 {
        f.instruction(&I::I64Const(gex_abi::DISPATCH_SHIFT as i64));
        f.instruction(&I::I64ShrU);
    }
    f.instruction(&I::I32WrapI64);
    f.instruction(&I::I32Add);
    f.instruction(&I::LocalTee(6));
    f.instruction(&I::I32Const(gex_abi::BODY_LIMIT as i32));
    f.instruction(&I::I32Load(m(0, 2)));
    f.instruction(&I::I32GeU);
    f.instruction(&I::If(BlockType::Empty));
    f.instruction(&I::I32Const(-1));
    f.instruction(&I::Return);
    f.instruction(&I::End);
    // ctx::PC = target (a body checks it: an x86 slot covers four bytes).
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::I64Store(m(ctx::PC as u64, 3)));
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::LocalGet(2));
    f.instruction(&I::LocalGet(3));
    f.instruction(&I::LocalGet(4));
    f.instruction(&I::LocalGet(6));
    f.instruction(&I::ReturnCallIndirect { type_index: body_ty, table_index: body_table });
    f.instruction(&I::End);
    f
}

/// `(ctx) -> ()`: tail-call the block for `ctx::LAST_TARGET`, exactly as
/// tier-1's `emit_dispatch_tail_call_inner` does after its stores.
fn dispatch_function(block_ty: u32) -> Function {
    use Instruction as I;
    // Locals: 1 = target (i64), 2 = page (i64), 3 = entry (i32), 4 = index (i32).
    let mut f = Function::new([(2, ValType::I64), (2, ValType::I32)]);
    let m = |off: u64, align: u32| MemArg {
        offset: off,
        align,
        memory_index: 0,
    };
    f.instruction(&I::LocalGet(0));
    f.instruction(&I::I64Load(m(ctx::LAST_TARGET as u64, 3)));
    f.instruction(&I::LocalSet(1));
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::I64Const(gex_abi::DISPATCH_PAGE.trailing_zeros() as i64));
    f.instruction(&I::I64ShrU);
    f.instruction(&I::LocalSet(2));
    f.instruction(&I::LocalGet(2));
    f.instruction(&I::I32WrapI64);
    f.instruction(&I::I32Const(4));
    f.instruction(&I::I32Mul);
    f.instruction(&I::I32Load(m(gex_abi::DISPATCH_DIR, 2)));
    f.instruction(&I::I32Const(0));
    f.instruction(&I::LocalGet(2));
    f.instruction(&I::I64Const(gex_abi::DISPATCH_MAX_PAGES as i64));
    f.instruction(&I::I64LtU);
    f.instruction(&I::Select);
    f.instruction(&I::LocalTee(3));
    f.instruction(&I::I32Const(gex_abi::DISPATCH_SLOT_MASK as i32));
    f.instruction(&I::I32And);
    f.instruction(&I::LocalGet(1));
    f.instruction(&I::I64Const((gex_abi::DISPATCH_PAGE - 1) as i64));
    f.instruction(&I::I64And);
    if gex_abi::DISPATCH_SHIFT != 0 {
        f.instruction(&I::I64Const(gex_abi::DISPATCH_SHIFT as i64));
        f.instruction(&I::I64ShrU);
    }
    f.instruction(&I::I32WrapI64);
    f.instruction(&I::I32Add);
    f.instruction(&I::LocalSet(4));
    let n = gex_abi::DISPATCH_TABLES as u32;
    if n <= 1 {
        f.instruction(&I::LocalGet(0));
        f.instruction(&I::LocalGet(4));
        f.instruction(&I::ReturnCallIndirect {
            type_index: block_ty,
            table_index: 0,
        });
        f.instruction(&I::End);
        return f;
    }
    f.instruction(&I::LocalGet(3));
    f.instruction(&I::I32Const(gex_abi::DISPATCH_SLOT_BITS as i32));
    f.instruction(&I::I32ShrU);
    f.instruction(&I::LocalSet(3));
    for _ in 0..n {
        f.instruction(&I::Block(BlockType::Empty));
    }
    f.instruction(&I::LocalGet(3));
    let targets: Vec<u32> = (0..n).map(|k| n - 1 - k).collect();
    f.instruction(&I::BrTable(targets.into(), n - 1));
    for k in (0..n).rev() {
        f.instruction(&I::End);
        f.instruction(&I::LocalGet(0));
        f.instruction(&I::LocalGet(4));
        f.instruction(&I::ReturnCallIndirect {
            type_index: block_ty,
            table_index: k,
        });
    }
    f.instruction(&I::End);
    f
}

fn names_wanted() -> bool {
    std::env::var_os("GEX_CGEN_NAMES").is_some()
}

/// The defined functions of clang's module that read or write a global
/// (clang's shadow-stack pointer: a frame it could not keep in locals), by
/// name, so the build can leave their guest functions to tier-1. Anything
/// else the splice cannot place (a data segment, a table) still fails the
/// build loudly, in `parse_c`.
pub fn unspliceable_users(cwasm: &[u8]) -> Result<Vec<String>> {
    let c = parse_c_as(cwasm, true).context("reading the C module")?;
    let nimp = c.func_imports.len() as u32;
    let mut out = Vec::new();
    for (k, body) in c.bodies.iter().enumerate() {
        let f = nimp + k as u32;
        let mut ops = body.get_operators_reader()?;
        while !ops.eof() {
            if matches!(ops.read()?, Operator::GlobalGet { .. } | Operator::GlobalSet { .. }) {
                let name = c.names.get(&f).cloned().or_else(|| {
                    c.exports.iter().find(|(_, &i)| i == f).map(|(n, _)| n.clone())
                });
                out.push(name.unwrap_or_else(|| format!("#{f}")));
                break;
            }
        }
    }
    Ok(out)
}
