//! `.eh_frame` and LSDA parsing, for function recovery.
//!
//! Every function the compiler emitted unwind information for has an FDE,
//! whose `pc_begin`/`pc_range` give its exact extent, and that survives
//! `strip`. The FDE's LSDA (C++ exception tables) names the landing pads,
//! which the unwinder jumps to from outside the function: interior entries.
//!
//! Parsed from the file's section headers, so a binary stripped of those
//! as well yields nothing (and recovery falls back to the other sources).

use gex_ld::Image;

/// One FDE: a function's address range, and its LSDA if it has one. All
/// addresses biased.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fde {
    pub start: u64,
    pub end: u64,
    pub lsda: Option<u64>,
}

/// A section header, as far as recovery cares.
#[derive(Clone, Debug)]
pub struct Section {
    pub name: String,
    pub addr: u64,
    pub offset: u64,
    pub size: u64,
    pub nobits: bool,
}

fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}
fn u64_at(d: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(o..o + 8)?.try_into().ok()?))
}

/// The section headers of an ELF64 file, empty when there are none.
pub fn sections(elf: &[u8]) -> Vec<Section> {
    let mut out = Vec::new();
    let (Some(shoff), Some(shentsize), Some(shnum), Some(shstrndx)) = (
        u64_at(elf, 0x28),
        u16_at(elf, 0x3a),
        u16_at(elf, 0x3c),
        u16_at(elf, 0x3e),
    ) else {
        return out;
    };
    if shoff == 0 || shentsize < 64 {
        return out;
    }
    let hdr = |i: usize| shoff as usize + i * shentsize as usize;
    let strtab = u64_at(elf, hdr(shstrndx as usize) + 24).unwrap_or(0) as usize;
    for i in 0..shnum as usize {
        let h = hdr(i);
        let (Some(name), Some(ty), Some(addr), Some(offset), Some(size)) = (
            u32_at(elf, h),
            u32_at(elf, h + 4),
            u64_at(elf, h + 16),
            u64_at(elf, h + 24),
            u64_at(elf, h + 32),
        ) else {
            continue;
        };
        let n = strtab + name as usize;
        let end = elf[n.min(elf.len())..]
            .iter()
            .position(|&b| b == 0)
            .map_or(elf.len(), |p| n + p);
        out.push(Section {
            name: String::from_utf8_lossy(&elf[n.min(elf.len())..end]).into_owned(),
            addr,
            offset,
            size,
            nobits: ty == 8,
        });
    }
    out
}

/// A cursor over encoded DWARF data, `base` being the address of `d[0]`.
struct Cur<'a> {
    d: &'a [u8],
    pos: usize,
    base: u64,
}

impl Cur<'_> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.d.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    fn uleb(&mut self) -> Option<u64> {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let b = self.u8()?;
            if shift < 64 {
                v |= ((b & 0x7f) as u64) << shift;
            }
            shift += 7;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
    }
    fn sleb(&mut self) -> Option<i64> {
        let (mut v, mut shift) = (0i64, 0u32);
        loop {
            let b = self.u8()?;
            if shift < 64 {
                v |= ((b & 0x7f) as i64) << shift;
            }
            shift += 7;
            if b & 0x80 == 0 {
                if shift < 64 && b & 0x40 != 0 {
                    v |= -1i64 << shift;
                }
                return Some(v);
            }
        }
    }
    fn fixed(&mut self, n: usize) -> Option<u64> {
        let b = self.d.get(self.pos..self.pos + n)?;
        self.pos += n;
        let mut buf = [0u8; 8];
        buf[..n].copy_from_slice(b);
        Some(u64::from_le_bytes(buf))
    }
    /// A pointer in DWARF encoding `enc` (`DW_EH_PE_*`). Indirect pointers
    /// are not followed (`None`): nothing here needs one.
    fn ptr(&mut self, enc: u8) -> Option<u64> {
        if enc == 0xff {
            return None;
        }
        let at = self.base.wrapping_add(self.pos as u64);
        let raw = match enc & 0x0f {
            0x00 => self.fixed(8)?,
            0x01 => self.uleb()?,
            0x02 => self.fixed(2)?,
            0x03 => self.fixed(4)?,
            0x04 => self.fixed(8)?,
            0x09 => self.sleb()? as u64,
            0x0a => self.fixed(2)? as u16 as i16 as i64 as u64,
            0x0b => self.fixed(4)? as u32 as i32 as i64 as u64,
            0x0c => self.fixed(8)?,
            _ => return None,
        };
        let v = match enc & 0x70 {
            0x00 => raw,
            0x10 => at.wrapping_add(raw),
            // datarel/textrel/funcrel: not used by .eh_frame on Linux.
            _ => return None,
        };
        if enc & 0x80 != 0 {
            return None;
        }
        Some(v)
    }
}

/// Every FDE in the file's `.eh_frame`, biased by `bias`.
pub fn fdes(elf: &[u8], bias: u64) -> Vec<Fde> {
    let Some(sec) = sections(elf)
        .into_iter()
        .find(|s| s.name == ".eh_frame" && !s.nobits)
    else {
        return Vec::new();
    };
    let Some(d) = elf.get(sec.offset as usize..(sec.offset + sec.size) as usize) else {
        return Vec::new();
    };
    parse_eh_frame(d, sec.addr.wrapping_add(bias))
}

/// Parse `.eh_frame` bytes that live at guest address `base`.
pub fn parse_eh_frame(d: &[u8], base: u64) -> Vec<Fde> {
    #[derive(Clone, Copy)]
    struct Cie {
        fde_enc: u8,
        lsda_enc: u8,
        z: bool,
    }
    let mut cies: std::collections::HashMap<usize, Option<Cie>> = Default::default();
    let parse_cie = |at: usize| -> Option<Cie> {
        let mut c = Cur { d, pos: at, base };
        let len = c.fixed(4)?;
        if len == 0xffff_ffff {
            return None;
        }
        let _id = c.fixed(4)?;
        let version = c.u8()?;
        let aug_start = c.pos;
        while c.u8()? != 0 {}
        let aug = &d[aug_start..c.pos - 1];
        if aug.contains(&b'e') {
            // "eh": an ancient GCC format with a pointer here.
            return None;
        }
        c.uleb()?; // code alignment
        c.sleb()?; // data alignment
        if version == 1 {
            c.u8()?;
        } else {
            c.uleb()?;
        }
        let mut cie = Cie {
            fde_enc: 0,
            lsda_enc: 0xff,
            z: false,
        };
        if aug.first() == Some(&b'z') {
            cie.z = true;
            c.uleb()?;
            for &ch in &aug[1..] {
                match ch {
                    b'R' => cie.fde_enc = c.u8()?,
                    b'L' => cie.lsda_enc = c.u8()?,
                    b'P' => {
                        let enc = c.u8()?;
                        // The personality pointer; its value is not needed,
                        // but it has to be stepped over.
                        let _ = c.ptr(enc & 0x7f);
                    }
                    b'S' | b'B' | b'G' => {}
                    _ => return None,
                }
            }
        }
        Some(cie)
    };

    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= d.len() {
        let mut c = Cur { d, pos, base };
        let Some(len) = c.fixed(4) else { break };
        if len == 0 {
            break;
        }
        if len == 0xffff_ffff {
            // 64-bit DWARF: never produced for .eh_frame in practice.
            break;
        }
        let next = pos + 4 + len as usize;
        let id_pos = c.pos;
        let Some(id) = c.fixed(4) else { break };
        if id != 0 {
            let cie_at = id_pos.wrapping_sub(id as usize);
            let cie = *cies.entry(cie_at).or_insert_with(|| parse_cie(cie_at));
            if let Some(cie) = cie {
                let start = c.ptr(cie.fde_enc);
                let range = c.ptr(cie.fde_enc & 0x0f);
                let mut lsda = None;
                if cie.z {
                    if let Some(_aug_len) = c.uleb() {
                        if cie.lsda_enc != 0xff {
                            lsda = c.ptr(cie.lsda_enc).filter(|&a| a != 0);
                        }
                    }
                }
                if let (Some(s), Some(r)) = (start, range) {
                    // A zero pc_begin is a discarded (garbage-collected)
                    // function's FDE left behind by the linker.
                    if s != 0 && r != 0 {
                        out.push(Fde {
                            start: s,
                            end: s.wrapping_add(r),
                            lsda,
                        });
                    }
                }
            }
        }
        pos = next;
    }
    out
}

/// The landing pads an LSDA names, for a function starting at `func`.
pub fn landing_pads(image: &Image, lsda: u64, func: u64) -> Vec<u64> {
    let mut out = Vec::new();
    // The LSDA is small; take what the segment holds from it on.
    let Some(seg) = image
        .segments
        .iter()
        .find(|s| lsda >= s.vaddr && lsda < s.vaddr + s.bytes.len() as u64)
    else {
        return out;
    };
    let d = &seg.bytes[(lsda - seg.vaddr) as usize..];
    let mut c = Cur { d, pos: 0, base: lsda };
    let Some(lp_enc) = c.u8() else { return out };
    let lpstart = if lp_enc != 0xff {
        match c.ptr(lp_enc) {
            Some(v) => v,
            None => return out,
        }
    } else {
        func
    };
    let Some(tt_enc) = c.u8() else { return out };
    if tt_enc != 0xff && c.uleb().is_none() {
        return out;
    }
    let Some(cs_enc) = c.u8() else { return out };
    let Some(cs_len) = c.uleb() else { return out };
    let end = c.pos + cs_len as usize;
    while c.pos < end {
        let (Some(_start), Some(_len), Some(lp)) =
            (c.ptr(cs_enc & 0x0f), c.ptr(cs_enc & 0x0f), c.ptr(cs_enc & 0x0f))
        else {
            break;
        };
        if c.uleb().is_none() {
            break;
        }
        if lp != 0 {
            out.push(lpstart.wrapping_add(lp));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_cie_and_an_fde() {
        // CIE: len, id 0, version 1, "zR", code 1, data -8, ra 30,
        // aug len 1, R = pcrel|sdata4 (0x1b), padding.
        let mut d: Vec<u8> = Vec::new();
        let cie_body = [
            0u8, 0, 0, 0, // id
            1, b'z', b'R', 0, 1, 0x78, 30, 1, 0x1b, 0, 0, 0,
        ];
        d.extend_from_slice(&(cie_body.len() as u32).to_le_bytes());
        d.extend_from_slice(&cie_body);
        // FDE at offset 20: len, CIE pointer (offset from here back to 0),
        // pc_begin pcrel, pc_range, aug len 0.
        let fde_at = d.len();
        let mut fde = Vec::new();
        fde.extend_from_slice(&((fde_at + 4) as u32).to_le_bytes());
        let pc_field = (fde_at + 8) as u64;
        let base = 0x10000u64;
        let target = 0x400000u64;
        fde.extend_from_slice(&((target.wrapping_sub(base + pc_field)) as u32).to_le_bytes());
        fde.extend_from_slice(&0x40u32.to_le_bytes());
        fde.push(0);
        fde.extend_from_slice(&[0, 0, 0]);
        d.extend_from_slice(&(fde.len() as u32).to_le_bytes());
        d.extend_from_slice(&fde);
        d.extend_from_slice(&0u32.to_le_bytes());
        let fdes = parse_eh_frame(&d, base);
        assert_eq!(
            fdes,
            vec![Fde {
                start: target,
                end: target + 0x40,
                lsda: None
            }]
        );
    }
}
