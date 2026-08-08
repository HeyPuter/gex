//! Run the *runtime* translator on one address, natively.
//!
//!   cargo run --release -p gex-xlate --example translate-at -- <elf> <addr> [max-blocks]
//!
//! `gexc build` exercises `translate_image`, which walks a whole binary with
//! every symbol and relocation in hand. The browser never does that: there is
//! no ahead-of-time step in a page, so every block arrives through
//! `translate_runtime`, which starts from one address in live memory and
//! follows only what it can reach. The two take different paths through the
//! lifter and they fail differently.
//!
//! That difference cost most of a day. Xvfb builds statically in three
//! seconds -- 367,171 blocks, all of it -- and in the browser the translator
//! went into the block at `Xvfb+0x1826f8` and never came out, with the whole
//! run silent behind it because nothing else can run while a translation is
//! in progress. A forty-second browser run to reproduce, and no way to look
//! inside it.
//!
//! So: the same entry point, on the host, where it takes a second and a
//! profiler works. The memory image is approximated by mapping the ELF's
//! PT_LOADs at `PIE_BASE`, which is what the loader does for the pages that
//! matter here -- code and rodata. Nothing is relocated, because lifting
//! reads instructions rather than pointers.
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: translate-at <elf> <addr> [max-blocks]");
        std::process::exit(2);
    }
    let elf = std::fs::read(&args[1]).expect("read the elf");
    let addr = if args[2] == "object" { 0 } else { u64::from_str_radix(args[2].trim_start_matches("0x"), 16).expect("hex address") };
    let max_blocks: usize = args.get(3).map_or(2048, |s| s.parse().expect("a number"));

    // Just enough of the address space to hold the image. The guest's is two
    // gigabytes; this only needs to reach the end of the last segment.
    let base = gex_abi::PIE_BASE;
    let mut segments: Vec<(u64, u64, usize, usize)> = Vec::new();
    let phoff = u64::from_le_bytes(elf[0x20..0x28].try_into().unwrap()) as usize;
    let phentsize = u16::from_le_bytes(elf[0x36..0x38].try_into().unwrap()) as usize;
    let phnum = u16::from_le_bytes(elf[0x38..0x3a].try_into().unwrap()) as usize;
    let mut top = 0u64;
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        let kind = u32::from_le_bytes(elf[p..p + 4].try_into().unwrap());
        if kind != 1 {
            continue; // PT_LOAD only
        }
        let flags = u32::from_le_bytes(elf[p + 4..p + 8].try_into().unwrap());
        let offset = u64::from_le_bytes(elf[p + 8..p + 16].try_into().unwrap()) as usize;
        let vaddr = u64::from_le_bytes(elf[p + 16..p + 24].try_into().unwrap());
        let filesz = u64::from_le_bytes(elf[p + 32..p + 40].try_into().unwrap()) as usize;
        let memsz = u64::from_le_bytes(elf[p + 40..p + 48].try_into().unwrap());
        segments.push((base + vaddr, base + vaddr + memsz, offset, filesz));
        top = top.max(base + vaddr + memsz);
        if flags & 1 != 0 {
            eprintln!(
                "  executable segment {:#x}..{:#x}",
                base + vaddr,
                base + vaddr + memsz
            );
        }
    }
    let mut memory = vec![0u8; (top + 0x10000) as usize];
    for (start, _, offset, filesz) in &segments {
        memory[*start as usize..*start as usize + filesz]
            .copy_from_slice(&elf[*offset..*offset + *filesz]);
    }
    // Which addresses hold instructions. The lifter asks before every fetch,
    // and answering yes for everything is how a run ends up disassembling
    // .rodata.
    let exec: Vec<(u64, u64)> = {
        let mut v = Vec::new();
        for i in 0..phnum {
            let p = phoff + i * phentsize;
            if u32::from_le_bytes(elf[p..p + 4].try_into().unwrap()) != 1 {
                continue;
            }
            if u32::from_le_bytes(elf[p + 4..p + 8].try_into().unwrap()) & 1 == 0 {
                continue;
            }
            let vaddr = u64::from_le_bytes(elf[p + 16..p + 24].try_into().unwrap());
            let memsz = u64::from_le_bytes(elf[p + 40..p + 48].try_into().unwrap());
            v.push((base + vaddr, base + vaddr + memsz));
        }
        v
    };

    // `object` recompiles the whole executable segment the way the runtime
    // does when the guest maps a library, rather than following one address.
    // That is the path a browser now takes, and the one that breaks on
    // Mesa's software rasteriser: 471,205 blocks, which is the first object
    // in the graphics rootfs big enough to need more than one wasm module.
    if args[2] == "object" {
        let (lo, hi) = exec.first().copied().unwrap_or((base, base));
        let mut seeds: Vec<u64> = Vec::new();
        let began = std::time::Instant::now();
        let (modules, stats) = gex_xlate::translate_object(
            &memory,
            lo,
            hi,
            &mut seeds,
            &|a| exec.iter().any(|(s, e)| a >= *s && a + 4 <= *e),
            &|_| false,
            gex_xlate::PageTable::default(),
            Default::default(),
            max_blocks,
            false,
        );
        println!(
            "{} blocks in {} module(s), {:.2}s",
            stats.blocks,
            modules.len(),
            began.elapsed().as_secs_f64()
        );
        for (i, m) in modules.iter().enumerate() {
            println!(
                "  module {i}: {} KiB, {} table slots, {} pages claimed, {} exported blocks",
                m.wasm.len() / 1024,
                m.table_slots,
                m.dir_entries.len(),
                m.exported_blocks.len()
            );
        }
        return;
    }

    eprintln!("translating from {addr:#x}, at most {max_blocks} blocks");
    let _ = std::io::stderr().flush();
    let began = std::time::Instant::now();
    let (unit, stats) = gex_xlate::translate_runtime(
        &memory,
        addr,
        &|a| exec.iter().any(|(s, e)| a >= *s && a + 4 <= *e),
        &|_| false,
        gex_xlate::PageTable::default(),
        Default::default(),
        max_blocks,
        false,
    );
    println!(
        "{} blocks, {} instructions, {} KiB of wasm, in {:.2}s",
        stats.blocks,
        stats.instructions,
        unit.wasm.len() / 1024,
        began.elapsed().as_secs_f64()
    );
}
