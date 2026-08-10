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
///
/// The allocation mark is shared between clones rather than copied with them.
/// A host translates from a *snapshot* of this table and publishes the result
/// much later -- milliseconds of lifting and seconds of Cranelift later -- and
/// with a copied mark two threads doing that at once both allocated from the
/// same number. Two different guest pages were handed the same base, and an
/// indirect branch to one of them landed inside a function belonging to the
/// other; dpkg's decompressor produced a byte-perfect 5 MB of output and then
/// failed a stack canary in an `exit` handler it had entered halfway through.
///
/// Sharing the mark makes the collision impossible without a lock, which is
/// the point: the alternative is holding one across the compile, and a lock
/// held for seconds around a step every thread needs is a thing to avoid
/// rather than to measure.
///
/// Two translations can still allocate *different* bases for the same page.
/// That is harmless -- both sets of slots hold correct code for those
/// addresses, so a branch is right whichever it reaches -- and it costs a page
/// of slots.
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
    /// space, and growing a table from it means growing every table below it
    /// to its full ten million. That is fine while everything allocates
    /// linearly from zero and ruinous once the space is partitioned -- which
    /// it is: closures own tables 0..3 with their numbers baked in at build
    /// time, and the run translates into tables 4..7. See `RUNTIME_SLOT_BASE`
    /// in the global kernel for why.
    highs: std::sync::Arc<Vec<std::sync::atomic::AtomicU64>>,
    refill: Option<std::sync::Arc<Refill>>,
}

/// A source of slot numbers no other page table will hand out.
///
/// Without one, a table allocates from its own counter and two processes
/// running the same program hand out *the same numbers for different code* --
/// both start from the mark their image was built with and translate in
/// whatever order they happen to reach. That is fine as long as nothing
/// crosses between them, and fatal the moment one adopts a module the other
/// built: the adopted code lands on slots this process has already filled,
/// and this process's own directory entries still point there.
///
/// The host installs one that draws from a system-wide counter, so every
/// range issued anywhere is issued once. Hosts with a single process do not
/// need it and do not pay for it.
pub struct Refill(pub Box<dyn Fn(u64) -> u64 + Send + Sync>);

impl std::fmt::Debug for Refill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Refill")
    }
}

/// Slots taken from the global counter at a time.
///
/// One round trip through the kernel per 256 pages of translated code, against
/// a table that reaches a few hundred pages for a small program -- so a
/// process pays for one or two.
const REFILL_SLOTS: u64 = SLOTS_PER_PAGE * 256;

/// Above this many blocks, a module's element segments are passive and it
/// carries the fill helpers; below it, they are active and it carries none.
///
/// The two kinds of module want opposite things. A closure library is
/// instantiated once per guest process and mostly never reached -- 303 MiB of
/// `libgcrypt` for the sliver of it apt calls -- so writing its slots on
/// demand is worth a great deal. A unit the runtime translator emits is about
/// twenty blocks, wanted immediately, and an `apt-get update` installs
/// thousands of them a process, so the helpers there are pure compile cost:
/// emitting them everywhere took that run from 82 seconds to 114.
///
/// On, because it is measured. Alternating, one shim, only the artifact
/// changing, on the 19-object `--exhaustive` closure of apt-get:
///
///     active    54,889ms / 57,125ms   install 29,102ms / 31,658ms
///     deferred  51,198ms / 52,081ms   install 17,100ms / 15,862ms
///
/// Instantiate falls 46 percent and blocks translated are unchanged, so
/// nothing is lifted that the closure already held.
///
/// Getting there took four defects, each of which made the mechanism look
/// like a bad idea: `gexfillat` matching a packed directory entry against a
/// bare slot offset so it never fired; the host rather than the module
/// deciding what defers, which left a forked child filling eagerly; a page
/// whose module had not arrived yet written off for ever; and the ask sitting
/// below `maybe_recompile_at`, so a deferred process recompiled whole
/// libraries it already held.
const BIG_BLOCKS: usize = 4096;

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
    /// Allocation is linear in one *virtual* slot space that spans every
    /// table; which table a page lands in is that number divided by the size
    /// of a table. Keeping one counter is what makes the cross-process
    /// identity map still work -- a page adopted from another process brings
    /// its whole entry, table index and all, and means the same thing.
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
    /// address -- the same library mapped from the same file at the same
    /// offset -- and so can share what was translated from them. Without it
    /// every `execve` re-translates libc into fresh slots, which is 4.2 times
    /// the slots and 4.2 times the work for the same code.
    ///
    /// The caller owns the claim that the bytes match. Assigning a base whose
    /// slots hold something else is the same defect as reusing a freed one.
    pub fn assign(&mut self, page: u64, entry: u32) {
        self.pages.insert(page, entry);
        // And make sure this table never hands the same slots out again, and
        // that the host grows its tables far enough to contain them.
        //
        // An adopted page was allocated by somebody else, so it can sit above
        // anything this table has issued -- and `slots()` is what the host
        // grows to. Without this the directory names slots the table has not
        // got, and the guest's next indirect branch into that page is a
        // `table index is out of bounds` a long way from the cause.
        let end = gex_abi::dispatch_entry_table(entry) as u64 * Self::SLOTS_PER_TABLE
            + gex_abi::dispatch_entry_base(entry) as u64
            + SLOTS_PER_PAGE;
        self.high.fetch_max(end, std::sync::atomic::Ordering::SeqCst);
        self.note_virtual(end - SLOTS_PER_PAGE);
        // Without a global counter the allocation mark has to move too, or
        // this table hands the adopted slots out again. With one, the adopted
        // range was issued to somebody else and this table's own chunk is
        // untouched by it -- moving the mark would only waste the chunk.
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
    /// Used when the guest unmaps executable memory. The old slots are not
    /// reused -- reusing them would leave the guest's directory pointing at a
    /// base whose contents are about to change under it, which is exactly the
    /// bug this exists to close. They are simply abandoned; the table grows,
    /// and a program has to unmap a great deal of code before that matters.
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

    /// Take in another table's assignments, its own winning where they
    /// disagree.
    ///
    /// Publishing is ordered, and every store replays the units in that same
    /// order, so the last assignment merged here is the one whose directory
    /// entry the guest ends up holding.
    pub fn merge(&mut self, other: &PageTable) {
        for (page, base) in &other.pages {
            self.pages.insert(*page, *base);
        }
    }

    /// Move the allocation mark forward, never back.
    ///
    /// Slots must never be handed out twice. A host that saves and restores a
    /// page table -- which is what `fork` in the browser does -- would
    /// otherwise reuse everything the other side of the checkpoint allocated,
    /// and a page whose directory entry survived the restore would dispatch
    /// into whatever code now occupies its slots.
    pub fn advance_to(&mut self, slot: u64) {
        self.high.fetch_max(slot, std::sync::atomic::Ordering::SeqCst);
        if self.refill.is_none() {
            self.next_slot
                .fetch_max(slot, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Rebuild an assignment from the entries a previous run recorded.
    ///
    /// Used when a cached module is loaded instead of translated: the pages it
    /// claimed have to keep the same bases, because the directory entries the
    /// host is about to publish name them.
    ///
    /// This starts a *new* mark. Everything cloned from the result shares it;
    /// nothing already in flight does.
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
        // The first parameter of every one of these is the register context.
        // It stays an i32 even on a 64-bit memory: the shim declares it
        // `ctx: i32` on both targets and casts to a pointer inside, so this
        // is a fixed ABI rather than something that widens with the address
        // space. Emitted code extends it once per block instead -- see
        // `ctx_addr` in tier-1.
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

        // The memory is the host's, and shared, so every guest thread acts on
        // the same bytes -- a defined memory would be per-instance, and each
        // thread needs its own store. The table is sized in `finish`, once the
        // set of pages the module touches is known.
        m.import_memory(
            IMPORT_MODULE,
            exports::MEMORY,
            GUEST_MEM_INITIAL_PAGES,
            GUEST_MEM_PAGES,
            shared_memory,
        );
        if linkage == Linkage::Shared {
            // The tables come from the image module, so this one's blocks
            // dispatch through exactly the same slots. All of them are
            // imported whether or not this module's pages reach past the
            // first: the block bodies select between them with a `br_table`
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
            // The single largest serial phase there was: one wasm function
            // body per guest block, hundreds of thousands of them for a
            // library the size of libz3, generated one at a time while
            // Cranelift waited behind it with the rest of the machine idle.
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
                // A whole-program module's pages all live in table 0 -- one
                // program's text is nowhere near the ceiling. The rest are
                // declared empty so that the `br_table` in every block body
                // has a table for each of its arms.
                for _ in 1..gex_abi::DISPATCH_TABLES {
                    self.m.set_table(0, gex_abi::DISPATCH_TABLE_MAX_SLOTS as u32);
                }
                // Every slot starts as the miss stub; translated blocks then
                // overwrite the ones they own. A branch anywhere else asks
                // the host to translate rather than trapping.
                let mut funcs = vec![self.miss_stub; table_slots as usize];
                for (&addr, &f) in &self.blocks {
                    let page = dispatch_page(addr);
                    if let Some(entry) = self.page_table.lookup(page) {
                        // Masked, not used raw: a directory entry carries the
                        // table index in its top bits now. A whole-program
                        // module's pages are all in table 0 -- one program's
                        // text is nowhere near ten million slots -- so the
                        // mask is what makes that assumption explicit rather
                        // than silently indexing by a table number.
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
                    funcs,
                    passive: false,
                    // The image module's one segment covers the whole table,
                    // not a page, and is never demand filled.
                    guest_page: 0,
                });
            }
            Linkage::Shared => {
                // The table is imported, and the module fills it itself.
                //
                // It used to export every block by name and let the host set
                // the slots one at a time. That is a name lookup and a table
                // write per block, from outside the engine, and on a whole
                // shared library it is the dominant cost of using the
                // translation at all: 200,000 blocks for libc took half a
                // minute to install into each store, dwarfing the compile.
                //
                // An active element segment may target an imported table --
                // the earlier comment here said otherwise, and was wrong --
                // and the slot base of every page is already fixed by the
                // time this runs, because `page_base` allocated it. So the
                // offsets are constants and the whole thing is one segment
                // per page, filled by the engine at instantiation.
                let mut by_page: BTreeMap<u64, Vec<(u64, u32)>> = BTreeMap::new();
                for (addr, f) in std::mem::take(&mut self.exports_by_addr) {
                    by_page.entry(dispatch_page(addr)).or_default().push((addr, f));
                    exported_blocks.push((addr, String::new()));
                }
                // One segment per page, not one per contiguous run.
                //
                // A run per gap is the smaller encoding and it does not
                // scale: Mesa's driver is 457,000 blocks scattered over its
                // pages, which came to more than a hundred thousand runs --
                // and wasmtime refuses a module with more than 100,000
                // element segments. `--recompile-objects` failed on exactly
                // the library that most wants static translation, fell back
                // to translating it a block at a time at run time, and the
                // failure was a line on stderr that nothing was watching.
                //
                // The gaps are filled with this module's own miss stub. An
                // earlier comment here said that was "wrong -- the stub
                // belongs to a module this one cannot name", and that is not
                // so: every module, shared or not, defines its own stub. Nor
                // does filling clobber anyone: a page covers one 4 KiB region
                // of guest text, so every slot in it belongs to this object,
                // and an untranslated one should ask the host to translate --
                // which is what the stub does.
                //
                // Only the interior is filled. Trailing slots past the last
                // translated block cost nothing to leave alone.
                for (page, mut blocks) in by_page {
                    let Some(entry) = self.page_table.lookup(page) else {
                        continue;
                    };
                    // Which table this page lives in. A page is allocated and
                    // written as a unit and never straddles two, so one
                    // segment still covers it.
                    let table = gex_abi::dispatch_entry_table(entry) as u32;
                    blocks.sort_by_key(|(a, _)| *a);
                    let slot = |addr: u64| gex_abi::dispatch_index(addr, entry);
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
                        funcs,
                        // Written by `gexfill`, not by instantiation.
                        //
                        // These are the modules instantiated once per guest
                        // process against an imported table, and an active
                        // segment makes every process write every block --
                        // for a closure library, every block of a whole
                        // object. `libgcrypt` is 303 MiB of wasm and cost
                        // between 1.2 and 2.2 seconds a process to
                        // instantiate, nearly all of it slots for code the
                        // process never reached.
                        //
                        // Only on a module big enough for that to be the
                        // shape of the cost. A runtime unit is a couple of
                        // thousand blocks, wanted immediately, and making
                        // its segments passive cost `apt-get update` 32
                        // seconds against 82 -- see the helpers below.
                        passive: n_blocks > BIG_BLOCKS,
                        // What `gexfillat` matches a request against. The
                        // whole entry, table index included: see the field.
                        guest_page: page as u32,
                    });
                }
            }
        }

        for d in std::mem::take(&mut self.data) {
            self.m.data(d);
        }

        // The dispatch metadata rides in a custom section, so a module is
        // self-describing: a page fetches one file and the host reads out of
        // it everything it needs to set dispatch up. Written before the
        // struct is built because it describes exactly these fields.
        let meta = Metadata {
            text_base: self.text_base,
            table_slots,
            dir_entries: dir_entries.clone(),
            next_slot: self.page_table.slots(),
            n_blocks,
        };
        // The fillers, now that every segment this module has is known --
        // and only on a module whose segments are passive, because they are
        // three more functions for the engine to compile in every process
        // that instantiates it. `gexfillat` on libc is 355 range compares.
        // An `apt-get update` installs thousands of runtime units, and
        // carrying the helpers on all of them was the whole of the 32-second
        // regression: not the passive segments, not the crossing per
        // segment, both of which were measured and cleared.
        if n_blocks > BIG_BLOCKS {
            let void_ty = self.m.func_type(FuncType::new(vec![ValType::I32], vec![ValType::I32]));
            self.m.add_fill(void_ty);
            self.m.add_fill_at(void_ty);
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

        // The slot half only. Without this the table index rides along in the
        // top eight bits and is *added to the slot*, which is how the demo
        // site died: an entry naming table 1 is `(1 << 24) | slot`, so a
        // branch to a page in table 1 asked table 0 for element 16,790,416 --
        // against a table 9,999,360 long -- and the engine said `table index
        // is out of bounds`.
        //
        // `enter` was written when there was one table, where the top bits
        // are always zero and this mask would be an instruction that could
        // never change anything. #64 added eight of them and taught
        // `emit_dispatch_tail_call_inner` to mask and select; this copy of
        // the same lookup was not touched, and stayed wrong for as long as no
        // run used more than one table's worth of slots. `apt-get update` in
        // the browser is the first thing that does: forty processes drawing
        // from one global slot counter exhaust table 0, and the next `enter`
        // -- which is every `fork` and every `execve` -- traps.
        if gex_abi::DISPATCH_TABLES > 1 {
            f.i32_const(gex_abi::DISPATCH_SLOT_MASK as i32);
            f.byte(0x71); // i32.and
        }

        f.local_get(1);
        f.i64_const((DISPATCH_PAGE - 1) as i64);
        f.byte(0x83); // i64.and
        f.i64_const(2);
        f.byte(0x88); // i64.shr_u
        f.byte(0xa7); // i32.wrap_i64
        f.byte(0x6a); // i32.add
        f.local_set(index);

        if gex_abi::DISPATCH_TABLES <= 1 {
            f.local_get(0); // ctx argument for the target block
            f.local_get(index);
            f.return_call_indirect(self.block_ty, 0);
            return f.finish();
        }

        // And the table the entry names, selected the way tier-1 selects it:
        // `n` blocks, table k breaking to depth `n-1-k`, the default being
        // table 0 so an entry naming a table that does not exist reaches the
        // miss stub rather than the engine.
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
