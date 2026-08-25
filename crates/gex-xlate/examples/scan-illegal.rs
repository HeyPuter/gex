//! Decode every 4-byte word in an ELF's executable segments and print the ones
//! gex's decoder rejects. Unlike `translate-at object`, which only follows
//! reachable blocks, this covers every instruction slot -- so a rounding-narrow
//! op sitting in a branch table the block-walker never entered still shows up.
//! Data words in .text will show too; cross-reference with a real disassembler
//! (capstone) to keep only the ones that are genuinely instructions.
//!
//!   cargo run --release -p gex-xlate --example scan-illegal -- <elf>
use std::collections::BTreeMap;

fn main() {
    let path = std::env::args().nth(1).expect("usage: scan-illegal <elf>");
    let elf = std::fs::read(&path).expect("read the elf");
    let phoff = u64::from_le_bytes(elf[0x20..0x28].try_into().unwrap()) as usize;
    let phentsize = u16::from_le_bytes(elf[0x36..0x38].try_into().unwrap()) as usize;
    let phnum = u16::from_le_bytes(elf[0x38..0x3a].try_into().unwrap()) as usize;
    // raw -> count, for every word the decoder cannot place.
    let mut unknown: BTreeMap<u32, u32> = BTreeMap::new();
    let mut total = 0u64;
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        let kind = u32::from_le_bytes(elf[p..p + 4].try_into().unwrap());
        let flags = u32::from_le_bytes(elf[p + 4..p + 8].try_into().unwrap());
        if kind != 1 || flags & 1 == 0 {
            continue; // PT_LOAD, executable only
        }
        let offset = u64::from_le_bytes(elf[p + 8..p + 16].try_into().unwrap()) as usize;
        let filesz = u64::from_le_bytes(elf[p + 32..p + 40].try_into().unwrap()) as usize;
        let mut off = offset;
        while off + 4 <= offset + filesz {
            let raw = u32::from_le_bytes(elf[off..off + 4].try_into().unwrap());
            total += 1;
            match gex_isa::decode::decode(raw, 0) {
                gex_isa::Inst::Unknown { .. } | gex_isa::Inst::Undefined { .. } => {
                    *unknown.entry(raw).or_default() += 1;
                }
                _ => {}
            }
            off += 4;
        }
    }
    // Most common first: a hot missing instruction repeats across a library.
    let mut v: Vec<_> = unknown.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    eprintln!("{total} words scanned, {} distinct undecoded", v.len());
    for (raw, n) in v.iter() {
        println!("{raw:#010x} {n}");
    }
}
