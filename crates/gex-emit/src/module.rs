//! Assembling translated blocks into a loadable wasm module.
//!
//! The module's shape is fixed by the runtime ABI in `gex-abi`:
//!
//! * one linear memory that *is* the guest address space (PLAN §2.3);
//! * one funcref table indexed by `(pc - text_base) >> 2`, every slot filled
//!   — translated blocks where we have them, a miss stub everywhere else, so
//!   a branch to an untranslated address produces a precise diagnostic
//!   instead of an "uninitialized element" trap;
//! * an `enter(ctx, pc)` export the host calls to start execution.

use crate::tier1;
use crate::wasm::{DataSegment, ElementSegment, ExportKind, FuncBody, FuncType, Module, ValType};
use gex_abi::{ctx, exports, imports, GUEST_MEM_PAGES, IMPORT_MODULE};
use gex_ir::Block;
use std::collections::HashMap;

/// A finished translation unit.
pub struct TranslatedModule {
    /// The encoded wasm module.
    pub wasm: Vec<u8>,
    /// Guest address the dispatch table is indexed from.
    pub text_base: u64,
    /// Number of dispatch-table slots.
    pub text_slots: u64,
    /// Number of guest basic blocks translated.
    pub n_blocks: usize,
}

/// Builds a wasm module from GIR blocks.
pub struct ModuleBuilder {
    m: Module,
    text_base: u64,
    text_slots: u64,
    block_ty: u32,
    miss_stub: u32,
    /// Guest block address -> wasm function index.
    blocks: HashMap<u64, u32>,
    /// Blocks awaiting code generation, paired with their function index.
    pending: Vec<(Block, u32)>,
    imports: Imports,
    data: Vec<DataSegment>,
}

#[derive(Clone, Copy)]
struct Imports {
    syscall: u32,
    dispatch_miss: u32,
    illegal: u32,
    breakpoint: u32,
    sysreg_read: u32,
    sysreg_write: u32,
    cache_op: u32,
    safepoint: u32,
}

impl ModuleBuilder {
    /// Start a module covering guest text `[text_base, text_base + text_size)`.
    pub fn new(text_base: u64, text_size: u64) -> ModuleBuilder {
        assert_eq!(text_base % 4, 0, "text base must be instruction-aligned");
        let text_slots = text_size.div_ceil(4);

        let mut m = Module::new();
        let block_ty = m.func_type(FuncType::new(vec![ValType::I32], vec![]));
        let t_syscall = m.func_type(FuncType::new(vec![ValType::I32, ValType::I64], vec![]));
        let t_miss = m.func_type(FuncType::new(
            vec![ValType::I32, ValType::I64],
            vec![ValType::I32],
        ));
        let t_pc_i32 = m.func_type(FuncType::new(
            vec![ValType::I32, ValType::I64, ValType::I32],
            vec![],
        ));
        let t_sysreg_read = m.func_type(FuncType::new(
            vec![ValType::I32, ValType::I64, ValType::I32],
            vec![ValType::I64],
        ));
        let t_pc_i32_i64 = m.func_type(FuncType::new(
            vec![ValType::I32, ValType::I64, ValType::I32, ValType::I64],
            vec![],
        ));

        let im = IMPORT_MODULE;
        let imports = Imports {
            syscall: m.import_func(im, imports::SYSCALL, t_syscall),
            dispatch_miss: m.import_func(im, imports::DISPATCH_MISS, t_miss),
            illegal: m.import_func(im, imports::ILLEGAL, t_pc_i32),
            breakpoint: m.import_func(im, imports::BREAKPOINT, t_pc_i32),
            sysreg_read: m.import_func(im, imports::SYSREG_READ, t_sysreg_read),
            sysreg_write: m.import_func(im, imports::SYSREG_WRITE, t_pc_i32_i64),
            cache_op: m.import_func(im, imports::CACHE_OP, t_pc_i32_i64),
            safepoint: m.import_func(im, imports::SAFEPOINT, t_syscall),
        };

        m.set_memory(GUEST_MEM_PAGES, GUEST_MEM_PAGES);
        m.set_table(text_slots as u32, text_slots as u32);

        // The miss stub and `enter` are the first two local functions, so
        // their indices are known before any block is declared.
        let miss_stub = m.declare_func(block_ty);
        let enter = m.declare_func(t_syscall);

        let mut b = ModuleBuilder {
            m,
            text_base,
            text_slots,
            block_ty,
            miss_stub,
            blocks: HashMap::new(),
            pending: Vec::new(),
            imports,
            data: Vec::new(),
        };

        let stub_body = b.build_miss_stub();
        b.m.define_func(miss_stub, stub_body);
        let enter_body = b.build_enter();
        b.m.define_func(enter, enter_body);

        b.m.export(exports::MEMORY, ExportKind::Memory, 0);
        b.m.export(exports::DISPATCH_TABLE, ExportKind::Table, 0);
        b.m.export(exports::ENTER, ExportKind::Func, enter);
        b
    }

    /// Reserve a function index for a block starting at `addr`.
    ///
    /// Indices are handed out before code generation so a block can
    /// `return_call` a successor that has not been compiled yet.
    pub fn reserve(&mut self, addr: u64) -> u32 {
        if let Some(&f) = self.blocks.get(&addr) {
            return f;
        }
        let f = self.m.declare_func(self.block_ty);
        self.blocks.insert(addr, f);
        f
    }

    /// Add a translated block, reserving its index if necessary.
    pub fn add_block(&mut self, block: Block) {
        let f = self.reserve(block.start);
        self.pending.push((block, f));
    }

    /// True when `addr` already has a reserved function index.
    pub fn has_block(&self, addr: u64) -> bool {
        self.blocks.contains_key(&addr)
    }

    /// Add an initialiser for guest memory.
    pub fn add_data(&mut self, offset: u64, bytes: Vec<u8>) {
        self.data.push(DataSegment { offset, bytes });
    }

    /// Generate all block bodies and encode the module.
    pub fn finish(mut self) -> TranslatedModule {
        let compiled: Vec<(u32, Vec<u8>)> = {
            let cx = tier1::Context {
                syscall: self.imports.syscall,
                illegal: self.imports.illegal,
                breakpoint: self.imports.breakpoint,
                sysreg_read: self.imports.sysreg_read,
                sysreg_write: self.imports.sysreg_write,
                cache_op: self.imports.cache_op,
                dispatch_miss: self.imports.dispatch_miss,
                safepoint: self.imports.safepoint,
                block_ty: self.block_ty,
                text_base: self.text_base,
                text_slots: self.text_slots,
                blocks: &self.blocks,
            };
            self.pending
                .iter()
                .map(|(block, f)| (*f, tier1::compile(block, &cx)))
                .collect()
        };
        let n_blocks = compiled.len();
        for (f, body) in compiled {
            self.m.define_func(f, body);
        }

        // Fill every dispatch slot: a translated block where one starts, the
        // miss stub everywhere else.
        let mut funcs = vec![self.miss_stub; self.text_slots as usize];
        for (&addr, &f) in &self.blocks {
            let d = addr.wrapping_sub(self.text_base);
            if d % 4 == 0 && (d / 4) < self.text_slots {
                funcs[(d / 4) as usize] = f;
            }
        }
        self.m.element(ElementSegment {
            table: 0,
            offset: 0,
            funcs,
        });

        for d in std::mem::take(&mut self.data) {
            self.m.data(d);
        }

        TranslatedModule {
            wasm: self.m.encode(),
            text_base: self.text_base,
            text_slots: self.text_slots,
            n_blocks,
        }
    }

    // -- fixed runtime functions -------------------------------------------

    /// `(ctx) -> ()`: the slot every untranslated address points at.
    ///
    /// It reads the PC the branching code recorded in the context, hands it
    /// to the host, and tail-calls whatever slot the host returns. In M0 the
    /// host always raises an error; from M3 it translates and patches.
    fn build_miss_stub(&self) -> Vec<u8> {
        let mut f = FuncBody::new(1);
        f.local_get(0); // ctx argument for the eventual target
        f.local_get(0);
        f.local_get(0);
        f.load(0x29, 3, ctx::PC as u32); // i64.load ctx.pc
        f.call(self.imports.dispatch_miss);
        f.return_call_indirect(self.block_ty, 0);
        f.finish()
    }

    /// `(ctx, pc) -> ()`: the host's entry point into translated code.
    fn build_enter(&self) -> Vec<u8> {
        let mut f = FuncBody::new(2);
        let scratch = f.add_local(ValType::I64);

        // Record the entry PC so a miss inside `enter` reports it correctly.
        f.local_get(0);
        f.local_get(1);
        f.store(0x37, 3, ctx::PC as u32);

        f.local_get(0); // ctx argument for the target block

        // d = pc - text_base
        f.local_get(1);
        f.i64_const(self.text_base as i64);
        f.byte(0x7d); // i64.sub
        f.local_set(scratch);

        // in_range = d <u slots*4 && (d & 3) == 0
        f.local_get(scratch);
        f.i64_const((self.text_slots * 4) as i64);
        f.byte(0x54); // i64.lt_u
        f.local_get(scratch);
        f.i64_const(3);
        f.byte(0x83); // i64.and
        f.byte(0x50); // i64.eqz
        f.byte(0x71); // i32.and

        f.if_result(ValType::I32);
        f.local_get(scratch);
        f.i64_const(2);
        f.byte(0x88); // i64.shr_u
        f.byte(0xa7); // i32.wrap_i64
        f.else_();
        f.local_get(0);
        f.local_get(1);
        f.call(self.imports.dispatch_miss);
        f.end();

        f.return_call_indirect(self.block_ty, 0);
        f.finish()
    }
}
