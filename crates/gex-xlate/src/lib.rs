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

use gex_emit::{ModuleBuilder, TranslatedModule};
use gex_ir::{pass, Term};
use gex_ld::Image;
use gex_lift::{lift_block, BlockLimits};
use std::collections::{BTreeSet, HashSet};

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
    /// This is the completeness setting. Recursive descent plus a pointer
    /// scan still misses jump tables, which store scaled *relative* offsets
    /// that look nothing like addresses — `pop_arg`'s switch in musl is a
    /// perfect example. PLAN §5.5 gives every aligned text address a dispatch
    /// slot precisely so this is always possible; with no runtime translator
    /// yet (M3), filling them all ahead of time is what makes "any binary"
    /// true today. It costs translation time and module size, and becomes a
    /// pure optimisation to turn off once gex-jit can fill slots on demand.
    pub exhaustive: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            limits: BlockLimits::default(),
            extra_entries: Vec::new(),
            scan_pointers: true,
            exhaustive: true,
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
        if pc % 4 != 0 {
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

/// Translate a whole loaded image to a wasm module.
pub fn translate_image(image: &Image, opts: &Options) -> (TranslatedModule, Stats) {
    let view = CodeView::new(image);
    let (text_lo, text_hi) = image.text_range;
    let mut builder = ModuleBuilder::new(text_lo, text_hi - text_lo);
    let mut stats = Stats::default();

    // Collect entry points. A BTreeSet keeps translation order deterministic,
    // which keeps the emitted module byte-identical across runs and therefore
    // usable as a content-addressed cache key (PLAN §10).
    let mut worklist: BTreeSet<u64> = BTreeSet::new();
    let mut seen: HashSet<u64> = HashSet::new();

    worklist.insert(image.entry);
    for sym in &image.symbols {
        if view.is_executable(sym.value) && sym.value % 4 == 0 {
            worklist.insert(sym.value);
        }
    }
    for &a in &opts.extra_entries {
        if view.is_executable(a) {
            worklist.insert(a);
        }
    }
    if opts.scan_pointers {
        let before = worklist.len();
        for seg in &image.segments {
            for (i, chunk) in seg.bytes.chunks_exact(8).enumerate() {
                // Only naturally aligned words can be pointers.
                if (seg.vaddr as usize + i * 8) % 8 != 0 {
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
                    Some(gex_isa::Inst::Unknown { .. }) | Some(gex_isa::Inst::Undefined { .. })
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
        builder.reserve(addr);
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
        assert_eq!(m.text_slots, 3);
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
