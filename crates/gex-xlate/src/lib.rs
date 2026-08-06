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

use gex_emit::module::Linkage;
pub use gex_emit::module::Metadata;
pub use gex_emit::PageTable;
use gex_emit::{ModuleBuilder, TranslatedModule};
use gex_ir::{pass, Block, Term};
use gex_ld::Image;
use gex_lift::{lift_block, BlockLimits};
use std::collections::{BTreeSet, HashMap, HashSet};

/// Knobs for a translation run.
#[derive(Clone, Debug)]
pub struct Options {
    pub limits: BlockLimits,
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
}

impl Default for Options {
    fn default() -> Self {
        Options {
            limits: BlockLimits::default(),
            extra_entries: Vec::new(),
            scan_pointers: true,
            // Off by default now that runtime translation covers the gaps;
            // still available for a build that must not translate later.
            exhaustive: false,
            lazy: false,
            shared_memory: true,
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
    // A block that can leave the graph keeps everything live, and so does one
    // with a successor nobody translated.
    //
    // A call's flags are live if either the callee reads them or the code it
    // returns to does, so `call_return` counts as a successor here even though
    // it is not a control-flow edge out of this block. Without it a
    // hand-written `cmp; bl f; b.eq` -- legal whenever `f` preserves the
    // flags, which the ABI permits though no compiler relies on it -- would
    // lose its compare.
    let succs = |b: &Block| -> Vec<u64> { b.successors().chain(b.call_return).collect() };
    let opaque: Vec<bool> = blocks
        .iter()
        .map(|b| pass::escapes(b) || succs(b).iter().any(|s| !index.contains_key(s)))
        .collect();

    let mut live_in = vec![0u8; blocks.len()];
    loop {
        let mut changed = false;
        for (i, b) in blocks.iter().enumerate() {
            let live_out = if opaque[i] {
                pass::ALL_FLAGS
            } else {
                succs(b)
                    .iter()
                    .map(|s| live_in[index[s]])
                    .fold(0, |a, x| a | x)
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

    let mut removed = 0;
    for (i, b) in blocks.iter_mut().enumerate() {
        if opaque[i] {
            continue;
        }
        let live_out = succs(b)
            .iter()
            .map(|s| live_in[index[s]])
            .fold(0, |a, x| a | x);
        let dead = pass::flags_of_mask(pass::ALL_FLAGS & !live_out);
        if dead.is_empty() {
            continue;
        }
        let before = b.stmts.len();
        pass::dead_store_elim_with(b, &dead);
        pass::dce(b);
        removed += before - b.stmts.len();
    }
    removed
}

pub fn translate_image(image: &Image, opts: &Options) -> (TranslatedModule, Stats) {
    let view = CodeView::new(image);
    let (text_lo, text_hi) = image.text_range;
    let mut builder =
        ModuleBuilder::with_linkage(text_lo, text_hi - text_lo, Linkage::Own, opts.shared_memory);
    let mut stats = Stats::default();

    // Collect entry points. A BTreeSet keeps translation order deterministic,
    // which keeps the emitted module byte-identical across runs and therefore
    // usable as a content-addressed cache key (PLAN §10).
    let mut worklist: BTreeSet<u64> = BTreeSet::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut collected: Vec<Block> = Vec::new();

    worklist.insert(image.entry);
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

    while let Some(&addr) = worklist.iter().next() {
        worklist.remove(&addr);
        if !seen.insert(addr) {
            continue;
        }
        let mut block = lift_block(|pc| view.fetch(pc), addr, opts.limits);
        pass::optimize(&mut block);

        stats.blocks += 1;
        stats.instructions += ((block.end - block.start) / 4) as usize;
        if matches!(block.term, Term::Illegal { .. }) {
            stats.illegal_blocks += 1;
        }

        // Both the successors and the return site of any call in this block
        // are entry points; a callee's `ret` branches straight to the latter.
        for succ in block.successors().chain(block.call_return) {
            if !seen.contains(&succ) && view.is_executable(succ) {
                worklist.insert(succ);
            }
        }
        collected.push(block);
    }

    // Flag liveness needs the whole graph, so it runs once discovery has
    // finished rather than per block. Emission order is unchanged, which
    // keeps function indices -- and the module's hash -- deterministic.
    stats.dead_flag_stmts = eliminate_dead_flags(&mut collected);
    for block in collected {
        builder.reserve(block.start);
        builder.add_block(block);
    }

    let module = builder.finish();
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
pub fn translate_runtime(
    memory: &[u8],
    entry: u64,
    executable: &dyn Fn(u64) -> bool,
    known: &dyn Fn(u64) -> bool,
    page_table: PageTable,
    limits: BlockLimits,
    max_blocks: usize,
    shared_memory: bool,
) -> (TranslatedModule, Stats) {
    let fetch = |pc: u64| -> Option<u32> {
        if pc % 4 != 0 || !executable(pc) {
            return None;
        }
        let off = pc as usize;
        if off + 4 > memory.len() {
            return None;
        }
        Some(u32::from_le_bytes(memory[off..off + 4].try_into().unwrap()))
    };

    let mut builder =
        ModuleBuilder::with_pages(entry, 0, Linkage::Shared, page_table, shared_memory);
    let mut stats = Stats::default();
    let mut worklist: BTreeSet<u64> = BTreeSet::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut collected: Vec<Block> = Vec::new();
    worklist.insert(entry);

    while let Some(&addr) = worklist.iter().next() {
        worklist.remove(&addr);
        if !seen.insert(addr) || stats.blocks >= max_blocks {
            continue;
        }
        // Never re-translate something the table already answers for; the
        // existing entry may be a tier-2 region.
        if addr != entry && known(addr) {
            continue;
        }
        let mut block = lift_block(fetch, addr, limits);
        // Fold through memory before the ordinary passes, so a PLT's GOT load
        // becomes a constant and its `br` becomes a discoverable target.
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
            if fetch(succ).is_some() && !seen.contains(&succ) && !known(succ) {
                worklist.insert(succ);
            }
        }
        collected.push(block);
    }

    stats.dead_flag_stmts = eliminate_dead_flags(&mut collected);
    for block in collected {
        builder.add_block(block);
    }

    let module = builder.finish();
    stats.wasm_bytes = module.wasm.len();
    (module, stats)
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
            bias: 0,
            entry: base,
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
            pie: false,
            interp: None,
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
