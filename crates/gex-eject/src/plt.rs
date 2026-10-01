//! Finding an object's PLT stubs: the address each import is called at.
//!
//! A call to an imported function goes to a small stub in the importer that
//! jumps through the function's GOT slot. Binding a replacement to the stub
//! (rather than to the definition the slot resolves to) catches exactly that
//! importer's calls. The stubs are found by their shape, pointing at a GOT
//! slot a `JUMP_SLOT`/`GLOB_DAT` relocation names (`Image::named_relocs`):
//!
//! - x86-64: `[endbr64] [bnd] jmp *disp(%rip)`, 16-byte aligned (`.plt`,
//!   `.plt.sec`);
//! - AArch64: `[bti c] adrp x16; ldr x17, [x16, #lo]; add x16, x16, #lo;
//!   br x17`, 16-byte aligned.

use gex_ld::Image;
use std::collections::HashMap;

/// `(stub address, imported name)` for every stub found, sorted by address.
pub fn stubs(image: &Image) -> Vec<(u64, String)> {
    let slots: HashMap<u64, &str> = image.named_relocs.iter().map(|(s, n)| (*s, n.as_str())).collect();
    let mut out = Vec::new();
    for seg in image.segments.iter().filter(|s| s.executable) {
        let b = &seg.bytes;
        if gex_abi::X86 {
            for i in 0..b.len().saturating_sub(6) {
                let (op, next) = if b[i..].starts_with(&[0xf2, 0xff, 0x25]) {
                    (i + 3, i + 7)
                } else if b[i..].starts_with(&[0xff, 0x25]) && (i == 0 || b[i - 1] != 0xf2) {
                    (i + 2, i + 6)
                } else {
                    continue;
                };
                let Some(d) = b.get(op..op + 4) else { continue };
                let disp = i32::from_le_bytes(d.try_into().unwrap()) as i64;
                let slot = (seg.vaddr + next as u64).wrapping_add(disp as u64);
                let start = if i >= 4 && b[i - 4..i] == [0xf3, 0x0f, 0x1e, 0xfa] { i - 4 } else { i };
                let at = seg.vaddr + start as u64;
                if at % 16 != 0 {
                    continue;
                }
                if let Some(name) = slots.get(&slot) {
                    out.push((at, name.to_string()));
                }
            }
        } else {
            let word = |i: usize| b.get(i..i + 4).map(|w| u32::from_le_bytes(w.try_into().unwrap()));
            let mut i = 0;
            while i + 16 <= b.len() {
                let (Some(adrp), Some(ldr), Some(add), Some(br)) = (word(i), word(i + 4), word(i + 8), word(i + 12)) else {
                    break;
                };
                let pc = seg.vaddr + i as u64;
                if adrp & 0x9f00_001f == 0x9000_0010
                    && ldr & 0xffc0_03ff == 0xf940_0211
                    && add & 0xffc0_03ff == 0x9100_0210
                    && br == 0xd61f_0220
                {
                    let imm = (((adrp >> 5) & 0x7ffff) << 2 | (adrp >> 29) & 3) as i64;
                    let imm = (imm << 43) >> 43; // sign-extend 21 bits
                    let page = (pc & !0xfff).wrapping_add((imm << 12) as u64);
                    let slot = page + u64::from((ldr >> 10) & 0xfff) * 8;
                    let start = if i >= 4 && word(i - 4) == Some(0xd503_245f) { pc - 4 } else { pc };
                    if start % 16 == 0 {
                        if let Some(name) = slots.get(&slot) {
                            out.push((start, name.to_string()));
                        }
                    }
                }
                i += 4;
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(vaddr: u64, code: Vec<u8>, relocs: Vec<(u64, String)>) -> Image {
        let len = code.len() as u64;
        Image {
            soname: None,
            needed: Vec::new(),
            runpath: Vec::new(),
            bias: 0,
            entry: vaddr,
            code: vec![(vaddr, vaddr + len)],
            segments: vec![gex_ld::Segment {
                vaddr,
                bytes: code,
                mem_size: len,
                readable: true,
                writable: false,
                executable: true,
            }],
            phdr_vaddr: vaddr,
            phentsize: 56,
            phnum: 0,
            brk: vaddr + len,
            text_range: (vaddr, vaddr + len),
            tls: None,
            class32: false,
            symbols: Vec::new(),
            named_relocs: relocs,
            relocs: Vec::new(),
            init_array: Vec::new(),
            dynsyms: Vec::new(),
            irelative: Vec::new(),
            pie: true,
            interp: None,
        }
    }

    #[test]
    fn finds_a_stub_by_the_slot_it_jumps_through() {
        let base = 0x1000_0000u64;
        let (code, slot) = if gex_abi::X86 {
            // 0x..00: endbr64; bnd jmp *0x2000(%rip); nop  (.plt.sec)
            let mut c = vec![0xf3, 0x0f, 0x1e, 0xfa, 0xf2, 0xff, 0x25];
            c.extend_from_slice(&0x2000i32.to_le_bytes());
            c.extend_from_slice(&[0x0f, 0x1f, 0x44, 0x00, 0x00]);
            (c, base + 11 + 0x2000)
        } else {
            // adrp x16, +1 page; ldr x17, [x16, #0x18]; add x16, x16, #0x18; br x17
            let words = [0xb000_0010u32, 0xf940_0e11, 0x9100_6210, 0xd61f_0220];
            (words.iter().flat_map(|w| w.to_le_bytes()).collect(), base + 0x1000 + 0x18)
        };
        let img = image(base, code, vec![(slot, "getenv".into()), (slot + 8, "other".into())]);
        assert_eq!(stubs(&img), vec![(base, "getenv".to_string())]);
        // A jump through a slot no relocation names is not a stub.
        let img = image(base, img.segments[0].bytes.clone(), vec![(slot + 8, "other".into())]);
        assert!(stubs(&img).is_empty());
    }
}
