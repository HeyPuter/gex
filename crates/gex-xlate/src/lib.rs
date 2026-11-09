//! `gex-xlate` — the translator facade.
//!
//! Ties the decoder, lifter and backend together and drives *code discovery*.
//!
//! Discovery is a best-effort search, never a correctness requirement:
//! everything it misses ends up in an untranslated dispatch slot
//! and is reported precisely at run time instead of silently misexecuting.
//! The more it finds ahead of time, the less work is left for the runtime
//! translator.
//!
//! Four sources feed the work list:
//!
//! 1. the ELF entry point;
//! 2. `STT_FUNC` symbols that land in an executable segment;
//! 3. direct branch and call targets, followed recursively;
//! 4. every naturally-aligned doubleword anywhere in the image whose value
//!    points into executable memory — which is how function-pointer tables,
//!    `.init_array`, the GOT and C++ vtables get covered.

#![forbid(unsafe_code)]

pub use gex_emit::merge;
pub use gex_emit::module::Linkage;
pub use gex_emit::module::Refill;
pub use gex_emit::module::Identity;
pub use gex_emit::module::Metadata;
/// Turn the per-block stack-pointer check on, for a host with no environment
/// to read `GEX_CHECK_SP` from. See `gex_emit::tier1::set_check_sp`.
pub use gex_emit::tier1::set_check_sp;
pub use gex_emit::tier1::set_copy_loop;
pub use gex_emit::tier1::set_coherent_loads;
pub use gex_emit::tier1::{add_trap_pc, set_guard_limit, set_trap_pc};
pub use gex_emit::PageTable;
pub use gex_emit::TranslatedModule;
use gex_emit::ModuleBuilder;
use gex_ir::{pass, Block, Term};
use gex_ld::Image;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

pub mod arch;
pub use arch::{lift_block, BlockLimits};
use gex_abi::INST_ALIGN;

pub mod intrinsic;
pub mod profile;
pub use intrinsic::Intrinsic;

pub mod ehframe;
#[cfg(feature = "fjit")]
pub mod fjit;
pub mod func;
pub mod jumptable;
pub mod summary;
pub mod validate;

/// The import address table slots of `pe` (imports and delay imports), as
/// sorted, merged RVA ranges: the bytes the loader overwrites with the
/// imports' addresses, which [`func::pe_input`] zeroes. (Here rather than in
/// `func.rs`: this file is not a function-tier key input, so a host-side
/// check never re-keys the unit caches.)
pub fn pe_import_slots(pe: &gex_ld::pe::PeImage) -> Vec<(u64, u64)> {
    let ptr = if pe.is64 { 8 } else { 4 };
    let mut r: Vec<u64> = pe.imports.iter().flat_map(|d| d.symbols.iter().map(|s| s.iat_rva as u64)).collect();
    r.extend(pe.delay_imports.iter().flat_map(|d| d.symbols.iter().map(|s| s.iat_rva as u64)));
    r.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(r.len());
    for lo in r {
        let hi = lo + ptr;
        match out.last_mut() {
            Some(l) if lo <= l.1 => l.1 = l.1.max(hi),
            _ => out.push((lo, hi)),
        }
    }
    out
}

/// The hash the function tier keeps of a dispatch page (`page_size` bytes
/// at `page * page_size`), with the bytes of `slots` (absolute, sorted
/// ranges: [`pe_import_slots`] placed at an image's base) counted as zero.
/// A loader that fills an import address table inside `.text` after a unit
/// was placed has not rewritten code: without the mask, the page read as
/// patched, and every function on it (MSVC's `__SEH_prolog4` next to the
/// table) left for tier-1 through a call-out that never ended.
pub fn masked_page_hash(memory: &[u8], page: u64, page_size: u64, slots: &[(u64, u64)]) -> u64 {
    let lo = page * page_size;
    let hi = lo + page_size;
    let range = memory.get(lo as usize..(hi as usize).min(memory.len())).unwrap_or(&[]);
    masked_hash(range, page, page_size, slots)
}

/// [`masked_page_hash`] of the page's bytes `range` (what of it the memory
/// has), read by the caller.
pub fn masked_hash(range: &[u8], page: u64, page_size: u64, slots: &[(u64, u64)]) -> u64 {
    let lo = page * page_size;
    let hi = lo + page_size;
    let first = slots.partition_point(|r| r.1 <= lo);
    let hits = &slots[first..slots[first..].partition_point(|r| r.0 < hi) + first];
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut step = |c: &[u8]| {
        let mut b = [0u8; 8];
        b[..c.len()].copy_from_slice(c);
        h = (h ^ u64::from_le_bytes(b)).wrapping_mul(0x0100_0000_01b3);
    };
    if hits.is_empty() {
        range.chunks(8).for_each(&mut step);
    } else {
        let mut copy = range.to_vec();
        for &(a, b) in hits {
            let (a, b) = (a.max(lo) - lo, b.min(hi) - lo);
            if let Some(x) = copy.get_mut(a as usize..(b as usize).min(range.len())) {
                x.fill(0);
            }
        }
        copy.chunks(8).for_each(&mut step);
    }
    h
}

/// The first byte where the mapped code `live` of the segment at `vaddr`
/// differs from the image's `bytes` (as [`func::pe_input`] made them), or
/// `None`. Bytes in an import slot of `pe` mapped at `base` do not count:
/// Microsoft's linker puts the import address table at the start of `.text`,
/// the loader has written the imports' addresses there, and `pe_input`
/// zeroed them -- a unit reads a slot at run time, it never embeds one.
pub fn pe_code_difference(vaddr: u64, bytes: &[u8], live: &[u8], base: u64, pe: Option<&gex_ld::pe::PeImage>) -> Option<usize> {
    if live.len() != bytes.len() {
        return Some(live.len().min(bytes.len()));
    }
    if live == bytes {
        return None;
    }
    let slots = pe.map(pe_import_slots).unwrap_or_default();
    let in_slot = |a: u64| {
        let i = slots.partition_point(|r| base + r.1 <= a);
        slots.get(i).is_some_and(|r| base + r.0 <= a)
    };
    (0..bytes.len()).find(|&i| live[i] != bytes[i] && !in_slot(vaddr + i as u64))
}

/// Knobs for a translation run.
#[derive(Clone, Debug)]
pub struct Options {
    pub limits: BlockLimits,
    /// Guest addresses that are library functions with a wasm instruction
    /// behind them, so the translator emits that instead of lifting the
    /// function's own code. See [`intrinsic`].
    pub intrinsics: HashMap<u64, Intrinsic>,
    /// The file the program was loaded from, for the function JIT's view of
    /// its image (`.eh_frame` function starts): set per process.
    pub image_elf: Option<std::sync::Arc<[u8]>>,
    /// Extra guest addresses to treat as block entry points.
    pub extra_entries: Vec<u64>,
    /// Scan data for values that look like code pointers.
    pub scan_pointers: bool,
    /// Treat *every* 4-byte-aligned executable address as an entry point.
    ///
    /// Recursive descent plus a pointer scan still misses jump tables, which
    /// store scaled *relative* offsets that look nothing like addresses.
    /// Translating everything up front covers that without a runtime
    /// translator, at the cost of translation time and module size — useful
    /// for a deployment that must not compile code later, and as a way to
    /// isolate a suspected runtime-translation bug.
    pub exhaustive: bool,
    /// Seed discovery from the entry point alone, ignoring the symbol table,
    /// to keep the up-front module small and let runtime translation pull in
    /// the rest rather than compiling functions the run may never enter.
    pub lazy: bool,
    /// Once a page holds any translated block, translate every decodable
    /// instruction in it, so no page comes out *partly* covered.
    ///
    /// A partly-covered page is the closure's one real hazard: a run reaching
    /// an untranslated block in a page the closure has already based
    /// demand-fills that block into the closure's own page, and the mix of
    /// build-time and run-time blocks under one base corrupts dispatch --
    /// libgcrypt's Ed25519 field arithmetic, reached only through a function
    /// pointer installed at run time that no build-time scan can follow, died
    /// exactly this way. Completing a touched page leaves only *wholly*
    /// untouched pages to the demand path, which is the case that already
    /// works (a C++ library reached entirely through vtables lifts cleanly).
    /// Bounded, unlike `exhaustive`: only pages a real block already reached,
    /// and transitively what those pages' blocks reach.
    pub complete_pages: bool,
    /// Translate only the seed blocks (`extra_entries` and the image entry),
    /// without following their branch successors onto fresh pages. For a
    /// profile-guided build: the seeds ARE the reached set, so recursive descent
    /// off them only adds never-taken branches -- one run measured 29 reached
    /// blocks of libzstd exploding to 14,588 followed ones. Paired with
    /// `complete_pages`, every reached block's page is still made whole (each
    /// reached block is a seed, so its page is touched and completed), so the
    /// result is exactly the reached pages: no partial page, no static
    /// over-coverage. A block a divergent run reaches is either on a completed
    /// page (present) or a wholly untouched one (a clean demand-fill).
    pub seeds_only: bool,
    /// Declare the imported memory shared. Off only for a single-threaded
    /// build whose host cannot import a shared memory.
    pub shared_memory: bool,
    /// Emit passive element segments and the fill helpers whatever the block
    /// count, so a process can put this module's pages wherever it likes.
    /// Needed for closure objects; overrides the `BIG_BLOCKS` size rule, which
    /// was measured on runtime units rather than a whole-closure build.
    pub demand_fill: bool,
    /// Whether this module declares the memory and dispatch table or imports
    /// them: one module in a program owns them and the rest join it.
    pub linkage: Linkage,
    /// Where this module's page allocations begin in the shared dispatch
    /// table. Zero starts where a lone module would; a closure's objects each
    /// continue where the last stopped, so their blocks do not claim the same
    /// slots and overwrite one another.
    pub first_slot: u64,
    /// Build the function tier: every block also as C, compiled by clang and
    /// spliced into the tier-1 module (`gex-cgen`).
    /// Needs the `cgen` feature; runtime translation stays tier-1.
    pub cgen: bool,
    /// The function JIT (`gex-fjit`): every
    /// recovered function emitted as wasm directly, no clang, and spliced
    /// into the tier-1 module as `cgen` is. Needs the `fjit` feature.
    pub fjit: bool,
    /// Translate nothing ahead of time: the module is `enter` and the miss
    /// stubs, and every address reaches the runtime (`gexc run --fjit`,
    /// where the function table's translator serves misses).
    pub no_blocks: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            limits: BlockLimits::default(),
            intrinsics: HashMap::new(),
            image_elf: None,
            extra_entries: Vec::new(),
            scan_pointers: true,
            // Off by default now that runtime translation covers the gaps;
            // still available for a build that must not translate later.
            exhaustive: false,
            lazy: false,
            complete_pages: false,
            seeds_only: false,
            shared_memory: true,
            demand_fill: false,
            linkage: Linkage::Own,
            first_slot: 0,
            cgen: false,
            fjit: false,
            no_blocks: false,
        }
    }
}

/// What a translation run found.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub blocks: usize,
    pub instructions: usize,
    /// Blocks whose last instruction gex cannot execute.
    pub illegal_blocks: usize,
    /// Entry points that came from the pointer scan rather than control flow.
    pub discovered_from_data: usize,
    /// Entry points reachable only because `exhaustive` was set.
    pub discovered_exhaustively: usize,
    /// Statements deleted by cross-block flag liveness.
    pub dead_flag_stmts: usize,
    pub wasm_bytes: usize,
}

/// A read-only view of the placed image, used to fetch instruction words.
pub struct CodeView<'a> {
    image: &'a Image,
}

impl<'a> CodeView<'a> {
    pub fn new(image: &'a Image) -> CodeView<'a> {
        CodeView { image }
    }

    /// Copy the instruction bytes at `pc` into `buf`, if it lies in
    /// executable memory, returning how many were available (up to 16).
    pub fn fetch(&self, pc: u64, buf: &mut arch::FetchBuf) -> usize {
        if !pc.is_multiple_of(INST_ALIGN) {
            return 0;
        }
        for seg in &self.image.segments {
            if !seg.executable {
                continue;
            }
            // Past `bytes` the mapping is zeros: a PE section's trailing
            // zeros are not stored, and an instruction can end in one
            // (`ret $0xc`, or a rebased `jmp *slot` whose slot address has
            // a zero high byte, last in `.text`).
            let span = seg.mem_size.max(seg.bytes.len() as u64);
            if pc >= seg.vaddr && pc < seg.vaddr + span {
                let off = (pc - seg.vaddr) as usize;
                let n = (span as usize - off).min(16);
                let have = seg.bytes.len().saturating_sub(off).min(n);
                if have > 0 {
                    buf[..have].copy_from_slice(&seg.bytes[off..off + have]);
                }
                buf[have..n].fill(0);
                return n;
            }
        }
        0
    }

    /// Fetch gated on the *section* flags, so a block never runs off the end
    /// of a code section into the tables mapped beside it.
    pub fn fetch_code(&self, pc: u64, buf: &mut arch::FetchBuf) -> usize {
        if !self.is_executable(pc) {
            return 0;
        }
        let n = self.fetch(pc, buf);
        // Trim to the executable span: the bytes past it are not code.
        let mut k = INST_ALIGN as usize;
        while k < n && self.is_executable(pc + k as u64) {
            k += 1;
        }
        n.min(k)
    }


    pub fn is_executable(&self, addr: u64) -> bool {
        // Section flags, not `PF_X`: a segment's executable bit also covers
        // the string tables and relocations mapped alongside the code, which
        // discovery would otherwise disassemble into SIGILL blocks.
        if !self.image.code.is_empty() {
            return self
                .image
                .code
                .iter()
                .any(|&(lo, hi)| addr >= lo && addr < hi);
        }
        // Stripped of its section headers, and then the segments are all
        // there is.
        self.image
            .segments
            .iter()
            .any(|s| s.executable && addr >= s.vaddr && addr < s.vaddr + s.mem_size)
    }
}

/// Delete flag writes that no reachable block ever reads.
///
/// Every `cmp`, `adds` and `subs` writes all four of NZCV and the following
/// branch reads one or two; across the whole graph the rest are almost always
/// dead, and dropping the write lets [`pass::dce`] delete the arithmetic that
/// produced it.
///
/// An ordinary backwards liveness fixpoint. A successor not among the blocks
/// in hand is assumed to read every flag, so an incomplete graph loses the
/// optimisation rather than producing wrong code.
///
/// The general-register analogue was built and measured: it removed 44
/// statements out of 27,614 instructions, because GCC had already allocated
/// its registers and dead cross-block stores essentially do not occur in
/// optimised code. Not worth it -- the register traffic is live values a
/// block boundary forces out of locals, which only regions and block
/// parameters help.
fn eliminate_dead_flags(blocks: &mut [Block]) -> usize {
    // A kill switch, so the transform can be measured against its absence and
    // a suspected miscompilation bisected.
    if std::env::var_os("GEX_NO_FLAG_LIVENESS").is_some() {
        return 0;
    }
    let index: HashMap<u64, usize> = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.start, i))
        .collect();
    let effects: Vec<(u64, u64)> = blocks.iter().map(pass::flag_effects).collect();
    let all = pass::ALL_FLAGS;

    // Successors as block indices in one flat array, resolved once: the
    // fixpoint walks them every round, so a fresh `Vec` and hash lookup per
    // round made flag liveness the largest phase on a 410,000-block run.
    //
    // A call's `call_return` counts as a successor even though it is not a
    // control-flow edge out: a call's flags are live if the callee or the
    // code it returns to reads them. Without it a `cmp; bl f; b.eq` (legal
    // when `f` preserves the flags) would lose its compare.
    let mut succ_at: Vec<u32> = Vec::with_capacity(blocks.len() + 1);
    let mut succ_of: Vec<u32> = Vec::new();
    // A block that can leave the graph keeps everything live, and so does one
    // with a successor nobody translated.
    let mut opaque: Vec<bool> = Vec::with_capacity(blocks.len());
    for b in blocks.iter() {
        succ_at.push(succ_of.len() as u32);
        let mut unknown = false;
        for s in b.successors().chain(b.call_return) {
            match index.get(&s) {
                Some(&i) => succ_of.push(i as u32),
                None => unknown = true,
            }
        }
        opaque.push(unknown || pass::escapes(b));
    }
    succ_at.push(succ_of.len() as u32);
    let succs_of = |i: usize| -> &[u32] {
        &succ_of[succ_at[i] as usize..succ_at[i + 1] as usize]
    };

    let mut live_in = vec![0u64; blocks.len()];
    loop {
        let mut changed = false;
        for i in 0..blocks.len() {
            let live_out = if opaque[i] {
                all
            } else {
                succs_of(i)
                    .iter()
                    .fold(0, |a, &s| a | live_in[s as usize])
            };
            let (uses, defs) = effects[i];
            let next = uses | (live_out & !defs);
            if next != live_in[i] {
                live_in[i] = next;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // What each block may drop, settled before anything is rewritten, so the
    // rewrite reads nothing that another thread is changing.
    let dead: Vec<u64> = (0..blocks.len())
        .map(|i| {
            if opaque[i] {
                return 0;
            }
            let live_out = succs_of(i)
                .iter()
                .fold(0, |a, &s| a | live_in[s as usize]);
            all & !live_out
        })
        .collect();

    // Each block's rewrite touches only that block, so they all run at once.
    gex_emit::par::map_mut(blocks, &|i: usize, b: &mut Block| -> usize {
        let flags = pass::flags_of_mask(dead[i]);
        if flags.is_empty() {
            return 0;
        }
        let before = b.stmts.len();
        pass::dead_store_elim_with(b, &flags);
        pass::dce(b);
        before - b.stmts.len()
    })
    .iter()
    .sum::<usize>()
}


/// Print how long each phase of a translation took, when `GEX_TIMING` is set.
/// The balance between passes is not obvious and it moves as they change.
struct Phases {
    on: bool,
    last: gex_emit::par::Stopwatch,
}

impl Phases {
    fn start() -> Phases {
        Phases {
            on: std::env::var_os("GEX_TIMING").is_some(),
            last: gex_emit::par::Stopwatch::start(),
        }
    }

    fn mark(&mut self, what: &str) {
        if self.on {
            eprintln!("[timing] {what}: {:.2}s", self.last.secs());
        }
        self.last = gex_emit::par::Stopwatch::start();
    }
}

/// Discover and lift every block of `image` the options ask for, returning them
/// address-sorted and dead-flag-eliminated, plus the stats. The module emission
/// is left to the caller: [`translate_image`] builds one module, and
/// [`translate_image_split`] builds several when the object is too big for one.
fn image_blocks(image: &Image, opts: &Options) -> (Vec<Block>, Stats) {
    if opts.no_blocks {
        return (Vec::new(), Stats::default());
    }
    let mut phases = Phases::start();
    let view = CodeView::new(image);
    let mut stats = Stats::default();

    // Collect entry points. A BTreeSet keeps translation order deterministic,
    // which keeps the emitted module byte-identical across runs and therefore
    // usable as a content-addressed cache key.
    let mut worklist: BTreeSet<u64> = BTreeSet::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut collected: Vec<Block> = Vec::new();

    // A shared library's `e_entry` is zero, so `image.entry` is the load bias
    // and points at the ELF header; only an executable has a real entry point,
    // never at offset zero. Comparing against the bias tells them apart and
    // avoids disassembling "\x7fELF".
    if image.entry != image.bias && view.is_executable(image.entry) {
        worklist.insert(image.entry);
    }
    if !opts.lazy {
        for sym in &image.symbols {
            if view.is_executable(sym.value) && sym.value % INST_ALIGN == 0 {
                worklist.insert(sym.value);
            }
        }
    }
    for &a in &opts.extra_entries {
        if view.is_executable(a) {
            worklist.insert(a);
        }
    }
    if opts.scan_pointers && !opts.lazy {
        let before = worklist.len();
        for seg in &image.segments {
            for (i, chunk) in seg.bytes.chunks_exact(8).enumerate() {
                // Only naturally aligned words can be pointers.
                if !(seg.vaddr as usize + i * 8).is_multiple_of(8) {
                    continue;
                }
                let v = u64::from_le_bytes(chunk.try_into().unwrap());
                if v % INST_ALIGN == 0 && view.is_executable(v) {
                    worklist.insert(v);
                }
            }
        }
        stats.discovered_from_data = worklist.len() - before;

        // Code-embedded pointers. A function whose address is *taken* rather than
        // called -- `adrp`+`add` materialising its address into a register to be
        // stored in a struct and later called indirectly -- is reached by no
        // branch and lies in no data word, so neither successor-following nor the
        // data scan above finds it. libgcrypt installs its EdDSA field-arithmetic
        // ops exactly this way: `ec_p_init` writes `ctx->addm = ec_addm_25519`
        // and the verify calls through `ctx->addm`. A closure that misses them
        // leaves those blocks to a demand-fill into a page it already partly
        // covers -- the dispatch corruption that made `gpgv` abort on an EdDSA
        // `InRelease` signature ("lock ABI version mismatch"). So scan the code
        // itself for an `adr`/`adrp+add` that resolves to executable text.
        //
        // Over-seeding is safe: an address that is really data is dropped by
        // `is_executable`, and one that lands mid-function only adds a block the
        // guest never dispatches to unless it truly is an entry. The register
        // tracking need not be precise for the same reason -- a stale `adrp` page
        // paired with a later `add` at worst seeds one more real code address.
        let before_code = worklist.len();
        for seg in &image.segments {
            if !seg.executable {
                continue;
            }
            arch::scan_code_pointers(
                &seg.bytes,
                seg.vaddr,
                &|a| view.is_executable(a),
                &mut |t| {
                    worklist.insert(t);
                },
            );
        }
        stats.discovered_from_data += worklist.len() - before_code;
    }

    if opts.exhaustive {
        let before = worklist.len();
        for seg in &image.segments {
            if !seg.executable {
                continue;
            }
            let end = seg.vaddr + seg.bytes.len() as u64;
            // Skip addresses that are not really instructions: an executable
            // segment is full of padding and literal pools, and for a shared
            // library also holds string tables, relocations and `.rodata`.
            // `is_executable` (behind `fetch_code`) uses the section flags to
            // say which bytes are code. Skipping the rest keeps exhaustive
            // mode affordable (three quarters of the slots on a static musl
            // binary) and avoids folding a `.rodata` block over a real entry
            // point's slot -- the bug in #46 that broke `apt` on libc. The
            // entries found so far anchor the sweep (see `arch::sweep`).
            let anchors: Vec<u64> = worklist.range(seg.vaddr..end).copied().collect();
            let mut found: Vec<u64> = Vec::new();
            arch::sweep(
                &|pc, buf| view.fetch_code(pc, buf),
                seg.vaddr,
                end,
                &anchors,
                &mut |a| found.push(a),
            );
            worklist.extend(found);
        }
        stats.discovered_exhaustively = worklist.len().saturating_sub(before);
    }
    phases.mark("seeding");

    // Breadth-first, a level at a time, because a level lifts in parallel and
    // an address-ordered worklist does not. Each level is sorted, so block
    // order -- and the module hash the compiled-module cache is keyed on -- is
    // the same on every run regardless of thread scheduling.
    let mut frontier: Vec<u64> = worklist.iter().copied().collect();
    seen.extend(frontier.iter().copied());
    let show_illegal = std::env::var_os("GEX_SHOW_ILLEGAL").is_some();

    let compat = arch::compat();
    while !frontier.is_empty() {
        let lifted = gex_emit::par::map(&frontier, &|&addr| {
            // The lifting threads decode in this thread's mode (an image's
            // own, `set_thread_compat`), not only the process default.
            let _mode = compat_scope(compat);
            // A recognised library function is emitted as the shim's intrinsic
            // (a `memory.copy`/`memory.fill`) instead of lifting its own code --
            // the same substitution `discover` (the runtime path) does. Without
            // it the closure lifts glibc's hand-written SIMD `memcpy`/`memset`,
            // and a run that reaches one then depends on that translation being
            // right; a wrong lane there is a wild copy, exactly the heap
            // corruption `apt`/`gpgv` hit only with a closure. See `intrinsic`.
            if let Some(&k) = opts.intrinsics.get(&addr) {
                return intrinsic::block(addr, k);
            }
            // Stop at the *section* boundary, not the segment's: a `PF_X`
            // segment also maps the string tables, relocations and `.rodata`
            // that sit beside the code, and `CodeView::fetch` alone would keep
            // decoding those as instructions when a block runs off the end of a
            // real code section -- a garbage block that, dispatched to, executes
            // wild. `discover` (the runtime path) already gates its fetch on
            // `is_executable` for this reason; without the same gate here the
            // closure and the runtime disagree, and the closure lifts blocks the
            // runtime never would. This is the closure-only corruption `gpgv`
            // hit verifying an EdDSA signature.
            let mut block = lift_block(|pc, buf| view.fetch_code(pc, buf), addr, opts.limits);
            pass::optimize(&mut block);
            block
        });

        let mut next: Vec<u64> = Vec::new();
        for block in lifted {
            stats.blocks += 1;
            stats.instructions += ((block.end - block.start) / INST_ALIGN) as usize;
            if matches!(block.term, Term::Illegal { .. }) {
                stats.illegal_blocks += 1;
                // Name the instruction so the gap can be closed: a block that
                // reaches this is emitted, dispatched to, and raises SIGILL.
                if show_illegal {
                    if let Term::Illegal { raw, pc } = block.term {
                        eprintln!("[illegal] {raw:#010x} at {pc:#x}");
                    }
                }
            }

            // Both the successors and the return site of any call in this
            // block are entry points; a callee's `ret` branches straight to
            // the latter. Skipped for a `seeds_only` (profile-guided) build:
            // the seeds are already the reached set, and following their
            // branches only re-adds the never-taken code the profile exists to
            // leave out -- `complete_pages` still makes each touched page whole.
            if !opts.seeds_only {
                for succ in block.successors().chain(block.call_return) {
                    if !seen.contains(&succ) && view.is_executable(succ) {
                        seen.insert(succ);
                        next.push(succ);
                    }
                }
            }
            collected.push(block);
        }
        next.sort_unstable();
        frontier = next;
    }

    // Complete every page reachable discovery touched: lift each decodable
    // instruction on it that no branch or pointer reached, as a standalone block
    // whose successors are NOT followed. See `Options::complete_pages`.
    //
    // A page a closure covers only PARTLY is the hazard: its uncovered blocks
    // are reached through neither a branch nor a data pointer -- a C++ exception
    // landing pad the unwinder jumps to from `.eh_frame` is the case that hit
    // `apt` -- so no static discovery finds them, and they are demand-filled at
    // run time into a page whose other slots the closure already holds, a
    // partial page whose dispatch a wrong fill corrupts (the stack smash in
    // apt's gpgv method). Filling the gaps here makes every touched page WHOLE,
    // so nothing on it demand-fills. A page discovery never touched stays out of
    // the closure and demand-fills into empty slots, which is safe.
    //
    // Not followed and not in the frontier: following a completion block's
    // successors onto a fresh page would touch it, complete it, and cascade
    // through the whole object (libgcrypt reached 302 MB that way). Bounded to
    // touched pages, the growth is the gaps in code already carried, not the
    // rest of the library.
    if opts.complete_pages {
        let touched: std::collections::BTreeSet<u64> =
            collected.iter().map(|b| b.start & !0xfff).collect();
        let mut fill: Vec<u64> = Vec::new();
        // The blocks already on a page anchor its sweep, so on x86-64 the
        // gaps are filled from real instruction boundaries rather than from
        // every byte.
        let mut anchors: Vec<u64> = collected.iter().map(|b| b.start).collect();
        anchors.sort_unstable();
        for &pg in &touched {
            let lo = anchors.partition_point(|&a| a < pg);
            let hi = anchors.partition_point(|&a| a < pg + 0x1000);
            let page_anchors = &anchors[lo..hi];
            let mut found: Vec<u64> = Vec::new();
            arch::sweep(
                &|pc, buf| view.fetch_code(pc, buf),
                pg,
                pg + 0x1000,
                page_anchors,
                &mut |a| found.push(a),
            );
            for a in found {
                if !seen.contains(&a) {
                    seen.insert(a);
                    fill.push(a);
                }
            }
        }
        fill.sort_unstable();
        let lifted = gex_emit::par::map(&fill, &|&addr| {
            let _mode = compat_scope(compat);
            if let Some(&k) = opts.intrinsics.get(&addr) {
                return intrinsic::block(addr, k);
            }
            let mut block = lift_block(|pc, buf| view.fetch_code(pc, buf), addr, opts.limits);
            pass::optimize(&mut block);
            block
        });
        for block in lifted {
            stats.blocks += 1;
            stats.instructions += ((block.end - block.start) / INST_ALIGN) as usize;
            collected.push(block);
        }
    }

    phases.mark("lifting");

    // Address order: emission order fixes the function indices and so the
    // module bytes (cache key), and decides which blocks tier-1 folds into a
    // region. Neither should depend on how the lifting was scheduled.
    collected.sort_unstable_by_key(|b| b.start);

    // Flag liveness needs the whole graph, so it runs once discovery has
    // finished rather than per block.
    stats.dead_flag_stmts = eliminate_dead_flags(&mut collected);
    phases.mark("flag liveness");
    (collected, stats)
}

/// Build a module for `opts.first_slot`-based placement, either the one-shot
/// [`ModuleBuilder`] the whole-image emitters share.
fn image_builder(image: &Image, opts: &Options) -> ModuleBuilder {
    let (text_lo, text_hi) = image.text_range;
    // An image's code in the process's other mode (gex-eject's seeds of a
    // WoW64 program's 32-bit code): that mode's pages and directory.
    let alt = other_mode();
    let mut builder = if opts.first_slot == 0 && !alt {
        ModuleBuilder::with_linkage(text_lo, text_hi - text_lo, opts.linkage, opts.shared_memory)
    } else {
        let mut pages = gex_emit::module::PageTable::from_entries(&[], opts.first_slot);
        pages.set_alt(alt);
        ModuleBuilder::with_pages(text_lo, text_hi - text_lo, opts.linkage, pages, opts.shared_memory)
    };
    builder.set_demand_fill(opts.demand_fill);
    builder
}

/// Why `opts` cannot be built, if it cannot: the function tier with
/// profiling on (`GEX_PROFILE`, `--record`). The C side carries no
/// profiling hooks, so its counts would be lost without a word.
pub fn cgen_conflict(cgen: bool, profiling: bool) -> Option<&'static str> {
    (cgen && profiling).then_some(
        "the function tier (--cgen) cannot be built with profiling on (GEX_PROFILE=1, --record): \
         its C code is not instrumented, so the profile would silently lose everything it runs",
    )
}

pub fn translate_image(image: &Image, opts: &Options) -> (TranslatedModule, Stats) {
    translate_image_seeded(image, None, opts)
}

/// [`translate_image`] for a PE image from [`func::pe_input`]: the function
/// tier recovers its functions with [`func::recover_pe`] and those seeds
/// (`.pdata`, the entry, TLS callbacks, relocated code pointers).
pub fn translate_pe_image(image: &Image, seeds: &func::PeSeeds, opts: &Options) -> (TranslatedModule, Stats) {
    translate_image_seeded(image, Some(seeds), opts)
}

fn translate_image_seeded(image: &Image, pe: Option<&func::PeSeeds>, opts: &Options) -> (TranslatedModule, Stats) {
    if let Some(why) = cgen_conflict(opts.cgen || opts.fjit, profile::enabled()) {
        panic!("{why}");
    }
    let (collected, mut stats) = image_blocks(image, opts);
    let mut builder = image_builder(image, opts);
    let mut cgen: Vec<(Block, u32)> = Vec::new();
    for block in collected {
        let f = builder.reserve(block.start);
        if opts.cgen || opts.fjit {
            cgen.push((block.clone(), f));
        }
        builder.add_block(block);
    }
    let mut module = builder.finish();
    if opts.fjit {
        module.wasm = fjit_splice(&module.wasm, &cgen, image, pe, opts);
    } else if opts.cgen {
        module.wasm = cgen_splice(&module.wasm, &cgen, image, pe, opts);
    }
    stats.wasm_bytes = module.wasm.len();
    (module, stats)
}

/// The function tier's version of a tier-1 module (`Options::cgen`). Loud on
/// failure: a build that asked for the function tier must not quietly ship
/// tier-1.
#[cfg(feature = "cgen")]
fn cgen_splice(
    wasm: &[u8],
    blocks: &[(Block, u32)],
    image: &Image,
    pe: Option<&func::PeSeeds>,
    opts: &Options,
) -> Vec<u8> {
    // i386 (compatibility mode): the same tier, with a 4-byte return
    // address (gex-cgen's `class32`).
    let refs: Vec<(&Block, u32)> = blocks.iter().map(|(b, f)| (b, *f)).collect();
    let standin = gex_cgen::use_standin().unwrap_or_else(|e| panic!("function tier (--cgen): {e:#}"));
    let functions = if standin {
        gex_cgen::Functions::StandIn {
            symbols: image.symbols.iter().filter(|s| !s.ifunc).map(|s| s.value).collect(),
        }
    } else {
        // S2's recovery. No ELF bytes reach here, so no `.eh_frame` seeds:
        // symbols, the entry, relocations and call targets still seed it.
        let ropts = func::RecoverOptions {
            limits: opts.limits,
            ..Default::default()
        };
        // The image's mode for everything below, recovery to emission.
        let _mode = compat_scope(image.class32);
        let prog = match pe {
            Some(seeds) => func::recover_pe(image, seeds, &ropts),
            None => func::recover(image, None, &ropts),
        };
        let summaries = summary::analyze(&prog).map;
        // A profile-guided (`seeds_only`) build compiles only the functions
        // the seeds start: the hot set (`gexc build --functions`).
        let mut functions: Vec<_> = prog.functions.into_values().collect();
        if opts.seeds_only {
            let keep: HashSet<u64> = opts.extra_entries.iter().copied().collect();
            functions.retain(|f| keep.contains(&f.entry));
        }
        gex_cgen::Functions::Recovered {
            functions,
            intrinsics: opts.intrinsics.keys().copied().collect(),
            summaries,
        }
    };
    let byte = |a: u64| {
        image.segments.iter().find_map(|s| s.byte(a))
    };
    match gex_cgen::build(wasm, &refs, functions, image.class32, &byte) {
        Ok((w, _)) => w,
        Err(e) => panic!("function tier (--cgen): {e:#}"),
    }
}

/// The function JIT's version of a tier-1 module (`Options::fjit`): S2's
/// recovery and summaries as for `cgen_splice`, emitted by `gex-fjit`. Loud
/// on failure, as that is.
#[cfg(feature = "fjit")]
fn fjit_splice(
    wasm: &[u8],
    blocks: &[(Block, u32)],
    image: &Image,
    pe: Option<&func::PeSeeds>,
    opts: &Options,
) -> Vec<u8> {
    let refs: Vec<(&Block, u32)> = blocks.iter().map(|(b, f)| (b, *f)).collect();
    let ropts = func::RecoverOptions {
        limits: opts.limits,
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let _mode = compat_scope(image.class32);
    let prog = match pe {
        Some(seeds) => func::recover_pe(image, seeds, &ropts),
        None => func::recover(image, None, &ropts),
    };
    let summaries = summary::analyze(&prog).map;
    if std::env::var_os("GEX_FJIT_STATS").is_some() {
        eprintln!(
            "[fjit] recovery and summaries: {:.1} ms, {} functions",
            started.elapsed().as_secs_f64() * 1e3,
            prog.functions.len()
        );
    }
    let mut functions: Vec<_> = prog.functions.into_values().collect();
    if opts.seeds_only {
        let keep: HashSet<u64> = opts.extra_entries.iter().copied().collect();
        functions.retain(|f| keep.contains(&f.entry));
    }
    let intrinsics: Vec<u64> = opts.intrinsics.keys().copied().collect();
    let byte = |a: u64| {
        image.segments.iter().find_map(|s| s.byte(a))
    };
    let pops = fjit::ret_pops(&functions, image.class32, &byte);
    match gex_fjit::build(wasm, &refs, functions, &intrinsics, &summaries, image.class32, &pops) {
        Ok((w, _)) => w,
        Err(e) => panic!("function JIT (--fjit): {e:#}"),
    }
}

#[cfg(not(feature = "fjit"))]
fn fjit_splice(_: &[u8], _: &[(Block, u32)], _: &Image, _: Option<&func::PeSeeds>, _: &Options) -> Vec<u8> {
    panic!("Options::fjit is set, but gex-xlate was built without the `fjit` feature")
}

#[cfg(not(feature = "cgen"))]
fn cgen_splice(_: &[u8], _: &[(Block, u32)], _: &Image, _: Option<&func::PeSeeds>, _: &Options) -> Vec<u8> {
    panic!("Options::cgen is set, but gex-xlate was built without the `cgen` feature")
}

/// Like [`translate_image`], but emits SEVERAL modules when the object has more
/// than `max_funcs` blocks -- more than one wasm module may hold (libQt6Gui,
/// Mesa, libz3). Split by dispatch page, never within one, threading the page
/// table so later parts see earlier parts' slot assignments. An `Own` image
/// module is never split: it declares the memory and tables, and an executable
/// is always small enough for one module anyway.
pub fn translate_image_split(
    image: &Image,
    opts: &Options,
    max_funcs: usize,
) -> (Vec<TranslatedModule>, Stats) {
    let (collected, mut stats) = image_blocks(image, opts);
    if opts.linkage != Linkage::Shared || collected.len() <= max_funcs {
        let mut builder = image_builder(image, opts);
        for block in collected {
            builder.reserve(block.start);
            builder.add_block(block);
        }
        let module = builder.finish();
        stats.wasm_bytes = module.wasm.len();
        return (vec![module], stats);
    }
    // Split through the proven runtime splitter, threaded from this object's
    // starting slot and emitting the closure's passive segments (`demand_fill`).
    let mut page_table = gex_emit::module::PageTable::from_entries(&[], opts.first_slot);
    page_table.set_alt(other_mode());
    let modules = emit_modules(
        collected,
        image.text_range.0,
        page_table,
        opts.shared_memory,
        opts.demand_fill,
        max_funcs,
    );
    stats.wasm_bytes = modules.iter().map(|m| m.wasm.len()).sum();
    (modules, stats)
}

/// A module that translates nothing: the entry stub, the miss stub, and an
/// empty dispatch table. Every branch misses, so the runtime translator
/// produces the whole program -- usable with *any* guest, which a page that
/// lets a visitor supply their own binary needs. The trade is that the first
/// execution of every block pays a miss, a lift and a compile.
pub fn seed_module(shared_memory: bool) -> TranslatedModule {
    // A text range is still declared because the builder wants one; with no
    // blocks in it, nothing reads it back.
    ModuleBuilder::with_linkage(
        gex_abi::PIE_BASE,
        0,
        Linkage::Own,
        shared_memory,
    )
    .finish()
}

/// Statically recompile a whole mapped object: every decodable address in
/// `[lo, hi)`, not just what one entry point reaches.
///
/// `seeds` says where to start; empty means every decodable address in the
/// range. Empty is right for a stripped object and wrong for one with a
/// `.dynsym`, whose few thousand entry points beat scanning's two hundred
/// thousand -- most of them mid-function addresses an earlier block covers.
///
/// Discovery still runs (successors are followed, a PLT's GOT load is folded
/// so its indirect branch becomes a known target), but seeding from the whole
/// object rather than one entry point is what stops the output depending on
/// which address the guest reached first.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
pub fn translate_object(
    memory: &[u8],
    lo: u64,
    hi: u64,
    seeds: &[u64],
    executable: &(dyn Fn(u64) -> bool + Sync),
    known: &(dyn Fn(u64) -> bool + Sync),
    page_table: PageTable,
    limits: BlockLimits,
    max_blocks: usize,
    shared_memory: bool,
    intrinsics: &HashMap<u64, Intrinsic>,
) -> (Vec<TranslatedModule>, Stats) {
    // Addresses that do not begin a decodable instruction are skipped rather
    // than translated into a block that can only report SIGILL. On a static
    // musl binary that is about three quarters of them, and the same holds
    // here: an executable segment is full of padding and literal pools.
    let mut scanned: Vec<u64> = Vec::new();
    let seeds = if seeds.is_empty() {
        arch::sweep(
            &|pc, buf| fetch_code_bytes(memory, executable, pc, buf),
            lo,
            hi,
            &[],
            &mut |a| scanned.push(a),
        );
        &scanned[..]
    } else {
        seeds
    };
    stage("seeds", seeds.len());
    let (blocks, mut stats) = discover(
        memory,
        lo,
        seeds,
        executable,
        known,
        limits,
        max_blocks,
        usize::MAX,
        intrinsics,
    );
    stage("discovered", blocks.len());
    let modules = emit_modules(blocks, lo, page_table, shared_memory, false, MAX_FUNCS_PER_MODULE);
    stage("emitted", modules.len());
    stats.wasm_bytes = modules.iter().map(|m| m.wasm.len()).sum();
    (modules, stats)
}

/// Announce where a whole-object lift has got to, one line per stage on
/// stderr, when `GEX_XLATE_STAGES=1`. Helps locate which stage holds the
/// memory peak on a large object. The resident figure comes from `/proc` and
/// is native-only; the shim prints the count alone.
fn stage(what: &str, n: usize) {
    if std::env::var_os("GEX_XLATE_STAGES").is_none() {
        return;
    }
    let rss = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))?
                .split_whitespace()
                .nth(1)?
                .parse::<usize>()
                .ok()
        })
        .map_or(String::new(), |kib| format!("  resident {} MiB", kib / 1024));
    eprintln!("  xlate {what:12} {n:>9}{rss}");
}

/// How many block functions gex will put in one wasm module.
///
/// wasmtime refuses a module with more than a million functions, so an object
/// with more blocks (Mesa, libz3) is split into several. Well under the
/// ceiling on purpose: a split turns same-module direct tail calls into table
/// dispatches, so fewer, fuller modules are faster, and the chunking keeps
/// consecutive pages together so crossing edges are the rare long-range ones.
pub const MAX_FUNCS_PER_MODULE: usize = 500_000;

/// Encode discovered blocks as one module, or several if too many for one.
///
/// The split is by dispatch page, never within one: a page's element segment
/// is written whole by whichever module claims it, so two modules sharing a
/// page would have the second overwrite the first's slots with the stub.
fn emit_modules(
    blocks: Vec<Block>,
    text_base: u64,
    page_table: PageTable,
    shared_memory: bool,
    demand_fill: bool,
    max_funcs: usize,
) -> Vec<TranslatedModule> {
    if blocks.len() <= max_funcs {
        let mut builder = ModuleBuilder::with_pages(
            text_base,
            0,
            Linkage::Shared,
            page_table,
            shared_memory,
        );
        builder.set_demand_fill(demand_fill);
        for block in blocks {
            builder.add_block(block);
        }
        return vec![builder.finish()];
    }

    // Group by page, then fill one chunk at a time in address order. A page is
    // never split, so a chunk may overshoot the budget by one page's blocks.
    let mut by_page: BTreeMap<u64, Vec<Block>> = BTreeMap::new();
    for block in blocks {
        by_page
            .entry(block.start / gex_abi::DISPATCH_PAGE)
            .or_default()
            .push(block);
    }
    stage("grouped", by_page.len());

    let mut modules = Vec::new();
    // Threaded from one chunk to the next: page assignments the earlier
    // chunks made have to be visible to the later ones, or a page would be
    // given two different slot bases.
    let mut pages = page_table;
    let mut chunk: Vec<Block> = Vec::new();
    let mut flush = |chunk: &mut Vec<Block>, pages: &mut PageTable, modules: &mut Vec<_>| {
        if chunk.is_empty() {
            return;
        }
        let mut builder = ModuleBuilder::with_pages(
            text_base,
            0,
            Linkage::Shared,
            std::mem::take(pages),
            shared_memory,
        );
        builder.set_demand_fill(demand_fill);
        for block in chunk.drain(..) {
            builder.add_block(block);
        }
        let m = builder.finish();
        *pages = m.page_table.clone();
        modules.push(m);
    };
    for (_page, page_blocks) in by_page {
        if !chunk.is_empty() && chunk.len() + page_blocks.len() > max_funcs {
            flush(&mut chunk, &mut pages, &mut modules);
        }
        chunk.extend(page_blocks);
    }
    flush(&mut chunk, &mut pages, &mut modules);
    modules
}

#[allow(clippy::too_many_arguments)]
pub fn translate_runtime(
    memory: &[u8],
    entry: u64,
    executable: &(dyn Fn(u64) -> bool + Sync),
    known: &(dyn Fn(u64) -> bool + Sync),
    page_table: PageTable,
    limits: BlockLimits,
    max_blocks: usize,
    shared_memory: bool,
    intrinsics: &HashMap<u64, Intrinsic>,
) -> (TranslatedModule, Stats) {
    translate_seeded(
        memory,
        entry,
        &[entry],
        executable,
        known,
        page_table,
        limits,
        max_blocks,
        shared_memory,
        intrinsics,
    )
}

/// Tier-1 stubs at `entries`, for the function tier to splice over: lifted
/// from live memory (plus whatever shares their dispatch slots), in one
/// module against `page_table`, and each with its function index.
#[cfg(feature = "cgen")]
fn function_stubs(
    memory: &[u8],
    entries: &[u64],
    executable: &(dyn Fn(u64) -> bool + Sync),
    known: &(dyn Fn(u64) -> bool + Sync),
    page_table: PageTable,
    limits: BlockLimits,
    shared_memory: bool,
) -> Result<(TranslatedModule, Vec<(Block, u32)>), String> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut have: HashSet<u64> = HashSet::new();
    for &e in entries {
        if have.insert(e) && fetchable(memory, executable, e) && !known(e) {
            blocks.push(lift_one(e, memory, executable, limits).0);
        }
    }
    // As `translate_seeded`: blocks sharing a dispatch slot go together.
    if gex_abi::DISPATCH_SHIFT > gex_abi::INST_ALIGN_SHIFT {
        let span = 1u64 << gex_abi::DISPATCH_SHIFT;
        let mut i = 0;
        while i < blocks.len() {
            let lo = blocks[i].start & !(span - 1);
            for b in lo..lo + span {
                if !have.contains(&b) && known(b) && fetchable(memory, executable, b) {
                    have.insert(b);
                    blocks.push(lift_one(b, memory, executable, limits).0);
                }
            }
            i += 1;
        }
    }
    if blocks.is_empty() {
        return Err(format!("none of the {} function entries is translatable", entries.len()));
    }
    blocks.sort_by_key(|b| b.start);
    let mut page_table = page_table;
    page_table.set_alt(other_mode());
    let mut builder = ModuleBuilder::with_pages(blocks[0].start, 0, Linkage::Shared, page_table, shared_memory);
    let mut out: Vec<(Block, u32)> = Vec::new();
    for block in blocks {
        let f = builder.reserve(block.start);
        out.push((block.clone(), f));
        builder.add_block(block);
    }
    Ok((builder.finish(), out))
}

/// The function tier for code already in memory, at run time: a unit with a
/// tier-1 stub at each of `entries` (registered in dispatch) and, spliced
/// over them, the C of `functions` (gex-cgen). Nothing else is translated:
/// every other address stays the runtime translator's.
#[cfg(feature = "cgen")]
#[allow(clippy::too_many_arguments)]
pub fn translate_runtime_functions(
    memory: &[u8],
    entries: &[u64],
    executable: &(dyn Fn(u64) -> bool + Sync),
    known: &(dyn Fn(u64) -> bool + Sync),
    page_table: PageTable,
    limits: BlockLimits,
    shared_memory: bool,
    functions: Vec<gex_ir::func::Function>,
    summaries: HashMap<u64, gex_ir::func::Summary>,
    class32: bool,
) -> Result<(TranslatedModule, Stats), String> {
    let (mut module, stubs) = function_stubs(memory, entries, executable, known, page_table, limits, shared_memory)?;
    let refs: Vec<(&Block, u32)> = stubs.iter().map(|(b, f)| (b, *f)).collect();
    let (wasm, _) = gex_cgen::build(
        &module.wasm,
        &refs,
        gex_cgen::Functions::Recovered { functions, intrinsics: Vec::new(), summaries },
        class32,
        &|a| memory.get(a as usize).copied(),
    )
    .map_err(|e| format!("{e:#}"))?;
    module.wasm = wasm;
    let stats = Stats { blocks: stubs.len(), wasm_bytes: module.wasm.len(), ..Default::default() };
    Ok((module, stats))
}

/// [`translate_runtime_functions`] with the C already built and linked
/// (`gex_cgen::image::build_c_module`, which exports `e_<entry>` for every
/// function): stubs at those entries, and the module spliced over them.
#[cfg(feature = "cgen")]
#[allow(clippy::too_many_arguments)]
pub fn translate_runtime_prebuilt(
    memory: &[u8],
    executable: &(dyn Fn(u64) -> bool + Sync),
    known: &(dyn Fn(u64) -> bool + Sync),
    page_table: PageTable,
    limits: BlockLimits,
    shared_memory: bool,
    cwasm: &[u8],
) -> Result<(TranslatedModule, Stats), String> {
    let entries = gex_cgen::image::exported_entries(cwasm).map_err(|e| format!("{e:#}"))?;
    let (mut module, stubs) = function_stubs(memory, &entries, executable, known, page_table, limits, shared_memory)?;
    let pairs: Vec<(u64, u32)> = stubs.iter().filter(|(b, _)| entries.binary_search(&b.start).is_ok()).map(|(b, f)| (b.start, *f)).collect();
    let (wasm, _) = gex_cgen::splice::splice(&module.wasm, cwasm, &pairs, &gex_cgen::func::entry_name, false)
        .map_err(|e| format!("{e:#}"))?;
    module.wasm = wasm;
    let stats = Stats { blocks: stubs.len(), wasm_bytes: module.wasm.len(), ..Default::default() };
    Ok((module, stats))
}

/// The body both paths share: lift from live memory, following what each
/// block reaches, starting from whatever was seeded, and encode the result as
/// one module.
#[allow(clippy::too_many_arguments)]
fn translate_seeded(
    memory: &[u8],
    entry: u64,
    seeds: &[u64],
    executable: &(dyn Fn(u64) -> bool + Sync),
    known: &(dyn Fn(u64) -> bool + Sync),
    page_table: PageTable,
    limits: BlockLimits,
    max_blocks: usize,
    shared_memory: bool,
    intrinsics: &HashMap<u64, Intrinsic>,
) -> (TranslatedModule, Stats) {
    // Code in the process's other mode: its own pages and directory.
    let mut page_table = page_table;
    page_table.set_alt(other_mode());
    let (mut blocks, mut stats) = discover(
        memory, entry, seeds, executable, known, limits, max_blocks, MISS_CODE_BUDGET, intrinsics,
    );
    // A dispatch slot spans more bytes than an instruction (x86-64): every
    // block already translated that shares a slot with one of these has to be
    // in the same module, or this module's slot write would hide it. Lift
    // those again here; the slot then gets a trampoline covering all of them.
    if gex_abi::DISPATCH_SHIFT > gex_abi::INST_ALIGN_SHIFT {
        let span = 1u64 << gex_abi::DISPATCH_SHIFT;
        let mut have: HashSet<u64> = blocks.iter().map(|b| b.start).collect();
        let mut i = 0;
        while i < blocks.len() {
            let a = blocks[i].start;
            let lo = a & !(span - 1);
            for b in lo..lo + span {
                if b != a && !have.contains(&b) && known(b) && fetchable(memory, executable, b) {
                    have.insert(b);
                    let (blk, _) = match intrinsics.get(&b) {
                        Some(&k) => (intrinsic::block(b, k), None),
                        None => lift_one(b, memory, executable, limits),
                    };
                    stats.blocks += 1;
                    blocks.push(blk);
                }
            }
            i += 1;
        }
    }
    let mut modules = emit_modules(blocks, entry, page_table, shared_memory, false, usize::MAX);
    let module = modules.pop().expect("an unbounded budget emits one module");
    stats.wasm_bytes = module.wasm.len();
    (module, stats)
}

/// Edges discovery does not follow (an experiment knob, `set_discover_skip`):
/// bit 0 call returns, bit 1 call targets folded through memory, bit 2 the
/// block's own successors; bit 8 the discovery log; bits 16-23 a depth limit.
static DISCOVER_SKIP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// With `DISCOVER_SKIP` bit 8: every block discovery lifts, as
/// `(address, why, depth)`, for [`take_discover_log`]. `why`: 0 a seed, 1 a
/// conditional branch's taken side, 2 its fall-through, 3 a sole successor
/// (goto, direct call, syscall return), 4 a call's return address, 5 a call
/// target folded through memory. Depth is the breadth-first level.
static DISCOVER_LOG: std::sync::Mutex<Vec<(u64, u8, u16)>> = std::sync::Mutex::new(Vec::new());

/// The discovery log so far (`DISCOVER_SKIP` bit 8), emptied.
pub fn take_discover_log() -> Vec<(u64, u8, u16)> {
    std::mem::take(&mut *DISCOVER_LOG.lock().unwrap_or_else(|e| e.into_inner()))
}

/// See `DISCOVER_SKIP`.
pub fn set_discover_skip(bits: u32) {
    DISCOVER_SKIP.store(bits, std::sync::atomic::Ordering::Relaxed);
}

/// Lift every block reachable from the seeds.
///
/// Separate from encoding because one object's blocks may not fit in one
/// module -- see [`emit_modules`].
#[allow(clippy::too_many_arguments)]
/// The most guest code (bytes) one runtime miss lifts (`translate_runtime`;
/// a whole-image translation has none), whatever `max_blocks` allows: blocks end at `BlockLimits::max_insts` and fall through, so a run of
/// straight-line bytes -- a packed image's unpacking target, still zeros when
/// its stub jumps there -- was `max_blocks` full blocks in one module (an
/// AutoHotkey installer's first miss: a 68 MiB buffer and an arena out of
/// room). Ordinary code is far under it: 2048 blocks of a few instructions.
pub const MISS_CODE_BUDGET: usize = 128 << 10;

fn discover(
    memory: &[u8],
    entry: u64,
    seeds: &[u64],
    executable: &(dyn Fn(u64) -> bool + Sync),
    known: &(dyn Fn(u64) -> bool + Sync),
    limits: BlockLimits,
    max_blocks: usize,
    code_budget: usize,
    intrinsics: &HashMap<u64, Intrinsic>,
) -> (Vec<Block>, Stats) {
    let mut stats = Stats::default();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut collected: Vec<Block> = Vec::new();

    // Breadth-first, a level at a time, because a level lifts in parallel and
    // an arbitrary worklist does not. Each level is sorted, so the output is
    // identical across runs, which the content-addressed module cache needs.
    let mut frontier: Vec<u64> = seeds
        .iter()
        .copied()
        .filter(|a| *a == entry || !known(*a))
        .collect();
    frontier.sort_unstable();
    frontier.dedup();
    seen.extend(frontier.iter().copied());
    let logging = DISCOVER_SKIP.load(std::sync::atomic::Ordering::Relaxed) & 256 != 0;
    // Why each frontier address was queued (`DISCOVER_LOG`).
    let mut why: HashMap<u64, u8> = HashMap::new();
    let mut depth: u16 = 0;

    // Bits 16-23 of `DISCOVER_SKIP`: the deepest breadth-first level lifted
    // past the seeds (0: no limit). An experiment: 63% of the blocks node's
    // start-up lifts never run, and the deeper the level the fewer run.
    let max_depth = (DISCOVER_SKIP.load(std::sync::atomic::Ordering::Relaxed) >> 16) & 0xff;
    let compat = arch::compat();
    while !frontier.is_empty()
        && stats.blocks < max_blocks
        && stats.instructions * INST_ALIGN as usize <= code_budget
        && (max_depth == 0 || u32::from(depth) <= max_depth)
    {
        if frontier.len() + stats.blocks > max_blocks {
            frontier.truncate(max_blocks - stats.blocks);
        }
        let lifted = gex_emit::par::map(&frontier, &|&a| {
            let _mode = compat_scope(compat);
            match intrinsics.get(&a) {
                // Not lifted at all, so discovery never walks into the
                // function's body and the module never carries it.
                Some(&k) => (intrinsic::block(a, k), None),
                None => lift_one(a, memory, executable, limits),
            }
        });

        let mut next: Vec<u64> = Vec::new();
        if logging {
            let mut log = DISCOVER_LOG.lock().unwrap_or_else(|e| e.into_inner());
            for (block, _) in &lifted {
                log.push((block.start, why.get(&block.start).copied().unwrap_or(0), depth));
            }
        }
        depth = depth.saturating_add(1);
        for (block, folded) in lifted {
            stats.blocks += 1;
            stats.instructions += ((block.end - block.start) / INST_ALIGN) as usize;
            if matches!(block.term, Term::Illegal { .. }) {
                stats.illegal_blocks += 1;
            }

            // A computed branch whose target folds to a constant through
            // memory is worth following: that is every cross-object call in
            // a dynamic program. Folded on a scratch copy, never in the block
            // that is emitted: memory read at translation time is a guess
            // about the future -- a GOT slot lazily bound later, a global a
            // store has not reached yet -- and code that bakes it in is wrong
            // the moment it changes. Discovery can afford a wrong guess (an
            // unneeded block); execution cannot.
            let mask = DISCOVER_SKIP.load(std::sync::atomic::Ordering::Relaxed);
            let mut targets: Vec<(u64, u8)> = Vec::new();
            if mask & 4 == 0 {
                match block.term {
                    Term::Branch { taken, not_taken, .. } => {
                        targets.push((taken, 1));
                        targets.push((not_taken, 2));
                    }
                    _ => targets.extend(block.successors().map(|t| (t, 3))),
                }
            }
            if mask & 1 == 0 {
                targets.extend(block.call_return.map(|t| (t, 4)));
            }
            if let Some(t) = folded.filter(|_| mask & 2 == 0) {
                stats.discovered_from_data += 1;
                targets.push((t, 5));
            }
            for (succ, kind) in targets {
                // Deliberately *not* skipping an address inside a block
                // already lifted: an interior address is still a legal
                // indirect-branch target, and dispatch needs a block starting
                // exactly there. Entering the containing block would re-run
                // the instructions before it.
                if fetchable(memory, executable, succ)
                    && !seen.contains(&succ)
                    // Never re-translate something the table already answers
                    // for; the existing entry may be a tier-2 region.
                    && !known(succ)
                {
                    seen.insert(succ);
                    next.push(succ);
                    if logging {
                        why.insert(succ, kind);
                    }
                }
            }
            collected.push(block);
        }
        next.sort_unstable();
        frontier = next;
    }

    // Address order, as the worklist this replaced produced. See the note in
    // `translate_image`: emission order decides the module's bytes.
    collected.sort_unstable_by_key(|b| b.start);
    stats.dead_flag_stmts = eliminate_dead_flags(&mut collected);
    (collected, stats)
}

/// Whether `pc` holds an instruction this translation may read.
fn fetchable(memory: &[u8], executable: &(dyn Fn(u64) -> bool + Sync), pc: u64) -> bool {
    pc % INST_ALIGN == 0
        && executable(pc)
        && pc as usize + INST_ALIGN as usize <= memory.len()
}

/// Copy the instruction bytes at `pc` out of live memory, stopping at the
/// first byte that is not executable, so a block never decodes past the end
/// of a mapping.
fn fetch_code_bytes(
    memory: &[u8],
    executable: &(dyn Fn(u64) -> bool + Sync),
    pc: u64,
    buf: &mut arch::FetchBuf,
) -> usize {
    if !fetchable(memory, executable, pc) {
        return 0;
    }
    let off = pc as usize;
    let mut n = (memory.len() - off).min(16);
    // The first `INST_ALIGN` bytes are known executable; check the rest.
    let mut k = INST_ALIGN as usize;
    while k < n {
        if !executable(pc + k as u64) {
            n = k;
            break;
        }
        k += 1;
    }
    buf[..n].copy_from_slice(&memory[off..off + n]);
    n
}

/// Lift one block, and separately work out where its indirect branch (if
/// any) points *right now* by folding through memory, so a PLT's `br`/`jmp`
/// through a GOT slot becomes a target discovery can follow.
///
/// The folding happens on a scratch copy. The block returned for emission
/// reads memory at run time like any other: what a slot holds at translation
/// time is not what it holds later (lazy binding, a store this translation
/// ran ahead of), and the dispatch table, not the guess, decides the target.
/// Address ranges that are writable AND executable right now: a JIT's code
/// area. Blocks lifted from them carry a byte guard (see `Block::guard`). The
/// host refreshes this before each translation.
static VOLATILE_RANGES: std::sync::Mutex<Vec<(u64, u64)>> = std::sync::Mutex::new(Vec::new());

/// Whether the code this translator sees is 32-bit (i386 compatibility
/// mode), as the process default: a 32-bit program is 32-bit throughout.
/// Code of the other class in the same process is translated under
/// [`set_thread_compat`] or [`compat_scope`]. Clears this thread's own
/// setting. On the AArch64 guest there is no such mode and this is a no-op.
pub fn set_compat(compat: bool) {
    #[cfg(feature = "x86")]
    gex_isa_x86::set_mode(if compat {
        gex_isa_x86::Mode::Compat
    } else {
        gex_isa_x86::Mode::Long
    });
    #[cfg(not(feature = "x86"))]
    let _ = compat;
}

/// Whether the code this thread translates next is 32-bit, until the next
/// call on this thread or [`set_compat`] there: what translating one image
/// sets (`image.class32`). Other threads keep their own, so two threads
/// translating images of different classes (a WoW64 process) do not race.
pub fn set_thread_compat(compat: bool) {
    #[cfg(feature = "x86")]
    gex_isa_x86::set_thread_mode(if compat {
        gex_isa_x86::Mode::Compat
    } else {
        gex_isa_x86::Mode::Long
    });
    #[cfg(not(feature = "x86"))]
    let _ = compat;
}

/// [`set_thread_compat`] for as long as the scope lives; dropping it restores
/// what this thread had before.
#[must_use = "the mode holds only while the scope is alive"]
pub struct CompatScope {
    #[cfg(feature = "x86")]
    _scope: gex_isa_x86::ModeScope,
}

pub fn compat_scope(compat: bool) -> CompatScope {
    CompatScope {
        #[cfg(feature = "x86")]
        _scope: gex_isa_x86::ModeScope::enter(if compat {
            gex_isa_x86::Mode::Compat
        } else {
            gex_isa_x86::Mode::Long
        }),
    }
}

/// Whether the code this thread translates is in the mode the process's own
/// class is not (a WoW64 process's 32-bit code): its pages are keyed
/// `gex_abi::ALT_PAGE` and its modules read the other directory.
pub fn other_mode() -> bool {
    arch::compat() != default_compat()
}

/// The process default [`set_compat`] set, whatever this thread has open.
pub fn default_compat() -> bool {
    #[cfg(feature = "x86")]
    {
        gex_isa_x86::default_mode() == gex_isa_x86::Mode::Compat
    }
    #[cfg(not(feature = "x86"))]
    false
}

/// The mode code under the thread's selector (`ctx::CS`) is translated in:
/// a scope for one translation the thread's own code asked for.
pub fn cs_scope(cs: u32) -> CompatScope {
    compat_scope(gex_abi::ctx::cs_compat(cs, default_compat()))
}

pub fn set_volatile_ranges(ranges: Vec<(u64, u64)>) {
    *VOLATILE_RANGES.lock().unwrap_or_else(|e| e.into_inner()) = ranges;
}

fn is_volatile(addr: u64) -> bool {
    VOLATILE_RANGES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|(s, e)| addr >= *s && addr < *e)
}

/// The most bytes a guard compares: a block longer than this is guarded on
/// its head only, which still catches a JIT reusing the region.
const GUARD_MAX: usize = 256;

/// How much of `memory` exists: on wasm, no more than the memory's current
/// size (a guest slice names more than has been grown).
fn live_len(memory: &[u8]) -> usize {
    #[cfg(target_arch = "wasm32")]
    {
        ((core::arch::wasm32::memory_size(0) as u64) << 16).min(memory.len() as u64) as usize
    }
    #[cfg(target_arch = "wasm64")]
    {
        ((core::arch::wasm64::memory_size(0) as u64) << 16).min(memory.len() as u64) as usize
    }
    #[cfg(not(target_family = "wasm"))]
    {
        memory.len()
    }
}

fn lift_one(
    addr: u64,
    memory: &[u8],
    executable: &(dyn Fn(u64) -> bool + Sync),
    limits: BlockLimits,
) -> (Block, Option<u64>) {
    // The guard's bytes are taken before the block is decoded: another
    // thread may be writing this code (a JIT emitting a method or a
    // trampoline next to code already running), and bytes read after the
    // decode would vouch for a translation of the earlier ones.
    let snapshot: Option<Vec<u8>> = is_volatile(addr).then(|| {
        let lo = (addr as usize).min(memory.len());
        let hi = lo.saturating_add(GUARD_MAX).min(memory.len());
        memory[lo..hi].to_vec()
    });
    let fetch = |pc: u64, buf: &mut arch::FetchBuf| fetch_code_bytes(memory, executable, pc, buf);
    let mut block = lift_block(fetch, addr, limits);
    // `memory` may span the whole nameable space (the page's guest slice):
    // only what the memory has grown to is read. A constant address in the
    // pages KUSER_SHARED_DATA folding moves reads them where they are.
    let live = live_len(memory);
    let read = |a: u64, bytes: u32| -> Option<u64> {
        let a = gex_abi::fold_kuser(a).unwrap_or(a);
        let off = usize::try_from(a).ok()?;
        if off.checked_add(bytes as usize)? > live {
            return None;
        }
        Some(match bytes {
            1 => memory[off] as u64,
            2 => u16::from_le_bytes(memory[off..off + 2].try_into().unwrap()) as u64,
            4 => u32::from_le_bytes(memory[off..off + 4].try_into().unwrap()) as u64,
            _ => u64::from_le_bytes(memory[off..off + 8].try_into().unwrap()),
        })
    };
    if !gex_abi::X86 {
        // A64: folded through memory before the ordinary passes, so a PLT's
        // GOT load becomes a constant and its `br` becomes a target discovery
        // can follow.
        pass::const_fold_with_memory(&mut block, &read);
        pass::optimize(&mut block);
        let folded = match block.term {
            Term::Indirect { target, .. } => pass::const_value(&block, target),
            _ => None,
        };
        return (block, folded);
    }
    pass::optimize(&mut block);
    // Self-modifying code with no flush instruction to announce it.
    // TEST(GEX_TEST_LIFT_DELAY_US): widen the window between decoding a
    // JIT-code block and fixing its guard (a test-only switch).
    if snapshot.is_some() {
        static DELAY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let d = *DELAY.get_or_init(|| std::env::var("GEX_TEST_LIFT_DELAY_US").ok().and_then(|v| v.parse().ok()).unwrap_or(0));
        if d > 0 {
            std::thread::sleep(std::time::Duration::from_micros(d));
        }
    }
    if let Some(snap) = snapshot {
        let n = ((block.end as usize).max(addr as usize + 1) - addr as usize).min(snap.len());
        if n > 0 {
            block.guard = Some(snap[..n].to_vec());
        }
    }
    // x86-64: folded on a scratch copy only, never in the emitted block --
    // memory read at translation time is a guess about the future (a GOT slot
    // bound later), and code that bakes it in is wrong once it changes.
    let folded = match block.term {
        Term::Indirect { target, .. } => {
            let mut probe = block.clone();
            pass::const_fold_with_memory(&mut probe, &read);
            pass::const_value(&probe, target)
        }
        _ => None,
    };
    (block, folded)
}

#[cfg(all(test, not(feature = "x86")))]
mod tests {
    /// The same code translated twice, as two runs would: under
    /// `with_module_id_global` the module id is not in the bytes, so the two
    /// are the same bytes (what a cache keyed by them needs); compiled in,
    /// the second module's id makes them differ.
    #[test]
    fn a_module_id_in_a_global_leaves_the_bytes_alike() {
        // add x0, x0, #1; ret -- at 0x10000, its page numbered afresh each time.
        let mut mem = vec![0u8; 0x20000];
        mem[0x10000..0x10004].copy_from_slice(&0x9100_0400u32.to_le_bytes());
        mem[0x10004..0x10008].copy_from_slice(&0xd65f_03c0u32.to_le_bytes());
        let once = || {
            translate_runtime(
                &mem,
                0x10000,
                &|a| (0x10000..0x11000).contains(&a),
                &|_| false,
                PageTable::new(),
                BlockLimits::default(),
                16,
                true,
                &Default::default(),
            )
            .0
        };
        let (a, b) = (gex_emit::with_module_id_global(once), gex_emit::with_module_id_global(once));
        assert_ne!(a.module_id, b.module_id);
        assert_eq!(a.wasm, b.wasm);
        let has = |w: &[u8]| w.windows(gex_emit::MODULE_ID_GLOBAL.len()).any(|x| x == gex_emit::MODULE_ID_GLOBAL.as_bytes());
        assert!(has(&a.wasm));
        let (c, d) = (once(), once());
        assert!(!c.covered.is_empty() && !has(&c.wasm));
        assert_ne!(c.wasm, d.wasm);
    }

    /// A long run of straight-line code (what a packed image's stub jumps
    /// into) lifts no more than the budget in one miss, not `max_blocks` full
    /// blocks; the next miss carries on.
    #[test]
    fn one_miss_lifts_at_most_its_code_budget() {
        let (base, len) = (0x10000usize, 4 << 20);
        let mut mem = vec![0u8; base + len];
        for w in mem[base..].chunks_mut(4) {
            w.copy_from_slice(&0xd503_201fu32.to_le_bytes()); // nop
        }
        let (_, stats) = translate_runtime(
            &mem,
            base as u64,
            &|a| a >= base as u64 && a < (base + len) as u64,
            &|_| false,
            PageTable::new(),
            BlockLimits::default(),
            2048,
            true,
            &Default::default(),
        );
        let bytes = stats.instructions * 4;
        assert!(bytes > 0 && bytes <= super::MISS_CODE_BUDGET + BlockLimits::default().max_insts * 4, "{bytes} bytes lifted");
        assert!(stats.blocks < 2048);
    }

    #[test]
    fn cgen_refuses_profiling() {
        assert!(super::cgen_conflict(true, true).is_some());
        assert!(super::cgen_conflict(true, false).is_none());
        assert!(super::cgen_conflict(false, true).is_none());
    }

    use super::*;
    use gex_ld::elf::Segment;

    /// Past the stored bytes, up to the mapped size, a fetch reads zeros:
    /// from inside the stored bytes (an instruction ending in a trimmed zero)
    /// and from wholly past them (where `bytes[off..]` once panicked,
    /// stopping a unit build on a Wine DLL).
    #[test]
    fn a_fetch_reads_zeros_past_the_stored_bytes() {
        let mut img = image_with(&[0x9090_9090], 0x1000);
        img.segments[0].mem_size = 0x100;
        img.code = vec![(0x1000, 0x1100)];
        let v = CodeView::new(&img);
        let mut buf = [0xaau8; 16];
        assert_eq!(v.fetch(0x1000, &mut buf), 16);
        assert_eq!(buf[..6], [0x90, 0x90, 0x90, 0x90, 0, 0]);
        let mut buf = [0xaau8; 16];
        assert_eq!(v.fetch(0x1080, &mut buf), 16);
        assert_eq!(buf, [0u8; 16]);
        let mut buf = [0xaau8; 16];
        assert_eq!(v.fetch(0x10f8, &mut buf), 8);
        assert_eq!(buf[..8], [0u8; 8]);
        assert_eq!(v.fetch(0x1100, &mut buf), 0);
    }

    fn image_with(code: &[u32], base: u64) -> Image {
        let mut bytes = Vec::new();
        for w in code {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        let len = bytes.len() as u64;
        Image {
            class32: false,
            soname: None,
            needed: Vec::new(),
            runpath: Vec::new(),
            bias: 0,
            entry: base,
            code: vec![(base, base + len)],
            segments: vec![Segment {
                vaddr: base,
                bytes,
                mem_size: len,
                readable: true,
                writable: false,
                executable: true,
            }],
            phdr_vaddr: base,
            phentsize: 56,
            phnum: 1,
            brk: base + len,
            text_range: (base, base + len),
            tls: None,
            symbols: Vec::new(),
            named_relocs: Vec::new(),
            relocs: Vec::new(),
            init_array: Vec::new(),
            dynsyms: Vec::new(),
            irelative: Vec::new(),
            pie: false,
            interp: None,
        }
    }

    /// An object too large for one module splits into several, and no dispatch
    /// page is split across two modules (which would have the second overwrite
    /// the first's slots with the miss stub).
    #[test]
    fn a_large_object_splits_on_page_boundaries() {
        // Eight pages of `ret`, every word a seed, so the blocks spread
        // across pages instead of collapsing into one.
        let base = 0x10000u64;
        let span = 8 * gex_abi::DISPATCH_PAGE;
        let mut memory = vec![0u8; (base + span) as usize];
        let mut a = base;
        while a < base + span {
            memory[a as usize..a as usize + 4].copy_from_slice(&0xd65f03c0u32.to_le_bytes());
            a += 4;
        }
        let seeds: Vec<u64> = (base..base + span).step_by(4).collect();
        let (blocks, _) = discover(
            &memory,
            base,
            &seeds,
            &|x| x >= base && x < base + span,
            &|_| false,
            BlockLimits::default(),
            usize::MAX,
            usize::MAX,
            &HashMap::new(),
        );
        let total = blocks.len();
        assert_eq!(total, seeds.len(), "every `ret` is its own block");

        // A budget that forces a split without dictating where it falls.
        let modules = emit_modules(blocks, base, PageTable::default(), false, false, total / 4);
        assert!(modules.len() > 1, "the budget should have forced a split");

        let mut seen_addrs = HashSet::new();
        let mut seen_pages = HashMap::new();
        for (i, m) in modules.iter().enumerate() {
            for (addr, _) in &m.exported_blocks {
                assert!(seen_addrs.insert(*addr), "{addr:#x} exported twice");
                let page = addr / gex_abi::DISPATCH_PAGE;
                let owner = seen_pages.entry(page).or_insert(i);
                assert_eq!(*owner, i, "page {page:#x} is split across two modules");
            }
        }
        assert_eq!(seen_addrs.len(), total, "a block went missing in the split");

        // Slot bases are handed out once, across all the modules together.
        let mut bases = HashSet::new();
        for m in &modules {
            for (_page, base) in &m.dir_entries {
                assert!(bases.insert(*base), "slot base {base} given out twice");
            }
        }
    }

    #[test]
    fn translates_a_trivial_program() {
        // mov x0, #0 ; mov x8, #94 ; svc #0
        let img = image_with(&[0xd2800000, 0xd2800bc8, 0xd4000001], 0x400000);
        let (m, stats) = translate_image(&img, &Options::default());
        assert!(stats.blocks >= 1);
        assert_eq!(&m.wasm[..4], b"\0asm");
        assert_eq!(m.text_base, 0x400000);
        // One page of text, so the table is the reserved miss span plus one
        // page's worth of slots.
        assert_eq!(m.dir_entries.len(), 1);
        assert_eq!(m.table_slots, gex_abi::MISS_SLOTS + gex_abi::SLOTS_PER_PAGE);
    }

    #[test]
    fn follows_direct_branches() {
        // b .+8 ; udf ; ret
        let img = image_with(&[0x14000002, 0x00000000, 0xd65f03c0], 0x400000);
        let (_, stats) = translate_image(&img, &Options::default());
        // The entry block and the branch target, but not the skipped word.
        assert_eq!(stats.blocks, 2);
    }

    #[test]
    fn a_code_pointer_in_data_becomes_an_entry_point() {
        // Two instructions of code, then a doubleword pointing at the second.
        let base = 0x400000u64;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0xd65f03c0u32.to_le_bytes()); // ret
        bytes.extend_from_slice(&0xd65f03c0u32.to_le_bytes()); // ret
        let code_len = bytes.len() as u64;

        let mut img = image_with(&[0xd65f03c0, 0xd65f03c0], base);
        img.segments.push(Segment {
            vaddr: base + 0x1000,
            bytes: (base + 4).to_le_bytes().to_vec(),
            mem_size: 8,
            readable: true,
            writable: true,
            executable: false,
        });
        let _ = code_len;

        let (_, stats) = translate_image(&img, &Options::default());
        assert_eq!(stats.blocks, 2, "the pointed-at address must be translated");
        assert_eq!(stats.discovered_from_data, 1);
    }

    #[test]
    fn undecodable_instructions_do_not_fail_the_build() {
        // A SIMD encoding the lifter does not handle yet.
        let img = image_with(&[0x4ea11c20], 0x400000);
        let (m, stats) = translate_image(&img, &Options::default());
        assert_eq!(stats.illegal_blocks, 1);
        assert!(!m.wasm.is_empty());
    }
}
pub use gex_emit::set_trace_blocks;
