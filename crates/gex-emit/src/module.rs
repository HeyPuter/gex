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
use gex_abi::{
    ctx, dispatch_page, exports, imports, DISPATCH_DIR, DISPATCH_MAX_PAGES, DISPATCH_PAGE,
    GUEST_MEM_PAGES, IMPORT_MODULE, MISS_SLOTS, SLOTS_PER_PAGE,
};
use gex_ir::Block;
use std::collections::HashMap;

/// Where a module's memory and dispatch table come from.
///
/// The image module declares both. A module produced at run time imports
/// them instead, so its blocks read the same guest memory and branch through
/// the same table as everything already translated.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Linkage {
    /// Declare and export the memory and table.
    Own,
    /// Import them from the already-instantiated image module.
    Shared,
}

/// A finished translation unit.
pub struct TranslatedModule {
    /// The encoded wasm module.
    pub wasm: Vec<u8>,
    /// Guest address the module's text starts at.
    pub text_base: u64,
    /// Number of dispatch-table slots this module's table needs.
    pub table_slots: u64,
    /// Pages this module was the first to claim, as `(page, base slot)`. The
    /// host publishes them into the guest's dispatch directory.
    pub dir_entries: Vec<(u64, u32)>,
    /// The updated page assignment, to carry into the next translation.
    pub page_table: PageTable,
    /// Number of guest basic blocks translated.
    pub n_blocks: usize,
    /// For a [`Linkage::Shared`] module, the blocks it defines and the export
    /// name each is reachable under, so the host can install them in the
    /// table.
    pub exported_blocks: Vec<(u64, String)>,
}

impl TranslatedModule {
    /// The part of this translation a cached module still needs.
    pub fn metadata(&self) -> Metadata {
        Metadata {
            text_base: self.text_base,
            table_slots: self.table_slots,
            dir_entries: self.dir_entries.clone(),
            next_slot: self.page_table.slots(),
            n_blocks: self.n_blocks,
        }
    }
}

/// Global assignment of guest pages to dispatch-table slots.
///
/// This has to be process-wide, not per-module: a page's directory entry is a
/// single value in guest memory, so if two modules each picked their own base
/// for the same page the second would orphan the first's blocks and they
/// would be translated again on the next branch.
#[derive(Debug, Default, Clone)]
pub struct PageTable {
    pages: std::collections::BTreeMap<u64, u32>,
    next_slot: u64,
}

impl PageTable {
    /// Start allocating above the reserved miss span.
    pub fn new() -> PageTable {
        PageTable {
            pages: Default::default(),
            next_slot: MISS_SLOTS,
        }
    }

    /// The base slot for a page, allocating one if it has none.
    pub fn base(&mut self, page: u64) -> u32 {
        if let Some(&b) = self.pages.get(&page) {
            return b;
        }
        let base = self.next_slot as u32;
        self.next_slot += SLOTS_PER_PAGE;
        self.pages.insert(page, base);
        base
    }

    /// The base slot for a page, if it already has one.
    pub fn lookup(&self, page: u64) -> Option<u32> {
        self.pages.get(&page).copied()
    }

    /// Total slots allocated so far.
    pub fn slots(&self) -> u64 {
        self.next_slot
    }

    /// Rebuild an assignment from the entries a previous run recorded.
    ///
    /// Used when a cached module is loaded instead of translated: the pages it
    /// claimed have to keep the same bases, because the directory entries the
    /// host is about to publish name them.
    pub fn from_entries(entries: &[(u64, u32)], next_slot: u64) -> PageTable {
        PageTable {
            pages: entries.iter().copied().collect(),
            next_slot,
        }
    }
}

/// What the host needs from a translation besides the wasm itself.
///
/// Small enough to sit beside a cached module, which is the point: on a cache
/// hit the translation is skipped entirely, and this is everything that would
/// otherwise have to be recomputed to set up dispatch.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Metadata {
    pub text_base: u64,
    pub table_slots: u64,
    pub dir_entries: Vec<(u64, u32)>,
    /// The page table's high-water mark, so runtime translation carries on
    /// allocating where this left off.
    pub next_slot: u64,
    pub n_blocks: usize,
}

impl Metadata {
    /// A compact little-endian encoding. Hand-rolled because the alternative
    /// is a serialisation dependency for five fields.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(40 + self.dir_entries.len() * 12);
        v.extend_from_slice(&METADATA_MAGIC.to_le_bytes());
        v.extend_from_slice(&self.text_base.to_le_bytes());
        v.extend_from_slice(&self.table_slots.to_le_bytes());
        v.extend_from_slice(&self.next_slot.to_le_bytes());
        v.extend_from_slice(&(self.n_blocks as u64).to_le_bytes());
        v.extend_from_slice(&(self.dir_entries.len() as u64).to_le_bytes());
        for (page, base) in &self.dir_entries {
            v.extend_from_slice(&page.to_le_bytes());
            v.extend_from_slice(&base.to_le_bytes());
        }
        v
    }

    /// Decode, returning `None` for anything that is not exactly this format.
    ///
    /// A caller that gets `None` should treat the cache entry as absent; the
    /// sidecar is only ever written by gex, so the check is against corruption
    /// and version skew rather than a hostile input.
    pub fn decode(bytes: &[u8]) -> Option<Metadata> {
        let mut r = Reader { b: bytes, at: 0 };
        if r.u64()? != METADATA_MAGIC {
            return None;
        }
        let text_base = r.u64()?;
        let table_slots = r.u64()?;
        let next_slot = r.u64()?;
        let n_blocks = r.u64()? as usize;
        let count = r.u64()?;
        let mut dir_entries = Vec::with_capacity(count.min(1 << 20) as usize);
        for _ in 0..count {
            dir_entries.push((r.u64()?, r.u32()?));
        }
        // Trailing bytes mean this is not the file we think it is.
        if r.at != bytes.len() {
            return None;
        }
        Some(Metadata {
            text_base,
            table_slots,
            dir_entries,
            next_slot,
            n_blocks,
        })
    }
}

/// Bumped whenever the encoding changes, so an old sidecar is rejected.
const METADATA_MAGIC: u64 = 0x6765785f6d657431; // "gex_met1"

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.b.get(self.at..self.at + n)?;
        self.at += n;
        Some(s)
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
}

/// Builds a wasm module from GIR blocks.
pub struct ModuleBuilder {
    m: Module,
    text_base: u64,
    /// Page-to-slot assignment, shared with everything already translated.
    page_table: PageTable,
    /// Pages this module allocated, which the host must publish.
    new_pages: Vec<(u64, u32)>,
    block_ty: u32,
    miss_stub: u32,
    /// Guest block address -> wasm function index.
    blocks: HashMap<u64, u32>,
    /// Blocks awaiting code generation, paired with their function index.
    pending: Vec<(Block, u32)>,
    imports: Imports,
    data: Vec<DataSegment>,
    linkage: Linkage,
    /// Blocks to export by name, for a shared-linkage module.
    exports_by_addr: Vec<(u64, u32)>,
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
    fma: u32,
}

impl ModuleBuilder {
    /// Start an image module: it owns the memory and the dispatch table.
    pub fn new(text_base: u64, text_size: u64) -> ModuleBuilder {
        Self::with_linkage(text_base, text_size, Linkage::Own, true)
    }

    /// Start a module covering guest text `[text_base, text_base + text_size)`.
    ///
    /// `first_slot` is where this module's page allocations begin in the
    /// shared table; an image module starts just above the reserved miss
    /// span, and a runtime module starts wherever the host has grown the
    /// table to.
    pub fn with_linkage(
        text_base: u64,
        text_size: u64,
        linkage: Linkage,
        shared_memory: bool,
    ) -> ModuleBuilder {
        Self::with_pages(
            text_base,
            text_size,
            linkage,
            PageTable::new(),
            shared_memory,
        )
    }

    /// As [`Self::with_linkage`], continuing an existing page assignment.
    ///
    /// `shared_memory` decides whether the imported memory is declared shared.
    /// Threads need it and it costs nothing on the native host, but a
    /// single-threaded browser build wants it off: a Rust shim that imports a
    /// shared memory has to be built with `+atomics`, which needs
    /// `-Z build-std`.
    pub fn with_pages(
        text_base: u64,
        text_size: u64,
        linkage: Linkage,
        page_table: PageTable,
        shared_memory: bool,
    ) -> ModuleBuilder {
        assert_eq!(text_base % 4, 0, "text base must be instruction-aligned");
        let _ = text_size;

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
        let t_fma = m.func_type(FuncType::new(
            vec![
                ValType::I32,
                ValType::I64,
                ValType::I64,
                ValType::I64,
                ValType::I32,
            ],
            vec![ValType::I64],
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
            fma: m.import_func(im, imports::FMA, t_fma),
        };

        // The memory is the host's, and shared, so every guest thread acts on
        // the same bytes -- a defined memory would be per-instance, and each
        // thread needs its own store. The table is sized in `finish`, once the
        // set of pages the module touches is known.
        m.import_memory(
            IMPORT_MODULE,
            exports::MEMORY,
            GUEST_MEM_PAGES,
            shared_memory,
        );
        if linkage == Linkage::Shared {
            // The table comes from the image module, so this one's blocks
            // dispatch through exactly the same slots.
            m.import_table(IMPORT_MODULE, exports::DISPATCH_TABLE, 0);
        }

        // The miss stub and `enter` are the first two local functions, so
        // their indices are known before any block is declared.
        let miss_stub = m.declare_func(block_ty);
        let enter = m.declare_func(t_syscall);

        let mut b = ModuleBuilder {
            m,
            text_base,
            page_table,
            new_pages: Vec::new(),
            block_ty,
            miss_stub,
            blocks: HashMap::new(),
            pending: Vec::new(),
            imports,
            data: Vec::new(),
            linkage,
            exports_by_addr: Vec::new(),
        };

        let stub_body = b.build_miss_stub();
        b.m.define_func(miss_stub, stub_body);
        let enter_body = b.build_enter();
        b.m.define_func(enter, enter_body);

        if linkage == Linkage::Own {
            // Re-export the imported memory. The index space puts imports
            // first, so memory 0 is the host's; exporting it is what gives
            // each store an ordinary `Memory` handle to the shared bytes.
            b.m.export(exports::MEMORY, ExportKind::Memory, 0);
            b.m.export(exports::DISPATCH_TABLE, ExportKind::Table, 0);
        }
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

    /// Allocate the table slots for a page, if it has none yet.
    fn page_base(&mut self, addr: u64) -> u32 {
        let page = dispatch_page(addr);
        let fresh = self.page_table.lookup(page).is_none();
        let base = self.page_table.base(page);
        if fresh {
            self.new_pages.push((page, base));
        }
        base
    }

    /// Add a translated block, reserving its index if necessary.
    pub fn add_block(&mut self, block: Block) {
        let f = self.reserve(block.start);
        self.page_base(block.start);
        // A shared module cannot write the table itself — element segments
        // may not target an imported table at an offset the host chose — so
        // its blocks are exported and the host installs them.
        if self.linkage == Linkage::Shared {
            self.exports_by_addr.push((block.start, f));
        }
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
            let gir: HashMap<u64, &Block> =
                self.pending.iter().map(|(b, _)| (b.start, b)).collect();
            // A block reachable only from its one predecessor can be folded
            // into it without duplicating anything. `call_return` counts:
            // a callee's `ret` lands there, so it has a predecessor the
            // control-flow edges do not show.
            let mut preds: HashMap<u64, u32> = HashMap::new();
            for (b, _) in &self.pending {
                for s in b.successors().chain(b.call_return) {
                    *preds.entry(s).or_default() += 1;
                }
            }
            let cx = tier1::Context {
                syscall: self.imports.syscall,
                illegal: self.imports.illegal,
                breakpoint: self.imports.breakpoint,
                sysreg_read: self.imports.sysreg_read,
                sysreg_write: self.imports.sysreg_write,
                cache_op: self.imports.cache_op,
                dispatch_miss: self.imports.dispatch_miss,
                safepoint: self.imports.safepoint,
                fma: self.imports.fma,
                block_ty: self.block_ty,
                blocks: &self.blocks,
                gir: &gir,
                preds: &preds,
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

        let mut exported_blocks = Vec::new();
        let table_slots = self.page_table.slots();
        let dir_entries = std::mem::take(&mut self.new_pages);

        if self.linkage == Linkage::Own {
            // Publish the page directory as data, so a module is usable
            // without a host that knows to write it -- which is what the
            // cross-engine harness and the browser shim need. The native host
            // writes the same values from the metadata; a data segment simply
            // gets there first.
            //
            // Written as runs of consecutive pages rather than one span,
            // because the gaps may hold entries some other module claimed and
            // a zero would erase them.
            let mut sorted = dir_entries.clone();
            sorted.sort_unstable();
            let mut i = 0;
            while i < sorted.len() {
                let start_page = sorted[i].0;
                let mut bytes = Vec::new();
                while i < sorted.len() && sorted[i].0 == start_page + (bytes.len() / 4) as u64 {
                    bytes.extend_from_slice(&sorted[i].1.to_le_bytes());
                    i += 1;
                }
                self.data.push(DataSegment {
                    offset: DISPATCH_DIR + start_page * 4,
                    bytes,
                });
            }
        }

        match self.linkage {
            Linkage::Own => {
                self.m
                    .set_table(table_slots as u32, gex_abi::DISPATCH_TABLE_MAX_SLOTS as u32);
                // Every slot starts as the miss stub; translated blocks then
                // overwrite the ones they own. A branch anywhere else asks
                // the host to translate rather than trapping.
                let mut funcs = vec![self.miss_stub; table_slots as usize];
                for (&addr, &f) in &self.blocks {
                    let page = dispatch_page(addr);
                    if let Some(base) = self.page_table.lookup(page) {
                        let idx = base as u64 + ((addr % DISPATCH_PAGE) >> 2);
                        if (idx as usize) < funcs.len() {
                            funcs[idx as usize] = f;
                        }
                    }
                }
                self.m.element(ElementSegment {
                    table: 0,
                    offset: 0,
                    funcs,
                });
            }
            Linkage::Shared => {
                for (addr, f) in std::mem::take(&mut self.exports_by_addr) {
                    let name = format!("b{addr:x}");
                    self.m.export(&name, ExportKind::Func, f);
                    exported_blocks.push((addr, name));
                }
            }
        }

        for d in std::mem::take(&mut self.data) {
            self.m.data(d);
        }

        TranslatedModule {
            wasm: self.m.encode(),
            text_base: self.text_base,
            table_slots,
            dir_entries,
            page_table: self.page_table.clone(),
            n_blocks,
            exported_blocks,
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
        let page = f.add_local(ValType::I64);

        // Record the entry PC so a miss inside `enter` reports it correctly.
        f.local_get(0);
        f.local_get(1);
        f.store(0x37, 3, ctx::PC as u32);

        f.local_get(0); // ctx argument for the target block

        // The same branchless two-level lookup tier-1 emits: page directory,
        // then instruction slot within the page.
        f.local_get(1);
        f.i64_const(DISPATCH_PAGE.trailing_zeros() as i64);
        f.byte(0x88); // i64.shr_u
        f.local_set(page);

        f.local_get(page);
        f.byte(0xa7); // i32.wrap_i64
        f.i32_const(4);
        f.byte(0x6c); // i32.mul
        f.load(0x28, 2, DISPATCH_DIR as u32);
        f.i32_const(0);
        f.local_get(page);
        f.i64_const(DISPATCH_MAX_PAGES as i64);
        f.byte(0x54); // i64.lt_u
        f.select();

        f.local_get(1);
        f.i64_const((DISPATCH_PAGE - 1) as i64);
        f.byte(0x83); // i64.and
        f.i64_const(2);
        f.byte(0x88); // i64.shr_u
        f.byte(0xa7); // i32.wrap_i64
        f.byte(0x6a); // i32.add

        f.return_call_indirect(self.block_ty, 0);
        f.finish()
    }
}
