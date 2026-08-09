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

pub use gex_emit::module::Linkage;
pub use gex_emit::module::Metadata;
/// Turn the per-block stack-pointer check on, for a host with no environment
/// to read `GEX_CHECK_SP` from. See `gex_emit::tier1::set_check_sp`.
pub use gex_emit::tier1::set_check_sp;
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
    /// Seed discovery from the entry point alone, ignoring the symbol table.
    ///
    /// Ahead-of-time discovery is thorough, and on a large program that is the
    /// wrong trade: it hands the host a module with tens of thousands of
    /// functions to compile, most of which the run will never enter. Runtime
    /// translation reaches everything anyway, so this makes the up-front
    /// module small and lets execution pull in the rest.
    pub lazy: bool,
    /// Declare the imported memory shared. Off only for a single-threaded
    /// build whose host cannot import a shared memory.
    pub shared_memory: bool,
    /// Whether this module declares the memory and dispatch table or imports
    /// them.
    ///
    /// One module in a program owns them and the rest join it. That is already
    /// how runtime translation works; a whole-closure build needs the same
    /// distinction ahead of time, because a program is an executable *and* the
    /// libraries it was placed alongside, and only one of them can declare the
    /// address space they share.
    pub linkage: Linkage,
    /// Where this module's page allocations begin in the shared dispatch
    /// table.
    ///
    /// Zero means "start where a lone module would". It is not zero for a
    /// closure: every object of one is translated separately but they all run
    /// in the same process against the same table, so each has to continue
    /// where the last stopped. Translating them all from the same start meant
    /// libc's blocks and the executable's blocks claimed the same slots, and
    /// the program branched into whichever had been installed last.
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
            shared_memory: true,
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
        // Sections first. A segment's `PF_X` covers everything read-only in
        // the same mapping, which for a shared library means the string
        // tables and relocations as well as the code -- and discovery that
        // trusts it disassembles "MESA" and "PIPE" into blocks that end in
        // SIGILL. The section flags say which bytes are really instructions.
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
/// Every `cmp`, `adds` and `subs` writes all four of NZCV, and the branch that
/// follows reads one or two. Within a single block the other three have to be
/// kept, because a successor might want them; across the whole graph they are
/// almost always dead, and dropping the write lets [`pass::dce`] delete the
/// arithmetic that produced it. On a vectorised inner loop that is a third of
/// the block.
///
/// The analysis is an ordinary backwards liveness fixpoint over the blocks in
/// hand. A successor that is not among them -- an indirect branch, a call into
/// a function translated later, an address outside this module -- is assumed
/// to read every flag, so an incomplete graph loses the optimisation rather
/// than producing wrong code.
/// The general-register analogue of this pass is not here, and that is a
/// result rather than an omission.
///
/// Twenty percent of emitted code is guest registers going out to the context
/// at a block boundary and coming back at the next, so removing the writes
/// that no successor reads looks like the obvious whole-object optimisation:
/// it needs every successor in hand, which a static recompilation has and a
/// demand-driven translation of a few thousand blocks does not.
///
/// Built and measured: it removes 44 statements out of 27,614 guest
/// instructions. Nothing. The reason is that the guest was compiled by GCC,
/// which allocated its registers already -- a value written near the end of a
/// basic block is written because something downstream reads it, and dead
/// stores across a boundary essentially do not occur in optimised code.
///
/// A return had to be modelled before the analysis found even that much:
/// treating `ret` as opaque floods every function body with live registers
/// from its own exit, and the pass removes exactly zero. Assuming the
/// procedure call standard there -- x9 to x18 are caller-saved and dead at a
/// return -- cut the opaque blocks from 1514 to 1096 and changed the number
/// of statements removed not at all. So the assumption buys nothing either,
/// which is worth knowing before anyone trades soundness for it.
///
/// The traffic is not dead writes. It is live values that a block boundary
/// forces out of locals, and the only things that help are the ones that keep
/// them in locals across the boundary: regions, and passing registers as
/// block parameters. Both already exist.
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

    // Successors as block indices, in one flat array rather than a `Vec` per
    // block.
    //
    // The fixpoint below walks every block's successors on every round, and
    // this used to build a fresh `Vec<u64>` and hash each address back to an
    // index each time it did. On a 410,000-block exhaustive translation that
    // was millions of allocations and tens of millions of hash lookups, and
    // it made flag liveness the largest phase in the whole pipeline --
    // larger than lifting and larger than code generation. Resolving the
    // edges once turns the inner loop into two array reads.
    //
    // A call's `call_return` is a successor here even though it is not a
    // control-flow edge out of the block: a call's flags are live if either
    // the callee reads them or the code it returns to does. Without it a
    // hand-written `cmp; bl f; b.eq` -- legal whenever `f` preserves the
    // flags, which the ABI permits though no compiler relies on it -- would
    // lose its compare.
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
///
/// Static recompilation is a pipeline of a few large passes and the balance
/// between them is not obvious -- and it moves. Lifting was the whole cost
/// until it was spread over the cores, and then wasm encoding was. Without a
/// breakdown the next round of that is guesswork.
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
    let mut stats = Stats::default();

    // Collect entry points. A BTreeSet keeps translation order deterministic,
    // which keeps the emitted module byte-identical across runs and therefore
    // usable as a content-addressed cache key (PLAN §10).
    let mut worklist: BTreeSet<u64> = BTreeSet::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut collected: Vec<Block> = Vec::new();

    // A shared library has no entry point: its `e_entry` is zero, so
    // `image.entry` is just the load bias and points at the ELF header. gex
    // seeded it anyway and disassembled "\x7fELF" as an instruction, which is
    // one block per library that could only ever raise SIGILL.
    //
    // Executables always have one, and it is never at offset zero, so
    // comparing against the bias tells the two apart.
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
                // Skip addresses that do not start a decodable instruction.
                // Executable segments are full of alignment padding and
                // literal pools; a block there could only ever end in
                // SIGILL, and leaving the slot on the miss stub reports the
                // same thing without emitting a wasm function for it. This
                // is what keeps exhaustive mode affordable — on a static
                // musl binary it removes about three quarters of the slots.
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
    // an address-ordered worklist does not. In `--exhaustive` mode the first
    // level is every decodable address in the text, which is the whole job:
    // the run that translates all of Debian used to spend it on one core
    // while Cranelift waited for it with twenty-three idle.
    //
    // Each level is sorted, so the block order -- and with it the module's
    // hash, which the compiled-module cache is keyed on -- is the same on
    // every run regardless of how the threads were scheduled.
    let mut frontier: Vec<u64> = worklist.iter().copied().collect();
    seen.extend(frontier.iter().copied());
    let show_illegal = std::env::var_os("GEX_SHOW_ILLEGAL").is_some();

    while !frontier.is_empty() {
        let lifted = gex_emit::par::map(&frontier, &|&addr| {
            let mut block = lift_block(|pc| view.fetch(pc), addr, opts.limits);
            pass::optimize(&mut block);
            block
        });

        let mut next: Vec<u64> = Vec::new();
        for block in lifted {
            stats.blocks += 1;
            stats.instructions += ((block.end - block.start) / 4) as usize;
            if matches!(block.term, Term::Illegal { .. }) {
                stats.illegal_blocks += 1;
                // Which instruction, so the gap can be closed rather than
                // counted. The lifter reaching one of these is a hole in the
                // static recompilation: the block is emitted, it is
                // dispatched to, and it raises SIGILL.
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

    phases.mark("lifting");

    // Back into address order, which is what the worklist used to produce.
    // Emission order fixes the function indices and therefore the module's
    // bytes, which the compiled-module cache is keyed on; it also decides
    // which blocks tier-1 considers together when it folds regions. Neither
    // should change because the lifting was spread over more cores.
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
/// empty dispatch table.
///
/// Every branch it takes misses, so the runtime translator behind it produces
/// the whole program. That makes it usable with *any* guest, which is the
/// point: a web page cannot run the ahead-of-time compiler, so a page that
/// lets a visitor supply their own binary needs one module it can ship in
/// advance and use for all of them. The dispatch directory is indexed by
/// guest page across the whole address space, so nothing in here depends on
/// where the program it will end up running is linked.
///
/// The trade is the one the ahead-of-time path exists to avoid: the first
/// execution of every block goes through a miss, a lift and a compile.
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

/// Translate code that is not in the image, reading it from live guest
/// memory (PLAN §6.2).
///
/// This is the completeness guarantee: an address with no translation gets
/// one on demand, whether it was missed by static discovery, produced by a
/// guest JIT, or brought in by a library loaded after startup. Discovery
/// still runs recursively from the entry point, so the first miss inside a
/// function usually translates the whole function and the next branch in it
/// costs nothing.
///
/// `known` names the addresses that already have translations, so the work
/// list does not redo them.
/// Statically recompile a whole mapped object: every decodable address in
/// `[lo, hi)`, not just what one entry point reaches.
///
/// This is the static recompiler applied to shared libraries. Only the
/// executable is translated ahead of time from its ELF, because only the
/// executable is loaded by gex -- the interpreter maps everything else itself,
/// at run time, from guest code. So for a dynamically linked program almost
/// nothing was statically recompiled: the executable is a stub and ld.so,
/// libc and the rest all arrived one dispatch miss at a time.
///
/// `seeds` says where to start; empty means every decodable address in the
/// range. Empty is right for an object with no symbols left and wrong for one
/// that has them -- a shipped library's `.dynsym` names a few thousand entry
/// points, and scanning instead finds two hundred thousand, most of them the
/// middle of a function that a block starting earlier already covers.
///
/// Seeding from the object rather than from one entry point is what makes
/// it static. Discovery still runs -- a block's successors are followed, and
/// a PLT's GOT load is folded so its indirect branch becomes a known target --
/// but it no longer depends on which address the guest happened to reach
/// first, which is what made the demand-driven output differ between runs.
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

/// Announce where a whole-object lift has got to, when asked.
///
/// Set `GEX_XLATE_STAGES=1` to get a line per stage on stderr. This exists
/// because the largest object in the graphics rootfs -- Mesa's software
/// rasteriser, 471,205 blocks -- lifts to a peak working set of 2109 MiB,
/// against a shim heap of 2039 MiB, and a total at the end says nothing about
/// which stage holds the peak. Off by default and one `getenv` when it is on,
/// so it costs nothing in the runtime that cannot afford it.
///
/// The resident figure is read from `/proc` and is therefore native-only; in
/// the shim it prints the count alone, which is still the useful half.
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
/// wasmtime refuses a module with more than a million functions, and Mesa and
/// libz3 both have more blocks than that -- `--recompile-objects` failed on
/// them and quietly fell back to translating at run time, which is the one
/// outcome static recompilation exists to avoid. So an object that does not
/// fit in one module is emitted as several.
///
/// The number is well under the ceiling on purpose. Splitting is not free:
/// `tier1` emits a direct tail call for an edge whose target is in the same
/// module and a table dispatch for one that is not, so every split turns some
/// direct calls into indirect ones. Fewer, fuller modules are faster, and the
/// chunking below keeps consecutive pages together so that the edges that do
/// cross a boundary are the rare long-range ones rather than a function's own
/// fallthrough.
pub const MAX_FUNCS_PER_MODULE: usize = 500_000;

/// Encode discovered blocks as one module, or as several if there are too
/// many for one.
///
/// The split is by dispatch page, never within one: a page's slots are
/// allocated as a unit and its element segment is written by whichever module
/// claimed it, so two modules sharing a page would each hold half its slots
/// and the second to be installed would overwrite the first's with the stub.
/// Whole pages, in address order, keeps each module's slot range its own.
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

    // Group by page first, then walk the pages in address order filling one
    // chunk at a time. A page is never split, so a chunk may overshoot the
    // budget by up to one page's worth of blocks -- 1024 at the very most,
    // against a budget of half a million.
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

    // Breadth-first rather than the address-ordered worklist this used to be,
    // because a level can be lifted in parallel and an arbitrary worklist
    // cannot. Lifting is the serial half of static recompilation: Cranelift
    // uses every core, and until this changed the pass that feeds it used
    // one, so a large object spent minutes at 4% of the machine.
    //
    // The seeds are the first level. Each level is sorted, so the output is
    // the same on every run -- the compiled-module cache is content-addressed
    // and a translation that varied between runs would never hit it.
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
                // Deliberately *not* skipping an address that falls inside a
                // block already lifted.
                //
                // Doing so is a large saving -- it takes libc from 200,000
                // blocks to a few tens of thousands -- and it is wrong. An
                // interior address is still a legal target for an indirect
                // branch, and dispatch needs a block that starts exactly
                // there; entering the containing block would re-run the
                // instructions before it. With runtime translation behind it
                // that is only a miss. Without, it is where the program dies,
                // which is what `ld.so` did at 0x2801ae40 the first time this
                // was tried.
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

    /// An object too large for one module comes back as several, and the
    /// split respects the one boundary that matters.
    ///
    /// A dispatch page's slots are allocated as a unit and written by
    /// whichever module claimed the page. Two modules holding blocks from the
    /// same page would each write that page's whole element segment, and the
    /// second installed would overwrite the first's entries with the miss
    /// stub -- every block in the first module would then be silently
    /// unreachable, which is the kind of failure that shows up as a
    /// mysterious dispatch miss a long way from here.
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
