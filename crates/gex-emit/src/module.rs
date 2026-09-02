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
    GUEST_MEM_INITIAL_PAGES, GUEST_MEM_PAGES, IMPORT_MODULE, MISS_SLOTS, SLOTS_PER_PAGE,
};
use gex_ir::Block;
use std::collections::{BTreeMap, HashMap};

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
    /// Every page this module's blocks land on, claimed here or already
    /// known, as `(page, base slot)`.
    ///
    /// Distinct from `dir_entries`, which is only the pages this module
    /// *allocated* (what its data segment writes; a zero in a gap would erase
    /// another module's entry). Publishing a translation needs the pages it
    /// *covers*, including ones an earlier translation already numbered.
    pub covered: Vec<(u64, u32)>,
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
/// Process-wide, not per-module: a page's directory entry is a single value in
/// guest memory, so two modules picking their own base for the same page would
/// orphan each other's blocks.
///
/// The allocation mark is an `Arc` shared between clones, not copied with them:
/// a host translates from a snapshot and publishes seconds later, and with a
/// copied mark two concurrent translations would allocate the same base for
/// different pages. Sharing the mark makes that collision impossible without
/// holding a lock across the compile.
///
/// Two translations can still allocate *different* bases for the same page.
/// That is harmless -- both slot sets hold correct code -- and costs a page of
/// slots.
#[derive(Debug, Clone)]
pub struct PageTable {
    pages: std::collections::BTreeMap<u64, u32>,
    next_slot: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// End of the chunk `next_slot` is allocating from, when a refill is set.
    chunk_end: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Highest slot any page in this table names, allocated here or adopted.
    high: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The highest slot wanted in each dispatch table, separately.
    ///
    /// [`Self::slots`] is one running total across the whole virtual slot
    /// space; growing a table from it would grow every table below it to its
    /// full ten million, which is ruinous once the space is partitioned --
    /// closures own tables 0..3 and the run translates into 4..7.
    highs: std::sync::Arc<Vec<std::sync::atomic::AtomicU64>>,
    refill: Option<std::sync::Arc<Refill>>,
}

/// A source of slot numbers no other page table will hand out.
///
/// Without one, two processes running the same program allocate from their own
/// counters and hand out the same numbers for different code -- fatal the
/// moment one adopts a module the other built, since the adopted code lands on
/// slots this process has already filled. The host installs one drawing from a
/// system-wide counter; single-process hosts need it not and do not pay for it.
pub struct Refill(pub Box<dyn Fn(u64) -> u64 + Send + Sync>);

impl std::fmt::Debug for Refill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Refill")
    }
}

/// Slots taken from the global counter at a time: one kernel round trip per
/// eight pages of translated code.
///
/// It was 256 pages, and the round trip is not what the number should be
/// chosen for. The counter is system-wide and nothing gives a slot back, so
/// what a process does not use of its chunk is spent for the page's whole life
/// -- and a process here claims about 112 pages, so 256 threw away three
/// quarters of every chunk. A `$(…)` loop reached its 77th process and then
/// could not translate at all: past the last dispatch table an entry names
/// table `DISPATCH_TABLES` and the module built from it imports a table no
/// instance has, so the engine refuses it.
///
/// Eight pages leaves at most seven wasted per process, which took the same
/// loop from 76 processes to about 350 -- and made it faster, because a
/// process's dispatch tables are grown to reach its slots and a tighter range
/// is a smaller table. The round trip is an atomic in the global kernel; 32
/// times as many of them did not show up in the timings at all.
const REFILL_SLOTS: u64 = SLOTS_PER_PAGE * 8;

/// Above this many blocks, a module's element segments are passive and it
/// carries the fill helpers; below it, they are active and it carries none.
///
/// A large closure library is instantiated once per process and mostly never
/// reached, so demand-filling its slots is worth a great deal. A runtime unit
/// is a couple of thousand blocks wanted immediately, and an `apt-get update`
/// installs thousands per process, so the helpers there are pure compile cost.
use gex_abi::BIG_BLOCKS;

impl Default for PageTable {
    fn default() -> PageTable {
        PageTable::from_entries(&[], 0)
    }
}

impl PageTable {
    /// Start allocating above the reserved miss span.
    pub fn new() -> PageTable {
        PageTable::from_entries(&[], MISS_SLOTS)
    }

    /// Slots one table holds, rounded down so a page never straddles two.
    ///
    /// V8 refuses to grow a table past ten million however it was declared,
    /// and a page's 1024 slots are allocated and written as a unit, so the
    /// usable size is the largest whole number of pages under the cap.
    pub const SLOTS_PER_TABLE: u64 = (gex_abi::DISPATCH_TABLE_MAX_SLOTS / SLOTS_PER_PAGE)
        * SLOTS_PER_PAGE;

    /// The directory entry for a page, allocating slots if it has none.
    ///
    /// Allocation is linear in one *virtual* slot space spanning every table;
    /// the table a page lands in is that number divided by a table's size. One
    /// counter is what keeps the cross-process identity map working: an adopted
    /// page brings its whole entry, table index and all, and means the same.
    pub fn base(&mut self, page: u64) -> u32 {
        if let Some(&b) = self.pages.get(&page) {
            return b;
        }
        let v = self.take_slots();
        let entry = gex_abi::dispatch_entry(
            (v / Self::SLOTS_PER_TABLE) as usize,
            (v % Self::SLOTS_PER_TABLE) as u32,
        );
        self.pages.insert(page, entry);
        self.high.fetch_max(
            v + SLOTS_PER_PAGE,
            std::sync::atomic::Ordering::SeqCst,
        );
        self.note_virtual(v);
        entry
    }

    /// Record that virtual slot `v` and the page above it are wanted, so the
    /// table holding them knows how long it has to be.
    fn note_virtual(&self, v: u64) {
        let t = (v / Self::SLOTS_PER_TABLE) as usize;
        let within = v % Self::SLOTS_PER_TABLE + SLOTS_PER_PAGE;
        if let Some(h) = self.highs.get(t) {
            h.fetch_max(within, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Forget how long the tables have to be, keeping the assignment itself.
    ///
    /// For a process that numbers its own pages. Left alone, the mark from
    /// `from_entries` makes every process grow table 0 to the whole closure's
    /// slot count before the guest runs an instruction, for a table that then
    /// hands out a fraction of that. The page entries stay (see below); only
    /// the mark's contribution to the table lengths goes.
    pub fn forget_mark_high(&self) {
        for h in self.highs.iter() {
            h.store(0, std::sync::atomic::Ordering::SeqCst);
        }
        // The entries stay: a page published at the build's base really does
        // need its table to reach it. Only the mark goes.
        for e in self.pages.values() {
            let v = gex_abi::dispatch_entry_table(*e) as u64 * Self::SLOTS_PER_TABLE
                + gex_abi::dispatch_entry_base(*e) as u64;
            self.note_virtual(v);
        }
    }

    /// How long dispatch table `i` has to be for what has been assigned.
    pub fn table_need(&self, i: usize) -> u64 {
        self.highs
            .get(i)
            .map(|h| h.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(0)
    }

    /// One page's worth of slots, refilling from the global counter if this
    /// table has one and its chunk is spent.
    fn take_slots(&mut self) -> u64 {
        let Some(refill) = self.refill.clone() else {
            return self
                .next_slot
                .fetch_add(SLOTS_PER_PAGE, std::sync::atomic::Ordering::SeqCst);
        };
        loop {
            let v = self.next_slot.load(std::sync::atomic::Ordering::SeqCst);
            let end = self.chunk_end.load(std::sync::atomic::Ordering::SeqCst);
            // A page's slots are written as a unit and indexed off one base,
            // so a page must not straddle two tables either.
            let straddles = v / Self::SLOTS_PER_TABLE
                != (v + SLOTS_PER_PAGE - 1) / Self::SLOTS_PER_TABLE;
            if v + SLOTS_PER_PAGE <= end && !straddles {
                if self
                    .next_slot
                    .compare_exchange(
                        v,
                        v + SLOTS_PER_PAGE,
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                    )
                    .is_ok()
                {
                    return v;
                }
                continue;
            }
            let base = (refill.0)(REFILL_SLOTS);
            self.next_slot
                .store(base, std::sync::atomic::Ordering::SeqCst);
            self.chunk_end
                .store(base + REFILL_SLOTS, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Draw slot numbers from a counter shared with every other process.
    ///
    /// Install this before the table allocates anything it might publish. See
    /// [`Refill`] for what goes wrong without it.
    pub fn set_refill(&mut self, refill: std::sync::Arc<Refill>) {
        self.refill = Some(refill);
        // Spend whatever is left of the local chunk: the numbers below the
        // mark were issued by this process alone and are not safe to share.
        self.chunk_end
            .store(0, std::sync::atomic::Ordering::SeqCst);
    }

    /// Point a page at slots that already hold code for it.
    ///
    /// For a host that knows two processes have the same bytes at the same
    /// address and can share what was translated from them, sparing every
    /// `execve` a re-translation of libc into fresh slots.
    ///
    /// The caller owns the claim that the bytes match. Assigning a base whose
    /// slots hold something else is the same defect as reusing a freed one.
    pub fn assign(&mut self, page: u64, entry: u32) {
        self.pages.insert(page, entry);
        // Grow `high` (what the host grows its tables to) past the adopted
        // slots, or the directory names slots the table has not got and the
        // next indirect branch into the page traps out of bounds.
        let end = gex_abi::dispatch_entry_table(entry) as u64 * Self::SLOTS_PER_TABLE
            + gex_abi::dispatch_entry_base(entry) as u64
            + SLOTS_PER_PAGE;
        self.high.fetch_max(end, std::sync::atomic::Ordering::SeqCst);
        self.note_virtual(end - SLOTS_PER_PAGE);
        // Without a global counter the mark must move too, or this table hands
        // the adopted slots out again. With one, the range was issued
        // elsewhere and moving the mark would only waste this table's chunk.
        if self.refill.is_none() {
            self.next_slot
                .fetch_max(end, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// The base slot for a page, if it already has one.
    pub fn lookup(&self, page: u64) -> Option<u32> {
        self.pages.get(&page).copied()
    }

    /// Give up a page's slots, so the next translation of it gets fresh ones.
    ///
    /// Used when the guest unmaps executable memory. The old slots are
    /// abandoned, not reused: reusing them would point the directory at a base
    /// whose contents are about to change under it.
    pub fn forget(&mut self, page: u64) -> Option<u32> {
        self.pages.remove(&page)
    }

    /// How far the host has to grow its tables to contain what this table
    /// names -- its own allocations and anything it has adopted.
    pub fn slots(&self) -> u64 {
        self.high
            .load(std::sync::atomic::Ordering::SeqCst)
            .max(self.next_slot.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// Every page-to-base assignment made so far.
    pub fn entries(&self) -> Vec<(u64, u32)> {
        self.pages.iter().map(|(p, b)| (*p, *b)).collect()
    }

    /// Take in another table's assignments, `other`'s winning where they
    /// disagree -- publishing is ordered, so the last merged is the one the
    /// guest's directory ends up holding.
    pub fn merge(&mut self, other: &PageTable) {
        for (page, base) in &other.pages {
            self.pages.insert(*page, *base);
        }
    }

    /// Move the allocation mark forward, never back.
    ///
    /// Slots must never be handed out twice. A host that saves and restores a
    /// page table -- as `fork` in the browser does -- would otherwise reuse
    /// what the other side of the checkpoint allocated.
    pub fn advance_to(&mut self, slot: u64) {
        self.high.fetch_max(slot, std::sync::atomic::Ordering::SeqCst);
        if self.refill.is_none() {
            self.next_slot
                .fetch_max(slot, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Rebuild an assignment from the entries a previous run recorded.
    ///
    /// Used when a cached module is loaded instead of translated: its pages
    /// must keep the same bases, since the directory entries about to be
    /// published name them. Starts a *new* mark, shared only by later clones.
    pub fn from_entries(entries: &[(u64, u32)], next_slot: u64) -> PageTable {
        let t = PageTable {
            pages: entries.iter().copied().collect(),
            next_slot: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(next_slot)),
            chunk_end: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX)),
            high: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(next_slot)),
            highs: std::sync::Arc::new(
                (0..gex_abi::DISPATCH_TABLES)
                    .map(|_| std::sync::atomic::AtomicU64::new(0))
                    .collect(),
            ),
            refill: None,
        };
        // The mark says where allocation carries on, so the table holding it
        // has to be long enough for the page it will hand out next.
        t.note_virtual(next_slot.saturating_sub(SLOTS_PER_PAGE.min(next_slot)));
        for (_, e) in entries {
            let v = gex_abi::dispatch_entry_table(*e) as u64 * Self::SLOTS_PER_TABLE
                + gex_abi::dispatch_entry_base(*e) as u64;
            t.note_virtual(v);
        }
        t
    }
}

/// What the host needs from a translation besides the wasm itself: everything
/// that would otherwise have to be recomputed to set up dispatch on a cache
/// hit, small enough to sit beside the cached module.
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
    /// A `None` means treat the cache entry as absent; the check is against
    /// corruption and version skew, not hostile input.
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

/// The custom section a module carries its [`Metadata`] in.
pub const METADATA_SECTION: &str = "gex.meta";

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
    /// Every page a block landed on, whether or not this module claimed it.
    covered: Vec<(u64, u32)>,
    block_ty: u32,
    miss_stub: u32,
    /// Guest block address -> wasm function index.
    blocks: HashMap<u64, u32>,
    /// Blocks awaiting code generation, paired with their function index.
    pending: Vec<(Block, u32)>,
    imports: Imports,
    data: Vec<DataSegment>,
    linkage: Linkage,
    /// Emit passive segments and the fill helpers whatever the block count.
    /// See `Options::demand_fill`.
    demand_fill: bool,

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
    /// Emit passive segments and the fill helpers whatever the block count.
    /// Set for a closure object, whose slots a process must be able to place
    /// for itself. See `Options::demand_fill`.
    pub fn set_demand_fill(&mut self, on: bool) {
        self.demand_fill = on;
    }

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
        assert_eq!(
            text_base % gex_abi::INST_ALIGN,
            0,
            "text base must be instruction-aligned"
        );
        let _ = text_size;

        let mut m = Module::new();
        // The first parameter of every one of these is the register context.
        // A fixed-ABI i32 even on a 64-bit memory: the shim declares it
        // `ctx: i32` on both targets and casts to a pointer inside.
        let p = ValType::I32;
        let block_ty = m.func_type(FuncType::new(vec![p], vec![]));
        let t_syscall = m.func_type(FuncType::new(vec![p, ValType::I64], vec![]));
        let t_miss = m.func_type(FuncType::new(
            vec![p, ValType::I64],
            vec![ValType::I32],
        ));
        let t_pc_i32 = m.func_type(FuncType::new(
            vec![p, ValType::I64, ValType::I32],
            vec![],
        ));
        let t_sysreg_read = m.func_type(FuncType::new(
            vec![p, ValType::I64, ValType::I32],
            vec![ValType::I64],
        ));
        let t_pc_i32_i64 = m.func_type(FuncType::new(
            vec![p, ValType::I64, ValType::I32, ValType::I64],
            vec![],
        ));
        let t_fma = m.func_type(FuncType::new(
            vec![p, ValType::I64, ValType::I64, ValType::I64, ValType::I32],
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

        // The memory is the host's and shared, so every guest thread acts on
        // the same bytes; a defined memory would be per-instance. The table is
        // sized in `finish`, once the pages the module touches are known.
        m.import_memory(
            IMPORT_MODULE,
            exports::MEMORY,
            GUEST_MEM_INITIAL_PAGES,
            GUEST_MEM_PAGES,
            shared_memory,
        );
        if linkage == Linkage::Shared {
            // The tables come from the image module, so this one's blocks
            // dispatch through the same slots. All are imported even if this
            // module's pages reach only the first: block bodies `br_table`
            // over every table index, so every index has to exist.
            for i in 0..gex_abi::DISPATCH_TABLES {
                m.import_table(IMPORT_MODULE, exports::dispatch_table(i), 0);
            }
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
            covered: Vec::new(),
            block_ty,
            miss_stub,
            blocks: HashMap::new(),
            pending: Vec::new(),
            imports,
            data: Vec::new(),
            linkage,
            exports_by_addr: Vec::new(),
            demand_fill: false,
        };

        let stub_body = b.build_miss_stub();
        b.m.define_func(miss_stub, stub_body);
        let enter_body = b.build_enter();
        b.m.define_func(enter, enter_body);

        if linkage == Linkage::Own {
            // Re-export the imported memory (index 0, imports coming first),
            // giving each store an ordinary `Memory` handle to the shared bytes.
            b.m.export(exports::MEMORY, ExportKind::Memory, 0);
            for i in 0..gex_abi::DISPATCH_TABLES {
                b.m.export(exports::dispatch_table(i), ExportKind::Table, i as u32);
            }
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
        // Always, whether or not this module claimed the slots. See the note
        // on `TranslatedModule::covered`.
        if self.covered.last().map(|&(p, _)| p) != Some(page) {
            self.covered.push((page, base));
        }
        base
    }

    /// Add a translated block, reserving its index if necessary.
    pub fn add_block(&mut self, block: Block) {
        let f = self.reserve(block.start);
        self.page_base(block.start);
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
        let timing = std::env::var_os("GEX_TIMING").is_some();
        let started = crate::par::Stopwatch::start();
        let compiled: Vec<(u32, (Vec<u8>, Vec<u32>))> = {
            let gir: HashMap<u64, &Block> =
                self.pending.iter().map(|(b, _)| (b.start, b)).collect();
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
            };
            // The single largest serial phase there was: one wasm body per
            // guest block, hundreds of thousands for a large library.
            // `compile` reads nothing it does not own, so the map spreads.
            crate::par::map(&self.pending, &|(block, f): &(Block, u32)| {
                (*f, tier1::compile_with_relocs(block, &cx))
            })
        };
        let n_blocks = compiled.len();
        if timing {
            eprintln!("[timing]   tier-1 codegen: {:.2}s", started.secs());
        }
        let started = crate::par::Stopwatch::start();
        for (f, (body, relocs)) in compiled {
            self.m.define_func_with_relocs(f, body, relocs);
        }
        if timing {
            eprintln!("[timing]   defining bodies: {:.2}s", started.secs());
        }

        let mut exported_blocks = Vec::new();
        let table_slots = self.page_table.slots();
        let dir_entries = std::mem::take(&mut self.new_pages);
        let mut covered = std::mem::take(&mut self.covered);
        covered.sort_unstable();
        covered.dedup();

        if self.linkage == Linkage::Own {
            // Publish the page directory as data, so a module is usable
            // without a host that knows to write it (the cross-engine harness
            // and the browser shim). Written as runs of consecutive pages, not
            // one span, since a zero in a gap would erase another module's
            // entry there.
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
                // A whole-program module's pages all live in table 0. The rest
                // are declared empty so the `br_table` in every block body has
                // a table for each of its arms.
                for _ in 1..gex_abi::DISPATCH_TABLES {
                    self.m.set_table(0, gex_abi::DISPATCH_TABLE_MAX_SLOTS as u32);
                }
                // Every slot starts as the miss stub; translated blocks
                // overwrite the ones they own, so a branch elsewhere asks the
                // host to translate rather than trapping.
                let mut funcs = vec![self.miss_stub; table_slots as usize];
                for (&addr, &f) in &self.blocks {
                    let page = dispatch_page(addr);
                    if let Some(entry) = self.page_table.lookup(page) {
                        // Masked, not raw: an entry carries a table index in
                        // its top bits, and a whole-program module's pages are
                        // all table 0. Makes that assumption explicit.
                        debug_assert_eq!(gex_abi::dispatch_entry_table(entry), 0);
                        let idx = gex_abi::dispatch_index(addr, entry);
                        if (idx as usize) < funcs.len() {
                            funcs[idx as usize] = f;
                        }
                    }
                }
                self.m.element(ElementSegment {
                    table: 0,
                    offset: 0,
                    page_base: 0,
                    funcs,
                    passive: false,
                    // The image module's one segment covers the whole table,
                    // not a page, and is never demand filled.
                    guest_page: 0,
                });
            }
            Linkage::Shared => {
                // The table is imported, and the module fills it itself with
                // element segments rather than exporting every block for the
                // host to install one at a time (a name lookup and a table
                // write per block, which dwarfed the compile on a whole libc).
                //
                // An active element segment may target an imported table, and
                // `page_base` has already fixed each page's slot base, so the
                // offsets are constants: one segment per page, filled by the
                // engine at instantiation.
                let mut by_page: BTreeMap<u64, Vec<(u64, u32)>> = BTreeMap::new();
                for (addr, f) in std::mem::take(&mut self.exports_by_addr) {
                    by_page.entry(dispatch_page(addr)).or_default().push((addr, f));
                    exported_blocks.push((addr, String::new()));
                }
                // One segment per page, not one per contiguous run: a run per
                // gap does not scale (Mesa's driver came to over 100,000 runs,
                // and engines cap element-segment count near there).
                //
                // Gaps are filled with this module's own miss stub. A page
                // covers one 4 KiB region of guest text, so every slot belongs
                // to this object, and an untranslated one should ask the host
                // to translate -- which the stub does. Only the interior is
                // filled; trailing slots past the last block are left alone.
                // Passive (written by `gexfill`/`gexfillat`, not instantiation)
                // for a closure module (`demand_fill`), whose pages are filled a
                // span at a time as the guest branches in; active for a small
                // runtime unit, whose slots are written when it is instantiated.
                let passive = self.demand_fill;
                let _ = BIG_BLOCKS;
                for (page, mut blocks) in by_page {
                    let Some(entry) = self.page_table.lookup(page) else {
                        continue;
                    };
                    // Which table this page lives in. A page never straddles
                    // two, so one segment still covers it.
                    let table = gex_abi::dispatch_entry_table(entry) as u32;
                    blocks.sort_by_key(|(a, _)| *a);
                    let slot = |addr: u64| gex_abi::dispatch_index(addr, entry);
                    let page_base = (entry & gex_abi::DISPATCH_SLOT_MASK) as u32;
                    if passive {
                        // One `[first,last]` segment for a closure page, this
                        // module's miss stub in the gaps. Per-contiguous-run
                        // gap-free segments would be correct on a shared page too,
                        // but a densely fragmented C++ library (`libapt-pkg`)
                        // fragments into tens of thousands of runs, and `gexfill`'s
                        // `br_table` over the segments then exceeds the wasm max
                        // branch count. See `add_fill`.
                        let (Some(first), Some(last)) = (blocks.first(), blocks.last()) else {
                            continue;
                        };
                        let start = slot(first.0);
                        let end = slot(last.0);
                        let mut funcs = vec![self.miss_stub; (end - start + 1) as usize];
                        for (addr, f) in &blocks {
                            funcs[(slot(*addr) - start) as usize] = *f;
                        }
                        self.m.element(ElementSegment {
                            table,
                            offset: start as u32,
                            page_base,
                            funcs,
                            passive,
                            guest_page: page as u32,
                        });
                    } else {
                        // A small runtime unit shares its page with a closure whose
                        // blocks sit in this unit's gaps, so one segment per
                        // contiguous run and nothing written between them -- a
                        // `[first,last]` unit would stub over the closure's blocks.
                        let mut i = 0;
                        while i < blocks.len() {
                            let run_start = slot(blocks[i].0) as u32;
                            let mut funcs = vec![blocks[i].1];
                            let mut j = i + 1;
                            while j < blocks.len()
                                && slot(blocks[j].0) as u32 == run_start + funcs.len() as u32
                            {
                                funcs.push(blocks[j].1);
                                j += 1;
                            }
                            self.m.element(ElementSegment {
                                table,
                                offset: run_start,
                                page_base,
                                funcs,
                                passive,
                                guest_page: page as u32,
                            });
                            i = j;
                        }
                    }
                }
            }
        }

        for d in std::mem::take(&mut self.data) {
            self.m.data(d);
        }

        // The dispatch metadata rides in a custom section, so a module is
        // self-describing: one file is enough for the host to set dispatch up.
        let meta = Metadata {
            text_base: self.text_base,
            table_slots,
            dir_entries: dir_entries.clone(),
            next_slot: self.page_table.slots(),
            n_blocks,
        };
        // The fillers, now that every segment is known -- only on a module
        // with passive segments, since they are three more functions for the
        // engine to compile in every process that instantiates it.
        if self.demand_fill || n_blocks > BIG_BLOCKS {
            let void_ty = self.m.func_type(FuncType::new(vec![ValType::I32], vec![ValType::I32]));
            self.m.add_fill(void_ty);
            // `(page, base, from) -> filled`: `base` is where the asking
            // process wants this page's slots (not where the build put them),
            // `from` how far into the page to start, so a fill can cover the
            // span around the miss rather than all 1,024 slots.
            let at_ty = self.m.func_type(FuncType::new(
                vec![ValType::I32, ValType::I32, ValType::I32],
                vec![ValType::I32],
            ));
            self.m.add_fill_at(at_ty);
            let no_args = self.m.func_type(FuncType::new(vec![], vec![]));
            self.m.add_fill_all(no_args);
        }
        let started = crate::par::Stopwatch::start();
        let wasm = self
            .m
            .encode_with_custom(&[(METADATA_SECTION, meta.encode())]);
        if timing {
            eprintln!("[timing]   encoding: {:.2}s", started.secs());
        }
        TranslatedModule {
            wasm,
            text_base: self.text_base,
            table_slots,
            dir_entries,
            covered,
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
        f.local_get(0); // and again, for dispatch_miss itself
        f.local_get(0); // and as a memory address, to read the PC out
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

        let entry = f.add_local(ValType::I32);
        let index = f.add_local(ValType::I32);

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
        f.local_tee(entry);

        // The slot half only. Without this the table index in the top bits is
        // *added to the slot*: an entry naming table 1 is `(1 << 24) | slot`,
        // so a branch to a table-1 page over-indexes table 0 and traps. Only
        // bites once a run spans more than one table's worth of slots.
        if gex_abi::DISPATCH_TABLES > 1 {
            f.i32_const(gex_abi::DISPATCH_SLOT_MASK as i32);
            f.byte(0x71); // i32.and
        }

        f.local_get(1);
        f.i64_const((DISPATCH_PAGE - 1) as i64);
        f.byte(0x83); // i64.and
        if gex_abi::INST_ALIGN_SHIFT != 0 {
            f.i64_const(gex_abi::INST_ALIGN_SHIFT as i64);
            f.byte(0x88); // i64.shr_u
        }
        f.byte(0xa7); // i32.wrap_i64
        f.byte(0x6a); // i32.add
        f.local_set(index);

        if gex_abi::DISPATCH_TABLES <= 1 {
            f.local_get(0); // ctx argument for the target block
            f.local_get(index);
            f.return_call_indirect(self.block_ty, 0);
            return f.finish();
        }

        // And the table the entry names, selected as tier-1 selects it: table
        // k breaks to depth `n-1-k`, the default being table 0 so an entry
        // naming a nonexistent table reaches the miss stub, not the engine.
        let n = gex_abi::DISPATCH_TABLES as u32;
        f.local_get(entry);
        f.i32_const(gex_abi::DISPATCH_SLOT_BITS as i32);
        f.byte(0x76); // i32.shr_u
        f.local_set(entry);
        for _ in 0..n {
            f.block_void();
        }
        f.local_get(entry);
        let targets: Vec<u32> = (0..n).map(|k| n - 1 - k).collect();
        f.br_table(&targets, n - 1);
        for k in (0..n).rev() {
            f.end();
            f.local_get(0); // ctx argument for the target block
            f.local_get(index);
            f.return_call_indirect(self.block_ty, k);
        }
        f.finish()
    }
}
