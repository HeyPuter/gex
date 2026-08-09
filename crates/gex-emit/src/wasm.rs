//! A minimal WebAssembly binary writer.
//!
//! Written by hand rather than pulled from a crate for two reasons: the
//! translator has to run *inside* wasm as `gex-jit` (PLAN §2.1), where a lean
//! dependency-free encoder keeps the shipped module small; and tier-1 codegen
//! is a hot loop where we want to control byte emission directly.
//!
//! Covers the MVP plus the three proposals gex depends on: tail calls
//! (PLAN §2.4), threads/atomics (PLAN §5.8) and reference types for the
//! dispatch table.

/// Wasm value types.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum ValType {
    I32,
    I64,
    F32,
    F64,
    V128,
    FuncRef,
}

impl ValType {
    fn byte(self) -> u8 {
        match self {
            ValType::I32 => 0x7f,
            ValType::I64 => 0x7e,
            ValType::F32 => 0x7d,
            ValType::F64 => 0x7c,
            ValType::V128 => 0x7b,
            ValType::FuncRef => 0x70,
        }
    }
}

/// A function signature.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct FuncType {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
}

impl FuncType {
    pub fn new(params: Vec<ValType>, results: Vec<ValType>) -> FuncType {
        FuncType { params, results }
    }
}

// ---------------------------------------------------------------------------
// LEB128
// ---------------------------------------------------------------------------

pub fn write_u32(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

pub fn write_u64(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

pub fn write_i32(out: &mut Vec<u8>, v: i32) {
    write_i64(out, v as i64)
}

pub fn write_i64(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        let sign_bit_set = byte & 0x40 != 0;
        if (v == 0 && !sign_bit_set) || (v == -1 && sign_bit_set) {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn write_name(out: &mut Vec<u8>, s: &str) {
    write_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

// ---------------------------------------------------------------------------
// Instruction encoding
// ---------------------------------------------------------------------------

/// Opcode constants, named as in the specification's text format.
pub mod op {
    pub const UNREACHABLE: u8 = 0x00;
    pub const NOP: u8 = 0x01;
    pub const BLOCK: u8 = 0x02;
    pub const LOOP: u8 = 0x03;
    pub const IF: u8 = 0x04;
    pub const ELSE: u8 = 0x05;
    pub const END: u8 = 0x0b;
    pub const BR: u8 = 0x0c;
    pub const BR_IF: u8 = 0x0d;
    pub const BR_TABLE: u8 = 0x0e;
    pub const RETURN: u8 = 0x0f;
    pub const CALL: u8 = 0x10;
    pub const CALL_INDIRECT: u8 = 0x11;
    /// Tail-call proposal.
    pub const RETURN_CALL: u8 = 0x12;
    /// Tail-call proposal.
    pub const RETURN_CALL_INDIRECT: u8 = 0x13;

    pub const DROP: u8 = 0x1a;
    pub const SELECT: u8 = 0x1b;

    pub const LOCAL_GET: u8 = 0x20;
    pub const LOCAL_SET: u8 = 0x21;
    pub const LOCAL_TEE: u8 = 0x22;
    pub const GLOBAL_GET: u8 = 0x23;
    pub const GLOBAL_SET: u8 = 0x24;

    pub const I32_CONST: u8 = 0x41;
    pub const I64_CONST: u8 = 0x42;

    /// Atomic instruction prefix (threads proposal).
    pub const ATOMIC_PREFIX: u8 = 0xfe;
    /// Miscellaneous prefix (bulk memory, saturating conversions).
    pub const MISC_PREFIX: u8 = 0xfc;
}

/// Numeric opcodes shared by both integer widths, offset from the `i32` base.
#[derive(Copy, Clone, Debug)]
pub struct NumOps;

/// A function body under construction.
pub struct FuncBody {
    /// Locals beyond the parameters, in declaration order.
    locals: Vec<ValType>,
    pub code: Vec<u8>,
    n_params: u32,
}

impl FuncBody {
    pub fn new(n_params: u32) -> FuncBody {
        FuncBody {
            locals: Vec::new(),
            code: Vec::new(),
            n_params,
        }
    }

    /// Declare a new local and return its index.
    pub fn add_local(&mut self, ty: ValType) -> u32 {
        self.locals.push(ty);
        self.n_params + self.locals.len() as u32 - 1
    }

    pub fn n_locals(&self) -> usize {
        self.locals.len()
    }

    // -- raw emission ------------------------------------------------------

    pub fn byte(&mut self, b: u8) {
        self.code.push(b);
    }

    pub fn u32(&mut self, v: u32) {
        write_u32(&mut self.code, v);
    }

    // -- instructions ------------------------------------------------------

    pub fn local_get(&mut self, idx: u32) {
        self.byte(op::LOCAL_GET);
        self.u32(idx);
    }

    pub fn local_set(&mut self, idx: u32) {
        self.byte(op::LOCAL_SET);
        self.u32(idx);
    }

    pub fn local_tee(&mut self, idx: u32) {
        self.byte(op::LOCAL_TEE);
        self.u32(idx);
    }

    pub fn i32_const(&mut self, v: i32) {
        self.byte(op::I32_CONST);
        write_i32(&mut self.code, v);
    }

    pub fn i64_const(&mut self, v: i64) {
        self.byte(op::I64_CONST);
        write_i64(&mut self.code, v);
    }

    pub fn call(&mut self, func: u32) {
        self.byte(op::CALL);
        self.u32(func);
    }

    pub fn return_call(&mut self, func: u32) {
        self.byte(op::RETURN_CALL);
        self.u32(func);
    }

    pub fn return_call_indirect(&mut self, ty: u32, table: u32) {
        self.byte(op::RETURN_CALL_INDIRECT);
        self.u32(ty);
        self.u32(table);
    }

    pub fn select(&mut self) {
        self.byte(op::SELECT);
    }

    pub fn drop_(&mut self) {
        self.byte(op::DROP);
    }

    pub fn unreachable(&mut self) {
        self.byte(op::UNREACHABLE);
    }

    pub fn ret(&mut self) {
        self.byte(op::RETURN);
    }

    pub fn end(&mut self) {
        self.byte(op::END);
    }

    /// `if` with a single result value.
    pub fn if_result(&mut self, ty: ValType) {
        self.byte(op::IF);
        self.byte(ty.byte());
    }

    /// `if` with no result.
    pub fn if_void(&mut self) {
        self.byte(op::IF);
        self.byte(0x40);
    }

    /// `block` with no result.
    pub fn block_void(&mut self) {
        self.byte(op::BLOCK);
        self.byte(0x40);
    }

    /// `br_table`: branch to `targets[i]` for index `i`, or to `default`.
    pub fn br_table(&mut self, targets: &[u32], default: u32) {
        self.byte(op::BR_TABLE);
        self.u32(targets.len() as u32);
        for t in targets {
            self.u32(*t);
        }
        self.u32(default);
    }

    /// `loop` with no result.
    pub fn loop_void(&mut self) {
        self.byte(op::LOOP);
        self.byte(0x40);
    }

    /// Unconditional branch to the label `depth` levels out.
    pub fn br(&mut self, depth: u32) {
        self.byte(op::BR);
        self.u32(depth);
    }

    /// Branch to the label `depth` levels out when the `i32` on top is
    /// non-zero, consuming it.
    pub fn br_if(&mut self, depth: u32) {
        self.byte(op::BR_IF);
        self.u32(depth);
    }

    pub fn else_(&mut self) {
        self.byte(op::ELSE);
    }

    /// A memory access immediate: alignment hint (log2) and static offset.
    fn memarg(&mut self, align_log2: u32, offset: u32) {
        self.u32(align_log2);
        self.u32(offset);
    }

    /// Emit a load. `opcode` is the single-byte load opcode.
    pub fn load(&mut self, opcode: u8, align_log2: u32, offset: u32) {
        self.byte(opcode);
        self.memarg(align_log2, offset);
    }

    /// Emit a store. `opcode` is the single-byte store opcode.
    pub fn store(&mut self, opcode: u8, align_log2: u32, offset: u32) {
        self.byte(opcode);
        self.memarg(align_log2, offset);
    }

    /// Emit an atomic instruction (0xFE prefix) with a memory argument.
    pub fn atomic(&mut self, sub: u32, align_log2: u32, offset: u32) {
        self.byte(op::ATOMIC_PREFIX);
        self.u32(sub);
        self.memarg(align_log2, offset);
    }

    /// Emit a SIMD instruction (0xFD prefix) with a memory argument.
    pub fn simd_mem(&mut self, sub: u32, align_log2: u32, offset: u32) {
        self.byte(0xfd);
        self.u32(sub);
        self.memarg(align_log2, offset);
    }

    /// Emit a SIMD instruction (0xFD prefix) with a memory argument and a
    /// lane index — the `loadN_lane` / `storeN_lane` family.
    pub fn simd_mem_lane(&mut self, sub: u32, align_log2: u32, offset: u32, lane: u8) {
        self.byte(0xfd);
        self.u32(sub);
        self.memarg(align_log2, offset);
        self.byte(lane);
    }

    /// Emit a SIMD instruction with no immediate.
    pub fn simd(&mut self, sub: u32) {
        self.byte(0xfd);
        self.u32(sub);
    }

    /// Emit a SIMD instruction taking a lane index.
    pub fn simd_lane(&mut self, sub: u32, lane: u8) {
        self.byte(0xfd);
        self.u32(sub);
        self.byte(lane);
    }

    /// `i8x16.shuffle`, whose immediate is sixteen lane indices.
    pub fn simd_shuffle(&mut self, lanes: &[u8; 16]) {
        self.byte(0xfd);
        self.u32(0x0d);
        self.code.extend_from_slice(lanes);
    }

    /// `v128.const`.
    pub fn v128_const(&mut self, bytes: &[u8; 16]) {
        self.byte(0xfd);
        self.u32(0x0c);
        self.code.extend_from_slice(bytes);
    }

    /// `atomic.fence`.
    pub fn atomic_fence(&mut self) {
        self.byte(op::ATOMIC_PREFIX);
        self.u32(0x03);
        self.byte(0x00);
    }

    /// Serialise the body into the code-section entry form.
    pub fn finish(self) -> Vec<u8> {
        let mut body = Vec::with_capacity(self.code.len() + 16);
        // Run-length encode the local declarations.
        let mut runs: Vec<(u32, ValType)> = Vec::new();
        for ty in &self.locals {
            match runs.last_mut() {
                Some((n, t)) if *t == *ty => *n += 1,
                _ => runs.push((1, *ty)),
            }
        }
        write_u32(&mut body, runs.len() as u32);
        for (n, ty) in runs {
            write_u32(&mut body, n);
            body.push(ty.byte());
        }
        body.extend_from_slice(&self.code);
        body.push(op::END);

        let mut out = Vec::with_capacity(body.len() + 5);
        write_u32(&mut out, body.len() as u32);
        out.extend_from_slice(&body);
        out
    }
}

// ---------------------------------------------------------------------------
// Module assembly
// ---------------------------------------------------------------------------

/// What kind of thing an import brings in.
pub enum ImportKind {
    Func {
        ty: u32,
    },
    /// A memory the host provides, in pages: `min` is what it must allocate,
    /// `max` how far it may grow. A process pays for `min` the moment it
    /// exists, so the two are not the same number.
    Memory {
        min: u64,
        max: u64,
        shared: bool,
        /// The index type is `i64`, so an address is a guest address and
        /// the engine bounds-checks it directly. See `tier1::emit_address`.
        memory64: bool,
    },
    /// A funcref table with the given minimum size.
    Table {
        min: u32,
    },
}

/// An import.
pub struct Import {
    pub module: String,
    pub name: String,
    pub kind: ImportKind,
}

/// Where a table's initial contents come from.
pub struct ElementSegment {
    pub table: u32,
    pub offset: u32,
    pub funcs: Vec<u32>,
}

/// A memory initialiser.
pub struct DataSegment {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

/// A wasm module under construction.
#[derive(Default)]
pub struct Module {
    types: Vec<FuncType>,
    imports: Vec<Import>,
    n_func_imports: u32,
    /// Type index for each locally defined function.
    functions: Vec<u32>,
    /// Encoded bodies, parallel to `functions`.
    bodies: Vec<Vec<u8>>,
    /// `(min, max)` in elements, for the single funcref table.
    /// Locally defined dispatch tables, in index order.
    tables: Vec<(u32, u32)>,
    /// `(min, max)` in pages.
    memory: Option<(u64, u64)>,
    exports: Vec<(String, ExportKind, u32)>,
    elements: Vec<ElementSegment>,
    data: Vec<DataSegment>,
    start: Option<u32>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ExportKind {
    Func,
    Table,
    Memory,
    Global,
}

impl Module {
    pub fn new() -> Module {
        Module::default()
    }

    /// Intern a function type, returning its index.
    pub fn func_type(&mut self, ty: FuncType) -> u32 {
        if let Some(i) = self.types.iter().position(|t| *t == ty) {
            return i as u32;
        }
        self.types.push(ty);
        self.types.len() as u32 - 1
    }

    /// Declare an imported function. Must be called before [`Self::function`]
    /// so that function indices line up: imports occupy the low indices.
    pub fn import_func(&mut self, module: &str, name: &str, ty: u32) -> u32 {
        assert!(
            self.functions.is_empty(),
            "all imports must be declared before any local function"
        );
        self.imports.push(Import {
            module: module.to_string(),
            name: name.to_string(),
            kind: ImportKind::Func { ty },
        });
        self.n_func_imports += 1;
        self.n_func_imports - 1
    }

    /// Import the guest memory instead of declaring one.
    /// Import a memory instead of defining one.
    ///
    /// A defined memory belongs to its instance, so two stores instantiating
    /// the same module would get two of them. Every guest thread has to see
    /// the same bytes, which means the host owns the memory and every module
    /// imports it.
    /// `min` is what the host must allocate; `max` is how far it may grow.
    /// They differ because a process pays for `min` the moment it exists.
    pub fn import_memory(&mut self, module: &str, name: &str, min: u64, max: u64, shared: bool) {
        self.imports.push(Import {
            module: module.to_string(),
            name: name.to_string(),
            kind: ImportKind::Memory {
                min,
                max,
                shared,
                memory64: crate::tier1::memory64_enabled(),
            },
        });
    }

    /// Import the dispatch table instead of declaring one.
    pub fn import_table(&mut self, module: &str, name: &str, min: u32) {
        self.imports.push(Import {
            module: module.to_string(),
            name: name.to_string(),
            kind: ImportKind::Table { min },
        });
    }

    /// Number of imported *functions*, which is what local function indices
    /// are offset by. Memory and table imports do not shift them.
    pub fn n_imports(&self) -> u32 {
        self.n_func_imports
    }

    /// Reserve a function index without providing a body yet, so that code
    /// generated for one block can reference another block that has not been
    /// emitted.
    pub fn declare_func(&mut self, ty: u32) -> u32 {
        self.functions.push(ty);
        self.bodies.push(Vec::new());
        self.n_imports() + self.functions.len() as u32 - 1
    }

    /// Supply the body of a previously declared function.
    pub fn define_func(&mut self, index: u32, body: Vec<u8>) {
        let local = (index - self.n_imports()) as usize;
        self.bodies[local] = body;
    }

    /// Declare a locally defined dispatch table.
    ///
    /// Called once per table, in index order. Several exist because V8 caps
    /// one at ten million entries and a real application needs more -- see
    /// `gex_abi::DISPATCH_TABLES`.
    pub fn set_table(&mut self, min: u32, max: u32) {
        self.tables.push((min, max));
    }

    pub fn set_memory(&mut self, min_pages: u64, max_pages: u64) {
        self.memory = Some((min_pages, max_pages));
    }

    pub fn export(&mut self, name: &str, kind: ExportKind, index: u32) {
        self.exports.push((name.to_string(), kind, index));
    }

    pub fn element(&mut self, seg: ElementSegment) {
        self.elements.push(seg);
    }

    pub fn data(&mut self, seg: DataSegment) {
        self.data.push(seg);
    }

    pub fn set_start(&mut self, func: u32) {
        self.start = Some(func);
    }

    /// Serialise the whole module.
    pub fn encode(&self) -> Vec<u8> {
        self.encode_with_custom(&[])
    }

    /// Encode, with custom sections appended. A custom section is how a
    /// module carries something the engine ignores and the host needs -- for
    /// gex, the dispatch metadata, so one file is enough to run a program
    /// rather than a module and a sidecar.
    pub fn encode_with_custom(&self, custom: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 << 20);
        out.extend_from_slice(b"\0asm");
        out.extend_from_slice(&1u32.to_le_bytes());

        // 1: types
        if !self.types.is_empty() {
            let mut s = Vec::new();
            write_u32(&mut s, self.types.len() as u32);
            for t in &self.types {
                s.push(0x60);
                write_u32(&mut s, t.params.len() as u32);
                for p in &t.params {
                    s.push(p.byte());
                }
                write_u32(&mut s, t.results.len() as u32);
                for r in &t.results {
                    s.push(r.byte());
                }
            }
            section(&mut out, 1, &s);
        }

        // 2: imports
        if !self.imports.is_empty() {
            let mut s = Vec::new();
            write_u32(&mut s, self.imports.len() as u32);
            for i in &self.imports {
                write_name(&mut s, &i.module);
                write_name(&mut s, &i.name);
                match &i.kind {
                    ImportKind::Func { ty } => {
                        s.push(0x00);
                        write_u32(&mut s, *ty);
                    }
                    ImportKind::Memory {
                        min,
                        max,
                        shared,
                        memory64,
                    } => {
                        s.push(0x02);
                        // Limits flags: bit 0 says a maximum follows, bit 1
                        // shared, bit 2 that the index type is `i64`. A
                        // shared memory must declare a maximum, and both of
                        // the other two are independent of that.
                        let mut flags = 0x01;
                        if *shared {
                            flags |= 0x02;
                        }
                        if *memory64 {
                            flags |= 0x04;
                        }
                        s.push(flags);
                        write_u64(&mut s, *min);
                        write_u64(&mut s, *max);
                    }
                    ImportKind::Table { min } => {
                        s.push(0x01);
                        s.push(ValType::FuncRef.byte());
                        s.push(0x00);
                        write_u32(&mut s, *min);
                    }
                }
            }
            section(&mut out, 2, &s);
        }

        // 3: functions
        if !self.functions.is_empty() {
            let mut s = Vec::new();
            write_u32(&mut s, self.functions.len() as u32);
            for t in &self.functions {
                write_u32(&mut s, *t);
            }
            section(&mut out, 3, &s);
        }

        // 4: tables
        if !self.tables.is_empty() {
            let mut s = Vec::new();
            write_u32(&mut s, self.tables.len() as u32);
            for (min, max) in &self.tables {
                s.push(ValType::FuncRef.byte());
                s.push(0x01); // limits with maximum
                write_u32(&mut s, *min);
                write_u32(&mut s, *max);
            }
            section(&mut out, 4, &s);
        }

        // 5: memories
        if let Some((min, max)) = self.memory {
            let mut s = Vec::new();
            write_u32(&mut s, 1);
            s.push(0x01); // limits with maximum
            write_u64(&mut s, min);
            write_u64(&mut s, max);
            section(&mut out, 5, &s);
        }

        // 7: exports
        if !self.exports.is_empty() {
            let mut s = Vec::new();
            write_u32(&mut s, self.exports.len() as u32);
            for (name, kind, idx) in &self.exports {
                write_name(&mut s, name);
                s.push(match kind {
                    ExportKind::Func => 0x00,
                    ExportKind::Table => 0x01,
                    ExportKind::Memory => 0x02,
                    ExportKind::Global => 0x03,
                });
                write_u32(&mut s, *idx);
            }
            section(&mut out, 7, &s);
        }

        // 8: start
        if let Some(f) = self.start {
            let mut s = Vec::new();
            write_u32(&mut s, f);
            section(&mut out, 8, &s);
        }

        // 9: elements
        if !self.elements.is_empty() {
            let mut s = Vec::new();
            write_u32(&mut s, self.elements.len() as u32);
            for e in &self.elements {
                if e.table == 0 {
                    s.push(0x00);
                } else {
                    s.push(0x02);
                    write_u32(&mut s, e.table);
                }
                s.push(op::I32_CONST);
                write_i32(&mut s, e.offset as i32);
                s.push(op::END);
                if e.table != 0 {
                    s.push(0x00); // elemkind: funcref
                }
                write_u32(&mut s, e.funcs.len() as u32);
                for f in &e.funcs {
                    write_u32(&mut s, *f);
                }
            }
            section(&mut out, 9, &s);
        }

        // 10: code
        if !self.bodies.is_empty() {
            let mut s = Vec::new();
            write_u32(&mut s, self.bodies.len() as u32);
            for b in &self.bodies {
                s.extend_from_slice(b);
            }
            section(&mut out, 10, &s);
        }

        // 11: data
        if !self.data.is_empty() {
            let mut s = Vec::new();
            write_u32(&mut s, self.data.len() as u32);
            // The offset of an active data segment is a constant expression
            // typed by the memory it lands in, so a 64-bit memory wants an
            // `i64.const` here -- an `i32.const` is a validation error, not a
            // widening.
            let wide = crate::tier1::memory64_enabled();
            for d in &self.data {
                s.push(0x00); // active, memory 0
                if wide {
                    s.push(op::I64_CONST);
                    write_i64(&mut s, d.offset as i64);
                } else {
                    s.push(op::I32_CONST);
                    write_i32(&mut s, d.offset as i32);
                }
                s.push(op::END);
                write_u32(&mut s, d.bytes.len() as u32);
                s.extend_from_slice(&d.bytes);
            }
            section(&mut out, 11, &s);
        }

        for (name, bytes) in custom {
            let mut sec = Vec::with_capacity(name.len() + bytes.len() + 8);
            write_name(&mut sec, name);
            sec.extend_from_slice(bytes);
            section(&mut out, 0, &sec);
        }

        out
    }
}

fn section(out: &mut Vec<u8>, id: u8, payload: &[u8]) {
    out.push(id);
    write_u32(out, payload.len() as u32);
    out.extend_from_slice(payload);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb128_roundtrips() {
        let mut v = Vec::new();
        write_u32(&mut v, 0);
        assert_eq!(v, [0]);
        v.clear();
        write_u32(&mut v, 624485);
        assert_eq!(v, [0xe5, 0x8e, 0x26]);
        v.clear();
        write_i64(&mut v, -1);
        assert_eq!(v, [0x7f]);
        v.clear();
        write_i64(&mut v, -123456);
        assert_eq!(v, [0xc0, 0xbb, 0x78]);
        v.clear();
        write_i32(&mut v, 64);
        assert_eq!(v, [0xc0, 0x00]);
    }

    #[test]
    fn empty_module_has_a_valid_header() {
        let m = Module::new();
        let bytes = m.encode();
        assert_eq!(&bytes[..4], b"\0asm");
        assert_eq!(&bytes[4..8], &[1, 0, 0, 0]);
    }

    #[test]
    fn function_indices_account_for_imports() {
        let mut m = Module::new();
        let t = m.func_type(FuncType::new(vec![ValType::I32], vec![]));
        assert_eq!(m.import_func("gex", "syscall", t), 0);
        assert_eq!(m.import_func("gex", "illegal", t), 1);
        assert_eq!(m.declare_func(t), 2);
        assert_eq!(m.declare_func(t), 3);
    }
}
