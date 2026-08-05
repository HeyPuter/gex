//! ELF64 reading and placement for aarch64 guests.

use std::fmt;

// ELF constants, named as in `elf.h`.
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_AARCH64: u16 = 183;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_TLS: u32 = 7;

const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

const SHT_SYMTAB: u32 = 2;
const STT_FUNC: u8 = 2;

const DT_NULL: i64 = 0;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;

const R_AARCH64_RELATIVE: u32 = 1027;
const R_AARCH64_IRELATIVE: u32 = 1032;

/// Why an image could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    TooShort,
    NotElf,
    NotElf64Le,
    NotAarch64,
    UnsupportedType(u16),
    /// The image declares a `PT_INTERP`; gex-ld's dynamic path (PLAN §6.4)
    /// lands in M2.
    NeedsInterpreter(String),
    /// A segment would not fit in the guest address space.
    SegmentOutOfRange { vaddr: u64, size: u64 },
    Malformed(&'static str),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::TooShort => write!(f, "file is too short to be an ELF image"),
            LoadError::NotElf => write!(f, "not an ELF file (bad magic)"),
            LoadError::NotElf64Le => write!(f, "not a 64-bit little-endian ELF file"),
            LoadError::NotAarch64 => write!(f, "not an aarch64 ELF file"),
            LoadError::UnsupportedType(t) => write!(f, "unsupported ELF type {t}"),
            LoadError::NeedsInterpreter(p) => {
                write!(f, "dynamically linked (PT_INTERP = {p}); not supported yet")
            }
            LoadError::SegmentOutOfRange { vaddr, size } => write!(
                f,
                "segment at 0x{vaddr:x} (0x{size:x} bytes) does not fit in the guest address space"
            ),
            LoadError::Malformed(m) => write!(f, "malformed ELF: {m}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// One mapped region of the loaded image.
#[derive(Clone, Debug)]
pub struct Segment {
    /// Guest virtual address after applying the load bias.
    pub vaddr: u64,
    /// Bytes to copy from the file; shorter than `mem_size` for `.bss`.
    pub bytes: Vec<u8>,
    /// Total size of the mapping, zero-filled beyond `bytes`.
    pub mem_size: u64,
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
}

/// A function symbol, used as an entry-point hint for code discovery.
#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub value: u64,
    pub size: u64,
}

/// A loaded process image.
#[derive(Clone, Debug)]
pub struct Image {
    /// Load bias applied to every `p_vaddr` (zero for `ET_EXEC`).
    pub bias: u64,
    /// Guest entry point, already biased.
    pub entry: u64,
    pub segments: Vec<Segment>,
    /// Guest address of the program headers, for `AT_PHDR`.
    pub phdr_vaddr: u64,
    pub phentsize: u64,
    pub phnum: u64,
    /// First address above every loaded segment: where `brk` starts.
    pub brk: u64,
    /// Executable address range, as `[start, end)`.
    pub text_range: (u64, u64),
    /// `PT_TLS`, if present: `(vaddr, filesz, memsz, align)`.
    pub tls: Option<(u64, u64, u64, u64)>,
    /// Function symbols, when a symbol table survives.
    pub symbols: Vec<Symbol>,
    /// True for `ET_DYN` images.
    pub pie: bool,
}

struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn u8(&self, off: usize) -> Result<u8, LoadError> {
        self.data.get(off).copied().ok_or(LoadError::TooShort)
    }
    fn u16(&self, off: usize) -> Result<u16, LoadError> {
        let b = self.data.get(off..off + 2).ok_or(LoadError::TooShort)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&self, off: usize) -> Result<u32, LoadError> {
        let b = self.data.get(off..off + 4).ok_or(LoadError::TooShort)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&self, off: usize) -> Result<u64, LoadError> {
        let b = self.data.get(off..off + 8).ok_or(LoadError::TooShort)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn slice(&self, off: usize, len: usize) -> Result<&'a [u8], LoadError> {
        self.data.get(off..off + len).ok_or(LoadError::TooShort)
    }
    fn cstr(&self, off: usize) -> String {
        let end = self.data[off..]
            .iter()
            .position(|b| *b == 0)
            .map(|n| off + n)
            .unwrap_or(self.data.len());
        String::from_utf8_lossy(&self.data[off..end]).into_owned()
    }
}

/// Parse and place an aarch64 ELF image.
///
/// `pie_base` is the load bias used for `ET_DYN` images; `ET_EXEC` images are
/// placed at their own `p_vaddr`s.
pub fn load(data: &[u8], pie_base: u64) -> Result<Image, LoadError> {
    let r = Reader { data };
    if data.len() < 64 {
        return Err(LoadError::TooShort);
    }
    if &data[0..4] != b"\x7fELF" {
        return Err(LoadError::NotElf);
    }
    if r.u8(4)? != ELFCLASS64 || r.u8(5)? != ELFDATA2LSB {
        return Err(LoadError::NotElf64Le);
    }
    let e_type = r.u16(16)?;
    if r.u16(18)? != EM_AARCH64 {
        return Err(LoadError::NotAarch64);
    }
    let pie = match e_type {
        ET_EXEC => false,
        ET_DYN => true,
        other => return Err(LoadError::UnsupportedType(other)),
    };
    let bias = if pie { pie_base } else { 0 };

    let e_entry = r.u64(24)?;
    let e_phoff = r.u64(32)? as usize;
    let e_shoff = r.u64(40)? as usize;
    let e_phentsize = r.u16(54)? as usize;
    let e_phnum = r.u16(56)? as usize;
    let e_shentsize = r.u16(58)? as usize;
    let e_shnum = r.u16(60)? as usize;

    let mut segments = Vec::new();
    let mut brk = 0u64;
    let mut text_lo = u64::MAX;
    let mut text_hi = 0u64;
    let mut phdr_vaddr = 0u64;
    let mut dynamic: Option<(u64, u64)> = None;
    let mut tls = None;
    let mut min_vaddr = u64::MAX;

    for i in 0..e_phnum {
        let off = e_phoff + i * e_phentsize;
        let p_type = r.u32(off)?;
        let p_flags = r.u32(off + 4)?;
        let p_offset = r.u64(off + 8)? as usize;
        let p_vaddr = r.u64(off + 16)?;
        let p_filesz = r.u64(off + 32)?;
        let p_memsz = r.u64(off + 40)?;

        match p_type {
            PT_INTERP => {
                return Err(LoadError::NeedsInterpreter(r.cstr(p_offset)));
            }
            PT_DYNAMIC => dynamic = Some((p_vaddr + bias, p_memsz)),
            PT_TLS => tls = Some((p_vaddr + bias, p_filesz, p_memsz, r.u64(off + 48)?)),
            PT_LOAD => {
                let vaddr = p_vaddr + bias;
                if vaddr.checked_add(p_memsz).is_none()
                    || vaddr + p_memsz >= gex_abi::GUEST_LIMIT
                {
                    return Err(LoadError::SegmentOutOfRange {
                        vaddr,
                        size: p_memsz,
                    });
                }
                min_vaddr = min_vaddr.min(vaddr);
                let bytes = r.slice(p_offset, p_filesz as usize)?.to_vec();
                let executable = p_flags & PF_X != 0;
                if executable {
                    text_lo = text_lo.min(vaddr);
                    text_hi = text_hi.max(vaddr + p_memsz);
                }
                brk = brk.max(vaddr + p_memsz);
                segments.push(Segment {
                    vaddr,
                    bytes,
                    mem_size: p_memsz,
                    readable: p_flags & PF_R != 0,
                    writable: p_flags & PF_W != 0,
                    executable,
                });
                // The program headers are conventionally inside the first
                // PT_LOAD; locate them by file offset.
                if (p_offset as u64) <= e_phoff as u64
                    && (e_phoff as u64) < p_offset as u64 + p_filesz
                {
                    phdr_vaddr = vaddr + (e_phoff as u64 - p_offset as u64);
                }
            }
            _ => {}
        }
    }

    if segments.is_empty() {
        return Err(LoadError::Malformed("no PT_LOAD segments"));
    }
    if text_lo == u64::MAX {
        return Err(LoadError::Malformed("no executable segment"));
    }

    let mut image = Image {
        bias,
        entry: e_entry + bias,
        segments,
        phdr_vaddr,
        phentsize: e_phentsize as u64,
        phnum: e_phnum as u64,
        // brk starts on the next page boundary above the image.
        brk: (brk + 0xfff) & !0xfff,
        text_range: (text_lo, text_hi),
        tls,
        symbols: Vec::new(),
        pie,
    };

    if let Some((dyn_vaddr, dyn_size)) = dynamic {
        apply_relative_relocations(&mut image, dyn_vaddr, dyn_size);
    }
    image.symbols = read_symbols(&r, e_shoff, e_shentsize, e_shnum, bias);
    Ok(image)
}

/// Apply the relocations a static-PIE image needs before its own code runs.
///
/// Only `R_AARCH64_RELATIVE` is handled here. `R_AARCH64_IRELATIVE` resolvers
/// are guest code and must run after entry (PLAN §6.4), so they are left to
/// the image's own startup path — which is exactly what musl's `rcrt1` does.
/// Applying `RELATIVE` twice is harmless, because it writes an absolute value
/// rather than adding to one, so an image that also self-relocates stays
/// correct.
///
/// Reads happen through the *placed* segments rather than the file, so the
/// dynamic table and relocation entries are seen exactly as the guest will
/// see them.
fn apply_relative_relocations(image: &mut Image, dyn_vaddr: u64, dyn_size: u64) {
    let mut rela: Option<u64> = None;
    let mut relasz: u64 = 0;
    let mut relaent: u64 = 24;

    for i in 0..dyn_size / 16 {
        let at = dyn_vaddr + i * 16;
        let (Some(tag), Some(val)) = (read_u64(image, at), read_u64(image, at + 8)) else {
            break;
        };
        match tag as i64 {
            DT_NULL => break,
            DT_RELA => rela = Some(val + image.bias),
            DT_RELASZ => relasz = val,
            DT_RELAENT => relaent = val,
            _ => {}
        }
    }

    let Some(rela_vaddr) = rela else { return };
    if relasz == 0 || relaent < 24 {
        return;
    }
    for i in 0..relasz / relaent {
        let at = rela_vaddr + i * relaent;
        let (Some(r_offset), Some(r_info), Some(r_addend)) = (
            read_u64(image, at),
            read_u64(image, at + 8),
            read_u64(image, at + 16),
        ) else {
            continue;
        };
        let r_type = (r_info & 0xffff_ffff) as u32;
        if r_type == R_AARCH64_RELATIVE {
            let target = r_offset + image.bias;
            let value = image.bias.wrapping_add(r_addend);
            write_u64(image, target, value);
        }
        // R_AARCH64_IRELATIVE is deliberately left to guest startup.
        let _ = R_AARCH64_IRELATIVE;
    }
}

/// Read a doubleword from the placed image.
fn read_u64(image: &Image, vaddr: u64) -> Option<u64> {
    for seg in &image.segments {
        if vaddr >= seg.vaddr && vaddr + 8 <= seg.vaddr + seg.bytes.len() as u64 {
            let off = (vaddr - seg.vaddr) as usize;
            let b = &seg.bytes[off..off + 8];
            return Some(u64::from_le_bytes([
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
            ]));
        }
    }
    None
}

/// Write a doubleword into the placed image.
fn write_u64(image: &mut Image, vaddr: u64, value: u64) {
    for seg in &mut image.segments {
        if vaddr >= seg.vaddr && vaddr + 8 <= seg.vaddr + seg.bytes.len() as u64 {
            let off = (vaddr - seg.vaddr) as usize;
            seg.bytes[off..off + 8].copy_from_slice(&value.to_le_bytes());
            return;
        }
    }
}

fn read_symbols(
    r: &Reader<'_>,
    e_shoff: usize,
    e_shentsize: usize,
    e_shnum: usize,
    bias: u64,
) -> Vec<Symbol> {
    let mut out = Vec::new();
    if e_shoff == 0 || e_shnum == 0 {
        return out;
    }
    for i in 0..e_shnum {
        let off = e_shoff + i * e_shentsize;
        let Ok(sh_type) = r.u32(off + 4) else { continue };
        if sh_type != SHT_SYMTAB {
            continue;
        }
        let (Ok(sh_offset), Ok(sh_size), Ok(sh_link), Ok(sh_entsize)) = (
            r.u64(off + 24),
            r.u64(off + 32),
            r.u32(off + 40),
            r.u64(off + 56),
        ) else {
            continue;
        };
        if sh_entsize == 0 {
            continue;
        }
        // The string table for this symbol table.
        let str_off = {
            let so = e_shoff + sh_link as usize * e_shentsize;
            r.u64(so + 24).unwrap_or(0) as usize
        };
        let n = (sh_size / sh_entsize) as usize;
        for k in 0..n {
            let so = sh_offset as usize + k * sh_entsize as usize;
            let (Ok(st_name), Ok(st_info), Ok(st_value), Ok(st_size)) =
                (r.u32(so), r.u8(so + 4), r.u64(so + 8), r.u64(so + 16))
            else {
                continue;
            };
            if st_info & 0xf != STT_FUNC || st_value == 0 {
                continue;
            }
            out.push(Symbol {
                name: r.cstr(str_off + st_name as usize),
                value: st_value + bias,
                size: st_size,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_elf() {
        assert_eq!(load(&[0; 64], 0).unwrap_err(), LoadError::NotElf);
        assert_eq!(load(&[0; 8], 0).unwrap_err(), LoadError::TooShort);
    }
}
