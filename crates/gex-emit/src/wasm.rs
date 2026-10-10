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
use std::collections::HashMap;

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

/// Custom section naming every immediate that points at a local function.
///
/// Fixed-width LEBs at these payload offsets, so `gex_emit::merge` can add a
/// function offset to each without a body changing length.
pub const RELOC_SECTION: &str = "gex.reloc";

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

/// A `u32` in exactly five LEB bytes, so it can be rewritten in place.
///
/// Wasm accepts a LEB longer than it needs to be as long as it is no longer
/// than the type allows, and the cost is four bytes in the module and nothing
/// at run time.
pub fn write_u32_fixed(out: &mut Vec<u8>, v: u32) {
    for i in 0..5 {
        let mut byte = ((v >> (i * 7)) & 0x7f) as u8;
        if i < 4 {
            byte |= 0x80;
        }
        out.push(byte);
    }
}

/// Rewrite a five-byte LEB written by [`write_u32_fixed`].
pub fn patch_u32_fixed(out: &mut [u8], at: usize, v: u32) {
    for i in 0..5 {
        let mut byte = ((v >> (i * 7)) & 0x7f) as u8;
        if i < 4 {
            byte |= 0x80;
        }
        out[at + i] = byte;
    }
}

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

pub(crate) fn write_name(out: &mut Vec<u8>, s: &str) {
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
    /// Offsets into `code` of every immediate naming a *local* function.
    ///
    /// Import calls are fixed for ever, but a call to another block indexes
    /// this module's own functions and moves when the module is concatenated
    /// with another. Recording the offsets lets the merger patch them without
    /// decoding the stream.
    relocs: Vec<u32>,
    /// Peephole for the function JIT ([`Self::fold_tees`]): the last
    /// `local.set` emitted, as (opcode offset, local, end of code then).
    last_set: Option<(usize, u32, usize)>,
    fold: bool,
    /// With `fold`: every local instruction, as (offset, local, length).
    local_log: Vec<(u32, u32, u8)>,
}

impl FuncBody {
    pub fn new(n_params: u32) -> FuncBody {
        FuncBody {
            locals: Vec::new(),
            code: Vec::new(),
            n_params,
            relocs: Vec::new(),
            last_set: None,
            fold: false,
            local_log: Vec::new(),
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
        // `local.set x; local.get x` is `local.tee x`: nothing can branch
        // between two adjacent instructions.
        if let Some((at, x, end)) = self.last_set {
            if self.fold && x == idx && end == self.code.len() && self.code[at] == op::LOCAL_SET {
                self.code[at] = op::LOCAL_TEE;
                self.last_set = None;
                return;
            }
        }
        let at = self.code.len();
        self.byte(op::LOCAL_GET);
        self.u32(idx);
        self.log_local(at, idx);
    }

    pub fn local_set(&mut self, idx: u32) {
        let at = self.code.len();
        self.byte(op::LOCAL_SET);
        self.u32(idx);
        self.last_set = Some((at, idx, self.code.len()));
        self.log_local(at, idx);
    }

    fn log_local(&mut self, at: usize, idx: u32) {
        if self.fold {
            self.local_log.push((at as u32, idx, (self.code.len() - at) as u8));
        }
    }

    /// Remove every `local.tee` of a local `temp` admits whose value nothing
    /// reads: the next instruction naming that local, in code order, sets
    /// it (or there is none). Sound for locals that never carry a value
    /// backwards (around a loop) -- a block-local temporary is set before
    /// any read in its block. The value the tee left on the stack stays.
    ///
    /// `loop_safe`: also keep a tee when a read of its local comes before
    /// it in code order, which a back-edge could reach with the tee's value
    /// (tier 1's fused loops keep their iteration counter and member index
    /// in pooled locals: stripping the counter's tee made a spinning loop
    /// never reach its signal poll).
    pub fn strip_dead_tees(&mut self, temp: &dyn Fn(u32) -> bool, loop_safe: bool) {
        if !self.fold {
            return;
        }
        let log = std::mem::take(&mut self.local_log);
        // First read of each local, in code order (`loop_safe`).
        let mut first_get: HashMap<u32, u32> = HashMap::new();
        if loop_safe {
            for &(at, idx, _) in &log {
                if self.code[at as usize] == op::LOCAL_GET {
                    first_get.entry(idx).or_insert(at);
                }
            }
        }
        let mut next_of: HashMap<u32, usize> = HashMap::new();
        let mut dead = vec![false; log.len()];
        for (k, &(at, idx, _)) in log.iter().enumerate().rev() {
            if self.code[at as usize] == op::LOCAL_TEE
                && temp(idx)
                && first_get.get(&idx).is_none_or(|&g| g > at)
            {
                dead[k] = match next_of.get(&idx) {
                    None => true,
                    Some(&n) => self.code[log[n].0 as usize] == op::LOCAL_SET,
                };
            }
            next_of.insert(idx, k);
        }
        if !dead.iter().any(|&d| d) {
            return;
        }
        let mut out = Vec::with_capacity(self.code.len());
        let mut cut: Vec<(u32, u32)> = Vec::new(); // (offset, bytes removed up to and including it)
        let mut from = 0usize;
        let mut removed = 0u32;
        for (k, &(at, _, len)) in log.iter().enumerate() {
            if dead[k] {
                out.extend_from_slice(&self.code[from..at as usize]);
                from = at as usize + len as usize;
                removed += len as u32;
                cut.push((at, removed));
            }
        }
        out.extend_from_slice(&self.code[from..]);
        let shift = |r: u32| cut.iter().take_while(|&&(at, _)| at < r).last().map_or(0, |&(_, n)| n);
        for r in self.relocs.iter_mut() {
            *r -= shift(*r);
        }
        self.code = out;
        self.last_set = None;
    }

    /// Fold `local.set x; local.get x` into `local.tee x` from here on
    /// (off by default, which keeps tier-1's bytes as they were).
    pub fn fold_tees(&mut self) {
        self.fold = true;
    }

    pub fn local_tee(&mut self, idx: u32) {
        let at = self.code.len();
        self.byte(op::LOCAL_TEE);
        self.u32(idx);
        self.log_local(at, idx);
    }

    pub fn i32_const(&mut self, v: i32) {
        self.byte(op::I32_CONST);
        write_i32(&mut self.code, v);
    }

    pub fn i64_const(&mut self, v: i64) {
        self.byte(op::I64_CONST);
        write_i64(&mut self.code, v);
    }

    /// Push the constant guest address `a` as the memory's index type: i32
    /// for a wasm32 guest memory, i64 for a memory64 one (docs/mem64.md).
    pub fn addr_const(&mut self, a: u64) {
        if gex_abi::mem64() {
            self.i64_const(a as i64);
        } else {
            self.i32_const(a as u32 as i32);
        }
    }

    /// Address arithmetic in the memory's index type (docs/mem64.md). For
    /// a wasm32 memory each emits exactly the i32 instruction tier-1 always
    /// has, so wasm32 modules are unchanged.
    ///
    /// `+ k`.
    pub fn addr_add_const(&mut self, k: u32) {
        if gex_abi::mem64() {
            self.i64_const(k as i64);
            self.byte(0x7c); // i64.add
        } else {
            self.i32_const(k as i32);
            self.byte(0x6a); // i32.add
        }
    }

    /// `& k`.
    pub fn addr_and_const(&mut self, k: i64) {
        if gex_abi::mem64() {
            self.i64_const(k);
            self.byte(0x83); // i64.and
        } else {
            self.i32_const(k as i32);
            self.byte(0x71); // i32.and
        }
    }

    /// `<u` of the two addresses on the stack, an i32 result.
    pub fn addr_lt_u(&mut self) {
        self.byte(if gex_abi::mem64() { 0x54 } else { 0x49 });
    }

    /// The address on the stack as an i64 value (for a host call).
    pub fn addr_to_i64(&mut self) {
        if !gex_abi::mem64() {
            self.byte(0xad); // i64.extend_i32_u
        }
    }

    /// The low 32 bits of the address on the stack, for alignment arithmetic.
    pub fn addr_low32(&mut self) {
        if gex_abi::mem64() {
            self.byte(0xa7); // i32.wrap_i64
        }
    }

    /// Make the i32 function-table index on top of the stack `gex.ftab`'s
    /// index type: a memory64 process's `ftab` is a table64, as clang's
    /// wasm64 units import it (docs/mem64.md §4).
    pub fn ftab_index(&mut self) {
        if gex_abi::mem64() {
            self.byte(0xad); // i64.extend_i32_u
        }
    }

    /// Make the i32 address on top of the stack the memory's index type:
    /// nothing for a wasm32 guest memory, a zero-extension for memory64. The
    /// context pointer (`ctx`, an i32 in every ABI) and the runtime's low
    /// addresses are such i32s.
    pub fn addr_i32(&mut self) {
        if gex_abi::mem64() {
            self.byte(0xad); // i64.extend_i32_u
        }
    }

    pub fn call(&mut self, func: u32) {
        self.byte(op::CALL);
        self.u32(func);
    }

    pub fn return_call(&mut self, func: u32) {
        self.byte(op::RETURN_CALL);
        self.u32(func);
    }

    /// A tail call to another block in this same module.
    ///
    /// Distinguished from [`Self::return_call`] only so the index can be
    /// recorded and written at a fixed width: five bytes, which is the most a
    /// `u32` LEB can take, so the merger may add an offset to it without the
    /// body changing length. Non-canonical LEB is valid wasm and is decoded
    /// once, when the module is compiled.
    pub fn return_call_local(&mut self, func: u32) {
        self.byte(op::RETURN_CALL);
        self.relocs.push(self.code.len() as u32);
        write_u32_fixed(&mut self.code, func);
    }

    /// Where this body names a local function, relative to the start of its
    /// code.
    pub fn relocs(&self) -> &[u32] {
        &self.relocs
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

    /// A load from memory `mem` (multi-memory). Memory 0 encodes exactly as
    /// [`Self::load`] does.
    pub fn load_m(&mut self, opcode: u8, align_log2: u32, mem: u32, offset: u32) {
        self.byte(opcode);
        self.memarg_m(align_log2, mem, offset);
    }

    /// A store to memory `mem` (multi-memory).
    pub fn store_m(&mut self, opcode: u8, align_log2: u32, mem: u32, offset: u32) {
        self.byte(opcode);
        self.memarg_m(align_log2, mem, offset);
    }

    /// A memory argument naming a memory: bit 6 of the alignment field says
    /// an index follows.
    fn memarg_m(&mut self, align_log2: u32, mem: u32, offset: u32) {
        if mem == 0 {
            self.memarg(align_log2, offset);
        } else {
            self.u32(align_log2 | 0x40);
            self.u32(mem);
            self.u32(offset);
        }
    }

    pub fn global_get(&mut self, idx: u32) {
        self.byte(op::GLOBAL_GET);
        self.u32(idx);
    }

    pub fn global_set(&mut self, idx: u32) {
        self.byte(op::GLOBAL_SET);
        self.u32(idx);
    }

    /// `memory.init seg mem`: copy from a passive data segment.
    pub fn memory_init(&mut self, seg: u32, mem: u32) {
        self.byte(op::MISC_PREFIX);
        self.u32(8);
        self.u32(seg);
        self.u32(mem);
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

    /// Encode, and say where in the encoding each local-function immediate
    /// ended up.
    ///
    /// The offsets [`Self::relocs`] holds are into `code`; the encoding puts a
    /// size prefix and the local declarations in front of it, so they all move
    /// by the same amount.
    pub fn finish_with_relocs(mut self) -> (Vec<u8>, Vec<u32>) {
        let mut relocs = std::mem::take(&mut self.relocs);
        let code_len = self.code.len();
        let out = self.finish();
        let shift = (out.len() - code_len - 1) as u32; // -1 for the trailing `end`
        for r in &mut relocs {
            *r += shift;
        }
        (out, relocs)
    }

    /// Serialise the body into the code-section entry form.
    pub fn finish(self) -> Vec<u8> {
        // Run-length encode the local declarations.
        let mut runs = 0u32;
        let mut decl = Vec::with_capacity(32);
        let mut i = 0;
        while i < self.locals.len() {
            let ty = self.locals[i];
            let mut j = i + 1;
            while j < self.locals.len() && self.locals[j] == ty {
                j += 1;
            }
            write_u32(&mut decl, (j - i) as u32);
            decl.push(ty.byte());
            runs += 1;
            i = j;
        }
        let mut head = Vec::with_capacity(5);
        write_u32(&mut head, runs);
        // Once, at its final size: the code was copied twice before (into a
        // body, then behind its length), each a fresh allocation.
        let body_len = head.len() + decl.len() + self.code.len() + 1;
        let mut out = Vec::with_capacity(body_len + 5);
        write_u32(&mut out, body_len as u32);
        out.extend_from_slice(&head);
        out.extend_from_slice(&decl);
        out.extend_from_slice(&self.code);
        out.push(op::END);
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
        /// A memory64 (64-bit index) memory.
        mem64: bool,
    },
    /// A funcref table with the given minimum size.
    Table {
        min: u32,
    },
    /// An immutable global of the given type.
    Global {
        ty: ValType,
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
    /// The base slot of the page this segment belongs to, as `gexc` numbered
    /// it. `offset - page_base` is where this segment starts *within* its
    /// page, and that distance is all a fill needs to be told: given the base
    /// the page has in the process doing the filling, it can write there
    /// instead of where the build happened to put it.
    pub page_base: u32,
    pub funcs: Vec<u32>,
    /// Filled by a call rather than by instantiation.
    ///
    /// An active segment writes its funcrefs on every instantiation, once per
    /// guest process, mostly for code the process never runs. A passive
    /// segment writes nothing until the host calls `gexfill` -- the seam a
    /// demand-filled table needs.
    pub passive: bool,
    /// The guest page this segment covers -- `pc / DISPATCH_PAGE` -- or zero
    /// if it covers no single page.
    ///
    /// What `gexfillat` matches on, deliberately the guest page rather than a
    /// dispatch entry: a miss always knows the address it faulted on, but not
    /// always an entry, which exists only once some module has claimed the
    /// page. Keying on an entry left every other miss re-translating code the
    /// closure already held.
    pub guest_page: u32,
    /// The offset is written as a five-byte LEB, so a host can rewrite it in
    /// place (`crate::dense::set_base`).
    pub padded: bool,
    /// The offset is this (imported) global's value instead of `offset`:
    /// given at instantiation, so the module's bytes do not depend on it.
    pub offset_global: Option<u32>,
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
    /// For each body, offsets within it that name a local function.
    ///
    /// Emitted as a custom section so a unit carries its own relocations:
    /// the store keeps wasm and nothing else, and an engine ignores a
    /// section it does not know.
    body_relocs: Vec<Vec<u32>>,
    /// `(min, max)` in elements for each locally defined dispatch table, in
    /// index order.
    tables: Vec<(u32, u32)>,
    /// `(min, max)` in pages.
    memory: Option<(u64, u64)>,
    exports: Vec<(String, ExportKind, u32)>,
    elements: Vec<ElementSegment>,
    data: Vec<DataSegment>,
    /// Passive data segments, numbered before the active ones (indices
    /// `0..passive.len()`), for `memory.init`.
    passive: Vec<Vec<u8>>,
    /// Defined globals: type, mutability and an integer initialiser.
    globals: Vec<(ValType, bool, i64)>,
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

    /// Import a memory instead of defining one.
    ///
    /// A defined memory belongs to its instance, so two stores would get two
    /// of them; every guest thread must see the same bytes, so the host owns
    /// the memory and every module imports it. `min` is what the host must
    /// allocate, `max` how far it may grow.
    pub fn import_memory(&mut self, module: &str, name: &str, min: u64, max: u64, shared: bool) {
        self.imports.push(Import {
            module: module.to_string(),
            name: name.to_string(),
            kind: ImportKind::Memory { min, max, shared, mem64: false },
        });
    }

    /// Import the guest memory: shared, and a memory64 one of
    /// [`gex_abi::LIMIT64_PAGES`] in an M64 process (docs/mem64.md), else
    /// `min..max` pages of wasm32.
    pub fn import_guest_memory(&mut self, module: &str, name: &str, min: u64, max: u64) {
        let mem64 = gex_abi::mem64();
        let max = if mem64 { gex_abi::LIMIT64_PAGES } else { max };
        self.imports.push(Import {
            module: module.to_string(),
            name: name.to_string(),
            kind: ImportKind::Memory { min, max, shared: true, mem64 },
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

    /// Import an immutable global, returning its index. Imported globals
    /// come before defined ones, so this must not be mixed with
    /// [`Module::add_global`].
    pub fn import_global(&mut self, module: &str, name: &str, ty: ValType) -> u32 {
        assert!(self.globals.is_empty(), "a global import after a defined global");
        let n = self.imports.iter().filter(|i| matches!(i.kind, ImportKind::Global { .. })).count() as u32;
        self.imports.push(Import {
            module: module.to_string(),
            name: name.to_string(),
            kind: ImportKind::Global { ty },
        });
        n
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
        self.body_relocs.push(Vec::new());
        self.n_imports() + self.functions.len() as u32 - 1
    }

    /// Supply the body of a previously declared function.
    pub fn define_func(&mut self, index: u32, body: Vec<u8>) {
        let local = (index - self.n_imports()) as usize;
        self.bodies[local] = body;
    }

    /// As [`Self::define_func`], recording where the body names a local
    /// function so the merger can find those immediates without decoding
    /// anything. See [`crate::merge`].
    pub fn define_func_with_relocs(&mut self, index: u32, body: Vec<u8>, relocs: Vec<u32>) {
        let local = (index - self.n_imports()) as usize;
        self.bodies[local] = body;
        self.body_relocs[local] = relocs;
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

    /// Declare, define and export `gexfillall`, which writes every one of this
    /// module's passive segments in a single call.
    ///
    /// One call, not one per segment: a module has hundreds of segments and a
    /// crossing into wasm for each is thousands of JS calls. This is what an
    /// eager fill uses; the per-page form is for the demand path.
    pub fn add_fill_all(&mut self, void_ty: u32) -> Option<u32> {
        let passive: Vec<(u32, u32, u32, u32)> = self
            .elements
            .iter()
            .enumerate()
            .filter(|(_, e)| e.passive)
            .map(|(i, e)| (i as u32, e.table, e.offset, e.funcs.len() as u32))
            .collect();
        if passive.is_empty() {
            return None;
        }
        let idx = self.declare_func(void_ty);
        let mut b = Vec::new();
        b.push(0x00); // no locals
        for (seg, table, offset, len) in &passive {
            b.push(op::I32_CONST);
            write_i32(&mut b, *offset as i32);
            b.push(op::I32_CONST);
            write_i32(&mut b, 0);
            b.push(op::I32_CONST);
            write_i32(&mut b, *len as i32);
            b.push(op::MISC_PREFIX);
            write_u32(&mut b, 12); // table.init
            write_u32(&mut b, *seg);
            write_u32(&mut b, *table);
        }
        b.push(op::END);
        let mut sized = Vec::with_capacity(b.len() + 5);
        write_u32(&mut sized, b.len() as u32);
        sized.extend_from_slice(&b);
        self.define_func(idx, sized);
        self.export(gex_abi::exports::FILL_ALL, ExportKind::Func, idx);
        Some(idx)
    }

    /// Declare, define and export `gexfillat`, which writes whichever of this
    /// module's passive segments covers a given guest page.
    ///
    /// The companion to `gexfill` and the one a demand-filled table wants: a
    /// miss knows a page, not a segment index, and which segment covers it is
    /// the module's business. A linear scan over the segments, once per page
    /// actually branched into. Answers 1 if this module filled a segment, 0 if
    /// the page is somebody else's.
    pub fn add_fill_at(&mut self, ty: u32) -> Option<u32> {
        let passive: Vec<(u32, u32, u32, u32, u32)> = self
            .elements
            .iter()
            .enumerate()
            .filter(|(_, e)| e.passive && e.guest_page != 0)
            .map(|(i, e)| {
                (
                    i as u32,
                    e.table,
                    // Where this segment starts inside its page, rather than
                    // the absolute slot the build chose.
                    e.offset.wrapping_sub(e.page_base),
                    e.funcs.len() as u32,
                    e.guest_page,
                )
            })
            .collect();
        if passive.is_empty() {
            return None;
        }
        let idx = self.declare_func(ty);
        let mut b = Vec::new();
        // Three i32 locals: the first slot this call writes and the last, both
        // clamped to what the segment actually covers (locals 3 and 4, after the
        // three parameters), and a "page was mine" flag (local 5). The flag,
        // rather than an early `return 1` on the first matching segment, because a
        // page is now covered by one segment per contiguous run of blocks -- gaps
        // hold demand-translated blocks a stub must not erase -- and every run on
        // the page has to be filled, not just the first found.
        b.push(0x01);
        write_u32(&mut b, 3);
        b.push(0x7f); // i32
        for (seg, table, within, len, guest_page) in &passive {
            // if (page == guest_page): exact page equality, not a range test
            // on `offset` -- `offset` is the slot of this segment's first
            // block, not the page base, and carries no table index, so a range
            // test could miss the page or match a different table's.
            b.push(op::LOCAL_GET);
            write_u32(&mut b, 0);
            b.push(op::I32_CONST);
            write_i32(&mut b, *guest_page as i32);
            b.push(0x46); // i32.eq
            b.push(op::IF);
            b.push(0x40); // void
            // A span around the slot that missed, not the whole page: `from`
            // is where the caller wants writing to start, this segment covers
            // `[within, within + len)`, so write the intersection
            //
            //     s = max(from, within)
            //     e = min(from + FILL_SPAN, within + len)
            //
            // and nothing when the span misses. `table.init` materialises a
            // funcref per entry, so writing the whole page is dear.
            let span = gex_abi::FILL_SPAN as i32;
            let end = (*within as i32).wrapping_add(*len as i32);
            // s = (from > within) ? from : within
            b.push(op::LOCAL_GET);
            write_u32(&mut b, 2);
            b.push(op::I32_CONST);
            write_i32(&mut b, *within as i32);
            b.push(op::LOCAL_GET);
            write_u32(&mut b, 2);
            b.push(op::I32_CONST);
            write_i32(&mut b, *within as i32);
            b.push(0x4b); // i32.gt_u
            b.push(0x1b); // select
            b.push(0x21); // local.set
            write_u32(&mut b, 3);
            // e = (from + span < within + len) ? from + span : within + len
            b.push(op::LOCAL_GET);
            write_u32(&mut b, 2);
            b.push(op::I32_CONST);
            write_i32(&mut b, span);
            b.push(0x6a); // i32.add
            b.push(op::I32_CONST);
            write_i32(&mut b, end);
            b.push(op::LOCAL_GET);
            write_u32(&mut b, 2);
            b.push(op::I32_CONST);
            write_i32(&mut b, span);
            b.push(0x6a); // i32.add
            b.push(op::I32_CONST);
            write_i32(&mut b, end);
            b.push(0x49); // i32.lt_u
            b.push(0x1b); // select
            b.push(0x21); // local.set
            write_u32(&mut b, 4);
            // if (e > s) table.init(dest = base + s, src = s - within, e - s)
            b.push(op::LOCAL_GET);
            write_u32(&mut b, 4);
            b.push(op::LOCAL_GET);
            write_u32(&mut b, 3);
            b.push(0x4b); // i32.gt_u
            b.push(op::IF);
            b.push(0x40); // void
            b.push(op::LOCAL_GET);
            write_u32(&mut b, 1);
            b.push(op::LOCAL_GET);
            write_u32(&mut b, 3);
            b.push(0x6a); // i32.add
            b.push(op::LOCAL_GET);
            write_u32(&mut b, 3);
            b.push(op::I32_CONST);
            write_i32(&mut b, *within as i32);
            b.push(0x6b); // i32.sub
            b.push(op::LOCAL_GET);
            write_u32(&mut b, 4);
            b.push(op::LOCAL_GET);
            write_u32(&mut b, 3);
            b.push(0x6b); // i32.sub
            b.push(op::MISC_PREFIX);
            write_u32(&mut b, 12); // table.init
            write_u32(&mut b, *seg);
            write_u32(&mut b, *table);
            b.push(op::END); // if (e > s)
            // The page was this module's, whether or not this run intersected the
            // span. Remembered, not returned: a later run on the same page still
            // has to fill.
            b.push(op::I32_CONST);
            write_i32(&mut b, 1);
            b.push(0x21); // local.set
            write_u32(&mut b, 5);
            b.push(op::END); // if (page == guest_page)
        }
        // 1 if any run on the page was filled, 0 if the page is somebody else's.
        b.push(op::LOCAL_GET);
        write_u32(&mut b, 5);
        b.push(op::END); // the function
        let mut sized = Vec::with_capacity(b.len() + 5);
        write_u32(&mut sized, b.len() as u32);
        sized.extend_from_slice(&b);
        self.define_func(idx, sized);
        self.export(gex_abi::exports::FILL_AT, ExportKind::Func, idx);
        Some(idx)
    }

    /// Declare, define and export `gexfill`, which writes this module's
    /// passive element segments into the tables it imports.
    ///
    /// `table.init` is the only instruction that can write a funcref into an
    /// imported table, so a passive segment has to be filled from inside the
    /// module. `gexfill(seg)` fills one segment whole, chosen by a `br_table`,
    /// and returns 1 if there was such a segment and 0 if `seg` is past the end
    /// -- the flag is there because the host cannot guess the segment count.
    ///
    /// Returns the exported function's index, or `None` when the module has no
    /// passive segments and needs no filling.
    pub fn add_fill(&mut self, void_ty: u32) -> Option<u32> {
        let passive: Vec<(u32, u32, u32, u32)> = self
            .elements
            .iter()
            .enumerate()
            .filter(|(_, e)| e.passive)
            .map(|(i, e)| (i as u32, e.table, e.offset, e.funcs.len() as u32))
            .collect();
        if passive.is_empty() {
            return None;
        }
        let idx = self.declare_func(void_ty);
        let mut b = Vec::new();
        b.push(0x00); // no locals
        // One block per segment, closed in order, so `br_table` can pick.
        for _ in 0..=passive.len() {
            b.push(op::BLOCK);
            b.push(0x40); // void
        }
        b.push(op::LOCAL_GET);
        write_u32(&mut b, 0);
        b.push(op::BR_TABLE);
        write_u32(&mut b, passive.len() as u32);
        for i in 0..passive.len() {
            write_u32(&mut b, i as u32);
        }
        write_u32(&mut b, passive.len() as u32); // default: fall out, do nothing
        for (seg, table, offset, len) in &passive {
            b.push(op::END);
            b.push(op::I32_CONST);
            write_i32(&mut b, *offset as i32);
            b.push(op::I32_CONST);
            write_i32(&mut b, 0);
            b.push(op::I32_CONST);
            write_i32(&mut b, *len as i32);
            b.push(op::MISC_PREFIX);
            write_u32(&mut b, 12); // table.init
            write_u32(&mut b, *seg);
            write_u32(&mut b, *table);
            b.push(op::I32_CONST);
            write_i32(&mut b, 1);
            b.push(op::RETURN);
        }
        b.push(op::END); // the outermost block
        b.push(op::I32_CONST);
        write_i32(&mut b, 0);
        b.push(op::END); // the function
        // A body carries its own length prefix; the code section writes each
        // one verbatim. Leaving it off makes the section shorter than its
        // header claims and the module fails to load.
        let mut sized = Vec::with_capacity(b.len() + 5);
        write_u32(&mut sized, b.len() as u32);
        sized.extend_from_slice(&b);
        self.define_func(idx, sized);
        self.export(gex_abi::exports::FILL, ExportKind::Func, idx);
        Some(idx)
    }

    pub fn data(&mut self, seg: DataSegment) {
        self.data.push(seg);
    }

    /// Add a passive data segment, returning its index. Passive segments are
    /// numbered first, so the index is fixed whatever active data follows.
    pub fn passive_data(&mut self, bytes: Vec<u8>) -> u32 {
        self.passive.push(bytes);
        self.passive.len() as u32 - 1
    }

    /// Define a global (none are imported), returning its index.
    pub fn add_global(&mut self, ty: ValType, mutable: bool, init: i64) -> u32 {
        self.globals.push((ty, mutable, init));
        self.globals.len() as u32 - 1
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
                    ImportKind::Memory { min, max, shared, mem64 } => {
                        s.push(0x02);
                        // Limits flags: bit 0 says a maximum follows, bit 1
                        // shared, bit 2 a 64-bit index. A shared memory must
                        // declare a maximum.
                        let mut flags = 0x01;
                        if *shared {
                            flags |= 0x02;
                        }
                        if *mem64 {
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
                    ImportKind::Global { ty } => {
                        s.push(0x03);
                        s.push(ty.byte());
                        s.push(0x00); // immutable
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

        // 6: globals
        if !self.globals.is_empty() {
            let mut s = Vec::new();
            write_u32(&mut s, self.globals.len() as u32);
            for (ty, mutable, init) in &self.globals {
                s.push(ty.byte());
                s.push(*mutable as u8);
                match ty {
                    ValType::I64 => {
                        s.push(op::I64_CONST);
                        write_i64(&mut s, *init);
                    }
                    _ => {
                        s.push(op::I32_CONST);
                        write_i32(&mut s, *init as i32);
                    }
                }
                s.push(op::END);
            }
            section(&mut out, 6, &s);
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
                if e.passive {
                    // Form 1: passive, elemkind funcref, a vector of indices.
                    s.push(0x01);
                    s.push(0x00);
                } else if e.table == 0 {
                    s.push(0x00);
                    s.push(op::I32_CONST);
                    write_i32(&mut s, e.offset as i32);
                    s.push(op::END);
                } else if let Some(g) = e.offset_global {
                    s.push(0x02);
                    write_u32(&mut s, e.table);
                    s.push(0x23); // global.get
                    write_u32(&mut s, g);
                    s.push(op::END);
                    s.push(0x00); // elemkind: funcref
                } else {
                    s.push(0x02);
                    write_u32(&mut s, e.table);
                    s.push(op::I32_CONST);
                    if e.padded {
                        let mut v = e.offset as i32;
                        for i in 0..5 {
                            let byte = (v & 0x7f) as u8;
                            v >>= 7;
                            s.push(if i < 4 { byte | 0x80 } else { byte & 0x7f });
                        }
                    } else {
                        write_i32(&mut s, e.offset as i32);
                    }
                    s.push(op::END);
                    s.push(0x00); // elemkind: funcref
                }
                write_u32(&mut s, e.funcs.len() as u32);
                for f in &e.funcs {
                    write_u32(&mut s, *f);
                }
            }
            section(&mut out, 9, &s);
        }

        // 12: data count, required before the code once `memory.init` names a
        // segment. Only a module with passive data has one.
        if !self.passive.is_empty() {
            let mut s = Vec::new();
            write_u32(&mut s, (self.passive.len() + self.data.len()) as u32);
            section(&mut out, 12, &s);
        }

        // 10: code
        let mut code_relocs: Vec<u32> = Vec::new();
        if !self.bodies.is_empty() {
            let mut s = Vec::new();
            write_u32(&mut s, self.bodies.len() as u32);
            for (i, b) in self.bodies.iter().enumerate() {
                // Where this body starts inside the section payload, so the
                // recorded offsets become payload-relative and the merger can
                // patch without knowing anything about bodies.
                let base = s.len() as u32;
                for r in &self.body_relocs[i] {
                    code_relocs.push(base + r);
                }
                s.extend_from_slice(b);
            }
            section(&mut out, 10, &s);
        }

        // 11: data
        if !self.data.is_empty() || !self.passive.is_empty() {
            let mut s = Vec::new();
            write_u32(&mut s, (self.passive.len() + self.data.len()) as u32);
            for p in &self.passive {
                s.push(0x01); // passive
                write_u32(&mut s, p.len() as u32);
                s.extend_from_slice(p);
            }
            // The offset of an active data segment is a constant expression
            // typed by the memory it lands in.
            let mem64 = self
                .imports
                .iter()
                .find_map(|i| match i.kind {
                    ImportKind::Memory { mem64, .. } => Some(mem64),
                    _ => None,
                })
                .unwrap_or(false);
            for d in &self.data {
                s.push(0x00); // active, memory 0
                if mem64 {
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

        // Where this module names its own functions. A custom section, so it
        // travels with the bytes -- the shared store keeps wasm and nothing
        // else -- and an engine ignores it.
        //
        // Not in a module with a dense segment (placed by an imported global,
        // `crate::dense`): `merge` refuses those, so nothing would read it,
        // and it was 3.5% of what node's start-up translates (340 B a unit).
        let unmergeable = self.elements.iter().any(|e| e.offset_global.is_some());
        if !code_relocs.is_empty() && !unmergeable {
            let mut sec = Vec::with_capacity(code_relocs.len() * 4 + 16);
            write_name(&mut sec, RELOC_SECTION);
            write_u32(&mut sec, code_relocs.len() as u32);
            for r in &code_relocs {
                sec.extend_from_slice(&r.to_le_bytes());
            }
            section(&mut out, 0, &sec);
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
