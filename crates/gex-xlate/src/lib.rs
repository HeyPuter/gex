//! `gex-xlate` — the translator facade.
//!
//! Ties the decoder, lifter and backend together and drives *code discovery*.
//!
//! Discovery is a best-effort search, never a correctness requirement
//! (PLAN §6.1): everything it misses ends up in an untranslated dispatch slot
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
pub use gex_emit::module::Metadata;
/// Turn the per-block stack-pointer check on, for a host with no environment
/// to read `GEX_CHECK_SP` from. See `gex_emit::tier1::set_check_sp`.
pub use gex_emit::tier1::set_check_sp;
pub use gex_emit::tier1::set_copy_loop;
pub use gex_emit::tier1::set_coherent_loads;
pub use gex_emit::PageTable;
pub use gex_emit::TranslatedModule;
use gex_emit::ModuleBuilder;
use gex_ir::{pass, Block, Term};
use gex_ld::Image;
use gex_lift::{lift_block, BlockLimits};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

pub mod intrinsic;
pub use intrinsic::Intrinsic;

/// Knobs for a translation run.
#[derive(Clone, Debug)]
pub struct Options {
    pub limits: BlockLimits,
    /// Guest addresses that are library functions with a wasm instruction
    /// behind them, so the translator emits that instead of lifting the
    /// function's own code. See [`intrinsic`].
    pub intrinsics: HashMap<u64, Intrinsic>,
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
}

impl Default for Options {
    fn default() -> Self {
        Options {
            limits: BlockLimits::default(),
            intrinsics: HashMap::new(),
            extra_entries: Vec::new(),
            scan_pointers: true,
            // Off by default now that runtime translation covers the gaps;
            // still available for a build that must not translate later.
            exhaustive: false,
            lazy: false,
            complete_pages: false,
            shared_memory: true,
            demand_fill: false,
            linkage: Linkage::Own,
            first_slot: 0,
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

    /// Fetch the instruction word at `pc`, if it lies in executable memory.
    pub fn fetch(&self, pc: u64) -> Option<u32> {
        if !pc.is_multiple_of(4) {
            return None;
        }
        for seg in &self.image.segments {
            if !seg.executable {
                continue;
            }
            if pc >= seg.vaddr && pc + 4 <= seg.vaddr + seg.bytes.len() as u64 {
                let off = (pc - seg.vaddr) as usize;
                return Some(u32::from_le_bytes(
                    seg.bytes[off..off + 4].try_into().unwrap(),
                ));
            }
        }
        None
    }

    fn is_executable(&self, addr: u64) -> bool {
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
    let effects: Vec<(u8, u8)> = blocks.iter().map(pass::flag_effects).collect();

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

    let mut live_in = vec![0u8; blocks.len()];
    loop {
        let mut changed = false;
        for i in 0..blocks.len() {
            let live_out = if opaque[i] {
                pass::ALL_FLAGS
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
    let dead: Vec<u8> = (0..blocks.len())
        .map(|i| {
            if opaque[i] {
                return 0;
            }
            let live_out = succs_of(i)
                .iter()
                .fold(0, |a, &s| a | live_in[s as usize]);
            pass::ALL_FLAGS & !live_out
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

pub fn translate_image(image: &Image, opts: &Options) -> (TranslatedModule, Stats) {
    let mut phases = Phases::start();
    let view = CodeView::new(image);
    let (text_lo, text_hi) = image.text_range;
    let mut builder = if opts.first_slot == 0 {
        ModuleBuilder::with_linkage(text_lo, text_hi - text_lo, opts.linkage, opts.shared_memory)
    } else {
        ModuleBuilder::with_pages(
            text_lo,
            text_hi - text_lo,
            opts.linkage,
            gex_emit::module::PageTable::from_entries(&[], opts.first_slot),
            opts.shared_memory,
        )
    };
    builder.set_demand_fill(opts.demand_fill);
    let mut stats = Stats::default();

    // Collect entry points. A BTreeSet keeps translation order deterministic,
    // which keeps the emitted module byte-identical across runs and therefore
    // usable as a content-addressed cache key (PLAN §10).
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
            if view.is_executable(sym.value) && sym.value % 4 == 0 {
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
                if v % 4 == 0 && view.is_executable(v) {
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
            let mut page: [Option<u64>; 32] = [None; 32];
            let end = seg.vaddr + seg.bytes.len() as u64;
            let mut a = seg.vaddr;
            while a + 4 <= end {
                if !view.is_executable(a) {
                    page = [None; 32];
                    a += 4;
                    continue;
                }
                let off = (a - seg.vaddr) as usize;
                let raw = u32::from_le_bytes(seg.bytes[off..off + 4].try_into().unwrap());
                match gex_isa::decode(raw, a) {
                    gex_isa::Inst::Adrp { rd: gex_isa::RegSel::Gp(r), target } => {
                        page[r as usize] = Some(target);
                    }
                    gex_isa::Inst::Adr { target, .. } => {
                        if target % 4 == 0 && view.is_executable(target) {
                            worklist.insert(target);
                        }
                    }
                    gex_isa::Inst::AddSubImm {
                        op: gex_isa::AddOp::Add,
                        rd: gex_isa::RegSel::Gp(rd),
                        rn: gex_isa::RegSel::Gp(rn),
                        imm,
                        ..
                    } => {
                        if let Some(p) = page[rn as usize] {
                            let target = p.wrapping_add(imm);
                            if target % 4 == 0 && view.is_executable(target) {
                                worklist.insert(target);
                            }
                        }
                        page[rd as usize] = None;
                    }
                    _ => {}
                }
                a += 4;
            }
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
            let mut a = seg.vaddr;
            while a + 4 <= end {
                // Skip addresses that are not really instructions: an
                // executable segment is full of padding and literal pools, and
                // for a shared library also holds string tables, relocations
                // and `.rodata`. `is_executable` uses the section flags to say
                // which bytes are code. Skipping the rest keeps exhaustive mode
                // affordable (three quarters of the slots on a static musl
                // binary) and avoids folding a `.rodata` block over a real
                // entry point's slot -- the bug in #46 that broke `apt` on libc.
                if !view.is_executable(a) {
                    a += 4;
                    continue;
                }
                match view.fetch(a).map(|raw| gex_isa::decode(raw, a)) {
                    Some(gex_isa::Inst::Unknown { .. })
                    | Some(gex_isa::Inst::Undefined { .. })
                    | None => {}
                    Some(_) => {
                        worklist.insert(a);
                    }
                }
                a += 4;
            }
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

    while !frontier.is_empty() {
        let lifted = gex_emit::par::map(&frontier, &|&addr| {
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
            let mut block = lift_block(
                |pc| if view.is_executable(pc) { view.fetch(pc) } else { None },
                addr,
                opts.limits,
            );
            pass::optimize(&mut block);
            block
        });

        let mut next: Vec<u64> = Vec::new();
        for block in lifted {
            stats.blocks += 1;
            stats.instructions += ((block.end - block.start) / 4) as usize;
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
            // the latter.
            for succ in block.successors().chain(block.call_return) {
                if !seen.contains(&succ) && view.is_executable(succ) {
                    seen.insert(succ);
                    next.push(succ);
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
        for &pg in &touched {
            let mut a = pg;
            while a < pg + 0x1000 {
                if !seen.contains(&a) && view.is_executable(a) {
                    if let Some(raw) = view.fetch(a) {
                        if !matches!(
                            gex_isa::decode(raw, a),
                            gex_isa::Inst::Unknown { .. } | gex_isa::Inst::Undefined { .. }
                        ) {
                            seen.insert(a);
                            fill.push(a);
                        }
                    }
                }
                a += 4;
            }
        }
        fill.sort_unstable();
        let lifted = gex_emit::par::map(&fill, &|&addr| {
            if let Some(&k) = opts.intrinsics.get(&addr) {
                return intrinsic::block(addr, k);
            }
            let mut block = lift_block(
                |pc| if view.is_executable(pc) { view.fetch(pc) } else { None },
                addr,
                opts.limits,
            );
            pass::optimize(&mut block);
            block
        });
        for block in lifted {
            stats.blocks += 1;
            stats.instructions += ((block.end - block.start) / 4) as usize;
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
    for block in collected {
        builder.reserve(block.start);
        builder.add_block(block);
    }
    phases.mark("reserving");

    let module = builder.finish();
    phases.mark("codegen and encoding");
    stats.wasm_bytes = module.wasm.len();
    (module, stats)
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
        let mut a = lo;
        while a + 4 <= hi {
            if executable(a) {
                let off = a as usize;
                if off + 4 <= memory.len() {
                    let raw = u32::from_le_bytes(memory[off..off + 4].try_into().unwrap());
                    if !matches!(
                        gex_isa::decode(raw, a),
                        gex_isa::Inst::Unknown { .. } | gex_isa::Inst::Undefined { .. }
                    ) {
                        scanned.push(a);
                    }
                }
            }
            a += 4;
        }
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
        intrinsics,
    );
    stage("discovered", blocks.len());
    let modules = emit_modules(blocks, lo, page_table, shared_memory, MAX_FUNCS_PER_MODULE);
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
            pages.clone(),
            shared_memory,
        );
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
    let (blocks, mut stats) = discover(
        memory, entry, seeds, executable, known, limits, max_blocks, intrinsics,
    );
    let mut modules = emit_modules(blocks, entry, page_table, shared_memory, usize::MAX);
    let module = modules.pop().expect("an unbounded budget emits one module");
    stats.wasm_bytes = module.wasm.len();
    (module, stats)
}

/// Lift every block reachable from the seeds.
///
/// Separate from encoding because one object's blocks may not fit in one
/// module -- see [`emit_modules`].
#[allow(clippy::too_many_arguments)]
fn discover(
    memory: &[u8],
    entry: u64,
    seeds: &[u64],
    executable: &(dyn Fn(u64) -> bool + Sync),
    known: &(dyn Fn(u64) -> bool + Sync),
    limits: BlockLimits,
    max_blocks: usize,
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

    while !frontier.is_empty() && stats.blocks < max_blocks {
        if frontier.len() + stats.blocks > max_blocks {
            frontier.truncate(max_blocks - stats.blocks);
        }
        let lifted = gex_emit::par::map(&frontier, &|&a| {
            match intrinsics.get(&a) {
                // Not lifted at all, so discovery never walks into the
                // function's body and the module never carries it.
                Some(&k) => intrinsic::block(a, k),
                None => lift_one(a, memory, executable, limits),
            }
        });

        let mut next: Vec<u64> = Vec::new();
        for block in lifted {
            stats.blocks += 1;
            stats.instructions += ((block.end - block.start) / 4) as usize;
            if matches!(block.term, Term::Illegal { .. }) {
                stats.illegal_blocks += 1;
            }

            // A computed branch whose target folded to a constant is worth
            // following: that is every cross-object call in a dynamic program.
            let mut targets: Vec<u64> = block.successors().chain(block.call_return).collect();
            if let Term::Indirect { target, .. } = block.term {
                if let Some(t) = pass::const_value(&block, target) {
                    stats.discovered_from_data += 1;
                    targets.push(t);
                }
            }
            for succ in targets {
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
    pc % 4 == 0 && executable(pc) && pc as usize + 4 <= memory.len()
}

/// Lift one block, with the folding that makes a PLT's indirect branch a
/// discoverable target.
fn lift_one(
    addr: u64,
    memory: &[u8],
    executable: &(dyn Fn(u64) -> bool + Sync),
    limits: BlockLimits,
) -> Block {
    let fetch = |pc: u64| -> Option<u32> {
        if !fetchable(memory, executable, pc) {
            return None;
        }
        let off = pc as usize;
        Some(u32::from_le_bytes(memory[off..off + 4].try_into().unwrap()))
    };
    let mut block = lift_block(fetch, addr, limits);
    // Folded through memory before the ordinary passes, so a PLT's GOT load
    // becomes a constant and its `br` becomes a target discovery can follow.
    pass::const_fold_with_memory(&mut block, &|a, bytes| {
        let off = a as usize;
        if off + bytes as usize > memory.len() {
            return None;
        }
        Some(match bytes {
            1 => memory[off] as u64,
            2 => u16::from_le_bytes(memory[off..off + 2].try_into().unwrap()) as u64,
            4 => u32::from_le_bytes(memory[off..off + 4].try_into().unwrap()) as u64,
            _ => u64::from_le_bytes(memory[off..off + 8].try_into().unwrap()),
        })
    });
    pass::optimize(&mut block);
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use gex_ld::elf::Segment;

    fn image_with(code: &[u32], base: u64) -> Image {
        let mut bytes = Vec::new();
        for w in code {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        let len = bytes.len() as u64;
        Image {
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
            &HashMap::new(),
        );
        let total = blocks.len();
        assert_eq!(total, seeds.len(), "every `ret` is its own block");

        // A budget that forces a split without dictating where it falls.
        let modules = emit_modules(blocks, base, PageTable::default(), false, total / 4);
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
