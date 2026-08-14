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

const SHT_DYNSYM: u32 = 11;
const SHT_SYMTAB: u32 = 2;
const STT_FUNC: u8 = 2;
/// A function whose address is decided at load time by running a resolver.
/// The symbol's value is the *resolver*, not the implementation.
const STT_GNU_IFUNC: u8 = 10;

const DT_NULL: i64 = 0;
const DT_NEEDED: i64 = 1;
const DT_PLTRELSZ: i64 = 2;
const DT_STRTAB: i64 = 5;
const DT_SYMTAB: i64 = 6;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;
const DT_SYMENT: i64 = 11;
const DT_JMPREL: i64 = 23;
const DT_SONAME: i64 = 14;
const DT_RPATH: i64 = 15;
const DT_RUNPATH: i64 = 29;
const DT_INIT_ARRAY: i64 = 25;
const DT_INIT_ARRAYSZ: i64 = 27;
const DT_INIT: i64 = 12;

const R_AARCH64_RELATIVE: u32 = 1027;
const R_AARCH64_IRELATIVE: u32 = 1032;
const R_AARCH64_GLOB_DAT: u32 = 1025;
const R_AARCH64_JUMP_SLOT: u32 = 1026;

/// Why an image could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    TooShort,
    NotElf,
    NotElf64Le,
    NotAarch64,
    UnsupportedType(u16),
    /// The image declares a `PT_INTERP` that could not be loaded.
    NeedsInterpreter(String),
    /// A segment would not fit in the guest address space.
    SegmentOutOfRange {
        vaddr: u64,
        size: u64,
    },
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
                write!(f, "could not load the interpreter {p}")
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
    /// For an ordinary function, where it is. For an indirect one, where its
    /// *resolver* is -- see [`Symbol::ifunc`].
    pub value: u64,
    pub size: u64,
    /// `STT_GNU_IFUNC`: the implementation is whatever the resolver returns
    /// at startup, so `value` is not an address anything should be called
    /// at. aarch64 glibc defines `memcpy`, `memmove` and `memset` this way.
    pub ifunc: bool,
}

/// One relocation, ready to apply.
#[derive(Clone, Debug)]
pub struct Reloc {
    /// Where the fixup goes, biased.
    pub at: u64,
    /// `R_AARCH64_*`.
    pub kind: u32,
    /// Index into [`Image::dynsyms`], or zero for the kinds that name none.
    pub sym: u32,
    pub addend: i64,
}

/// A dynamic symbol, as a linker needs it.
#[derive(Clone, Debug)]
pub struct DynSym {
    pub name: String,
    /// Unbiased: an undefined symbol has no address in this object at all,
    /// and a defined one is only an address once the bias is known.
    pub value: u64,
    pub size: u64,
    /// `st_shndx == SHN_UNDEF`: this object references it and does not
    /// define it.
    pub undefined: bool,
    /// `STB_WEAK`: an undefined weak that nothing defines resolves to zero
    /// rather than failing the link.
    pub weak: bool,
    pub ifunc: bool,
    /// `STT_TLS`: the value is an offset in the defining object's TLS block,
    /// not an address.
    pub tls: bool,
}

/// An image holding only the numbers the auxiliary vector reports, for a
/// process that is restored rather than loaded.
pub fn headers_only(phdr_vaddr: u64, phentsize: u64, phnum: u64, entry: u64) -> Image {
    Image {
        bias: 0,
        entry,
        segments: Vec::new(),
        phdr_vaddr,
        phentsize,
        phnum,
        brk: 0,
        text_range: (0, 0),
        tls: None,
        symbols: Vec::new(),
        named_relocs: Vec::new(),
        relocs: Vec::new(),
        init_array: Vec::new(),
        dynsyms: Vec::new(),
        irelative: Vec::new(),
        code: Vec::new(),
        pie: true,
        interp: None,
        soname: None,
        needed: Vec::new(),
        runpath: Vec::new(),
    }
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
    /// The sections that actually hold instructions, biased, as `[start, end)`.
    ///
    /// Segment permissions are too coarse to discover code with: an `R E` LOAD
    /// spans `.rodata` and the string tables too, which disassemble into blocks
    /// that end in SIGILL. Empty when the section headers are gone, and then
    /// the segments are all there is to go on.
    pub code: Vec<(u64, u64)>,
    /// `PT_TLS`, if present: `(vaddr, filesz, memsz, align)`.
    pub tls: Option<(u64, u64, u64, u64)>,
    /// Function symbols, when a symbol table survives.
    pub symbols: Vec<Symbol>,
    /// Every relocation, with enough to apply it: where, what kind, which
    /// symbol and what to add. ([`Self::named_relocs`] keeps only the two kinds
    /// code discovery needs.)
    pub relocs: Vec<Reloc>,
    /// `DT_INIT_ARRAY`, biased, and how many entries: the functions that must
    /// run before `main`, in order.
    pub init_array: Vec<u64>,
    /// Every symbol the dynamic table defines or references, in index order,
    /// so a relocation's symbol index resolves without a second parse.
    pub dynsyms: Vec<DynSym>,
    /// Relocations that name a symbol, as `(slot, name)` with `slot` biased.
    /// The linker fills the GOT slot before the call; reading it afterwards is
    /// how an `STT_GNU_IFUNC` such as `memcpy` gets an address.
    pub named_relocs: Vec<(u64, String)>,
    /// `R_AARCH64_IRELATIVE` relocations, as `(slot, resolver)`, both biased.
    /// Left unapplied on purpose (the resolver is guest code); afterwards
    /// `slot` holds the address the resolver chose.
    pub irelative: Vec<(u64, u64)>,
    /// True for `ET_DYN` images.
    pub pie: bool,
    /// The `PT_INTERP` path, when the image asks for a dynamic linker.
    pub interp: Option<String>,
    /// `DT_SONAME`, the name other objects link against. Absent on
    /// executables and on a library linked without one.
    pub soname: Option<String>,
    /// `DT_NEEDED`, in order: the objects this one names as dependencies, read
    /// so a program's closure can be worked out without running it.
    pub needed: Vec<String>,
    /// `DT_RUNPATH`, or `DT_RPATH` when there is no runpath, split on `:` and
    /// with `$ORIGIN` left as it stands for the caller to expand.
    pub runpath: Vec<String>,
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
    let mut interp: Option<String> = None;
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
            PT_INTERP => interp = Some(r.cstr(p_offset)),
            PT_DYNAMIC => dynamic = Some((p_vaddr + bias, p_memsz)),
            PT_TLS => tls = Some((p_vaddr + bias, p_filesz, p_memsz, r.u64(off + 48)?)),
            PT_LOAD => {
                let vaddr = p_vaddr + bias;
                if vaddr.checked_add(p_memsz).is_none() || vaddr + p_memsz >= gex_abi::GUEST_LIMIT {
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
        named_relocs: Vec::new(),
        relocs: Vec::new(),
        init_array: Vec::new(),
        dynsyms: Vec::new(),
        irelative: Vec::new(),
        code: Vec::new(),
        pie,
        interp,
        soname: None,
        needed: Vec::new(),
        runpath: Vec::new(),
    };

    if let Some((dyn_vaddr, dyn_size)) = dynamic {
        apply_relative_relocations(&mut image, dyn_vaddr, dyn_size);
        image.named_relocs = read_named_relocs(&image, dyn_vaddr, dyn_size);
        let (syms, relocs, inits) = read_dynamic(&image, dyn_vaddr, dyn_size);
        image.dynsyms = syms;
        image.relocs = relocs;
        image.init_array = inits;
        read_needed(&mut image, dyn_vaddr, dyn_size);
    }
    image.symbols = read_symbols(&r, e_shoff, e_shentsize, e_shnum, bias);
    image.code = read_code_sections(&r, e_shoff, e_shentsize, e_shnum, bias);
    Ok(image)
}

/// `DT_SONAME`, `DT_NEEDED` and the search path, out of the dynamic section.
///
/// Two passes, because every one of these is an offset into `DT_STRTAB` and
/// the dynamic section does not promise to name the string table first.
fn read_needed(image: &mut Image, dyn_vaddr: u64, dyn_size: u64) {
    let mut strtab = None;
    let mut entries: Vec<(i64, u64)> = Vec::new();
    for i in 0..dyn_size / 16 {
        let at = dyn_vaddr + i * 16;
        let (Some(tag), Some(val)) = (read_u64(image, at), read_u64(image, at + 8)) else {
            break;
        };
        let tag = tag as i64;
        if tag == DT_NULL {
            break;
        }
        if tag == DT_STRTAB {
            strtab = Some(val + image.bias);
        }
        entries.push((tag, val));
    }
    let Some(strtab) = strtab else { return };
    let mut rpath = Vec::new();
    let mut runpath = Vec::new();
    for (tag, val) in entries {
        let s = || read_cstr(image, strtab + val);
        match tag {
            DT_NEEDED => {
                let n = s();
                if !n.is_empty() {
                    image.needed.push(n);
                }
            }
            DT_SONAME => {
                let n = s();
                if !n.is_empty() {
                    image.soname = Some(n);
                }
            }
            // `$ORIGIN` is left alone: expanding it needs the object's own
            // path, which this function does not have and the caller does.
            DT_RPATH => rpath = s().split(':').map(str::to_string).collect(),
            DT_RUNPATH => runpath = s().split(':').map(str::to_string).collect(),
            _ => {}
        }
    }
    // A runpath, when present, replaces the rpath entirely -- that is the
    // whole point of the newer tag.
    image.runpath = if runpath.is_empty() { rpath } else { runpath };
}

/// Everything a linker needs out of the dynamic section: the symbol table,
/// every relocation with its type and addend, and the initialisers.
/// ([`read_named_relocs`] answers a narrower question for code discovery.)
pub(crate) fn read_dynamic(image: &Image, dyn_vaddr: u64, dyn_size: u64)
    -> (Vec<DynSym>, Vec<Reloc>, Vec<u64>) {
    const STT_GNU_IFUNC: u8 = 10;
    const STT_TLS: u8 = 6;
    const STB_WEAK: u8 = 2;
    let (mut rela, mut relasz, mut relaent) = (None, 0u64, 24u64);
    let (mut jmprel, mut pltrelsz) = (None, 0u64);
    let (mut symtab, mut strtab, mut syment) = (None, None, 24u64);
    let (mut init_array, mut init_arraysz, mut init) = (None, 0u64, None);
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
            DT_JMPREL => jmprel = Some(val + image.bias),
            DT_PLTRELSZ => pltrelsz = val,
            DT_SYMTAB => symtab = Some(val + image.bias),
            DT_STRTAB => strtab = Some(val + image.bias),
            DT_SYMENT => syment = val,
            DT_INIT_ARRAY => init_array = Some(val + image.bias),
            DT_INIT_ARRAYSZ => init_arraysz = val,
            DT_INIT => init = Some(val + image.bias),
            _ => {}
        }
    }
    let (Some(symtab), Some(strtab)) = (symtab, strtab) else {
        return (Vec::new(), Vec::new(), Vec::new());
    };
    if syment == 0 || relaent < 24 {
        return (Vec::new(), Vec::new(), Vec::new());
    }

    // How many symbols there are is not written down anywhere: the table runs
    // to the end of `.dynstr`, which conventionally follows it. Reading until
    // the entry stops making sense is what every loader does.
    let mut syms = Vec::new();
    let limit = if strtab > symtab { (strtab - symtab) / syment } else { 4096 };
    for i in 0..limit {
        let at = symtab + i * syment;
        let (Some(st_name), Some(st_info), Some(st_shndx)) =
            (read_u32(image, at), read_u8(image, at + 4), read_u16(image, at + 6))
        else {
            break;
        };
        let (Some(value), Some(size)) = (read_u64(image, at + 8), read_u64(image, at + 16)) else {
            break;
        };
        let kind = st_info & 0xf;
        syms.push(DynSym {
            name: read_cstr(image, strtab + st_name as u64),
            value,
            size,
            undefined: st_shndx == 0,
            weak: (st_info >> 4) == STB_WEAK,
            ifunc: kind == STT_GNU_IFUNC,
            tls: kind == STT_TLS,
        });
    }

    let mut relocs = Vec::new();
    for (table, size) in [(rela, relasz), (jmprel, pltrelsz)] {
        let Some(base) = table else { continue };
        for i in 0..size / relaent {
            let at = base + i * relaent;
            let (Some(r_offset), Some(r_info), Some(r_addend)) = (
                read_u64(image, at),
                read_u64(image, at + 8),
                read_u64(image, at + 16),
            ) else {
                continue;
            };
            relocs.push(Reloc {
                at: r_offset + image.bias,
                kind: (r_info & 0xffff_ffff) as u32,
                sym: (r_info >> 32) as u32,
                addend: r_addend as i64,
            });
        }
    }

    let mut inits = Vec::new();
    if let Some(f) = init {
        inits.push(f);
    }
    if let Some(base) = init_array {
        for i in 0..init_arraysz / 8 {
            if let Some(f) = read_u64(image, base + i * 8) {
                // Unrelocated in the file and relocated in memory; before the
                // relocations are applied it is the file's value that is
                // there, so bias it here and let `RELATIVE` overwrite it.
                inits.push(if f < image.bias { f + image.bias } else { f });
            }
        }
    }
    (syms, relocs, inits)
}

fn read_named_relocs(image: &Image, dyn_vaddr: u64, dyn_size: u64) -> Vec<(u64, String)> {
    let (mut rela, mut relasz, mut relaent) = (None, 0u64, 24u64);
    let (mut jmprel, mut pltrelsz) = (None, 0u64);
    let (mut symtab, mut strtab, mut syment) = (None, None, 24u64);
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
            DT_JMPREL => jmprel = Some(val + image.bias),
            DT_PLTRELSZ => pltrelsz = val,
            DT_SYMTAB => symtab = Some(val + image.bias),
            DT_STRTAB => strtab = Some(val + image.bias),
            DT_SYMENT => syment = val,
            _ => {}
        }
    }
    let (Some(symtab), Some(strtab)) = (symtab, strtab) else {
        return Vec::new();
    };
    if syment == 0 || relaent < 24 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (table, size) in [(rela, relasz), (jmprel, pltrelsz)] {
        let Some(base) = table else { continue };
        for i in 0..size / relaent {
            let at = base + i * relaent;
            let (Some(r_offset), Some(r_info)) = (read_u64(image, at), read_u64(image, at + 8))
            else {
                continue;
            };
            let r_type = (r_info & 0xffff_ffff) as u32;
            if r_type != R_AARCH64_JUMP_SLOT && r_type != R_AARCH64_GLOB_DAT {
                continue;
            }
            let sym = r_info >> 32;
            let Some(st_name) = read_u32(image, symtab + sym * syment) else {
                continue;
            };
            let name = read_cstr(image, strtab + st_name as u64);
            if !name.is_empty() {
                out.push((r_offset + image.bias, name));
            }
        }
    }
    out
}

/// A NUL-terminated string from the placed image.
fn read_cstr(image: &Image, vaddr: u64) -> String {
    let mut out = Vec::new();
    for i in 0..256u64 {
        match read_u8(image, vaddr + i) {
            Some(0) | None => break,
            Some(b) => out.push(b),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Apply the relocations a static-PIE image needs before its own code runs.
///
/// Only `R_AARCH64_RELATIVE`; `IRELATIVE` resolvers are guest code and are
/// deferred to startup (PLAN §6.4). Applying `RELATIVE` twice is harmless (it
/// writes an absolute value), so an image that also self-relocates stays
/// correct. Reads go through the *placed* segments, not the file.
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
        // Applying an IRELATIVE means running guest code, which is the
        // guest's own job at startup. Recorded so that whoever wants the
        // answer can read the slot once the guest has done it.
        if r_type == R_AARCH64_IRELATIVE {
            image
                .irelative
                .push((r_offset + image.bias, image.bias.wrapping_add(r_addend)));
        }
    }
}

/// Apply this image's own `R_AARCH64_RELATIVE` relocations in place, at its
/// bias, from the parsed reloc list.
///
/// The closure build (`gexc build --closure`) translates each object WITHOUT
/// linking it, but `Options::scan_pointers` finds indirect-branch targets by
/// reading data words that point into text -- a vtable, a `DT_INIT_ARRAY`, the
/// pubkey/ECC op table libgcrypt's Ed25519 verify reaches through. Those words
/// are `RELATIVE` relocations, so unrelocated they hold their unbiased addend;
/// the scan reads that, `is_executable` rejects it, and the code it names is
/// never lifted -- so `gpgv` branches into an unlifted block whose demand-fill
/// over a closure-covered page corrupts dispatch (`apt-get update` verifying an
/// EdDSA `InRelease` signature died exactly here). Applying RELATIVE first -- a
/// self-reference needs only this object's own bias, not the whole closure's
/// symbol table -- makes the scan see the real address. Idempotent, like
/// [`apply_relative_relocations`]: it writes an absolute value.
pub fn apply_own_relative(image: &mut Image) {
    let bias = image.bias;
    let writes: Vec<(u64, u64)> = image
        .relocs
        .iter()
        .filter(|r| r.kind == R_AARCH64_RELATIVE)
        .map(|r| (r.at, bias.wrapping_add(r.addend as u64)))
        .collect();
    for (at, value) in writes {
        write_u64(image, at, value);
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

/// Read a word from the placed image.
fn read_u32(image: &Image, vaddr: u64) -> Option<u32> {
    for seg in &image.segments {
        if vaddr >= seg.vaddr && vaddr + 4 <= seg.vaddr + seg.bytes.len() as u64 {
            let off = (vaddr - seg.vaddr) as usize;
            let b = &seg.bytes[off..off + 4];
            return Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        }
    }
    None
}

/// Read a halfword from the placed image.
fn read_u16(image: &Image, vaddr: u64) -> Option<u16> {
    for seg in &image.segments {
        if vaddr >= seg.vaddr && vaddr + 2 <= seg.vaddr + seg.bytes.len() as u64 {
            let off = (vaddr - seg.vaddr) as usize;
            return Some(u16::from_le_bytes([seg.bytes[off], seg.bytes[off + 1]]));
        }
    }
    None
}

/// Read a byte from the placed image.
fn read_u8(image: &Image, vaddr: u64) -> Option<u8> {
    for seg in &image.segments {
        if vaddr >= seg.vaddr && vaddr < seg.vaddr + seg.bytes.len() as u64 {
            return Some(seg.bytes[(vaddr - seg.vaddr) as usize]);
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

/// The address ranges of every section marked `SHF_EXECINSTR`.
///
/// See [`Image::code`] for why the segment's `PF_X` is not good enough.
fn read_code_sections(
    r: &Reader<'_>,
    e_shoff: usize,
    e_shentsize: usize,
    e_shnum: usize,
    bias: u64,
) -> Vec<(u64, u64)> {
    const SHF_EXECINSTR: u64 = 0x4;
    const SHT_NOBITS: u32 = 8;
    let mut out: Vec<(u64, u64)> = Vec::new();
    if e_shoff == 0 || e_shnum == 0 {
        return out;
    }
    for i in 0..e_shnum {
        let off = e_shoff + i * e_shentsize;
        let (Ok(sh_type), Ok(sh_flags), Ok(sh_addr), Ok(sh_size)) = (
            r.u32(off + 4),
            r.u64(off + 8),
            r.u64(off + 16),
            r.u64(off + 32),
        ) else {
            continue;
        };
        // `.bss` can be marked executable and has no contents to read.
        if sh_flags & SHF_EXECINSTR == 0 || sh_type == SHT_NOBITS || sh_size == 0 {
            continue;
        }
        // A section with no address is not mapped, so nothing branches to it.
        if sh_addr == 0 {
            continue;
        }
        out.push((sh_addr + bias, sh_addr + bias + sh_size));
    }
    // Merged, because `.plt`, `.text` and `.fini` are usually adjacent and a
    // single range is cheaper to test than three.
    out.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, end) in out {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
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
        let Ok(sh_type) = r.u32(off + 4) else {
            continue;
        };
        // `.dynsym` as well as `.symtab`: a stripped shared library keeps only
        // the former, and its exported functions are the entry points to start
        // discovery from.
        if sh_type != SHT_SYMTAB && sh_type != SHT_DYNSYM {
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
            let kind = st_info & 0xf;
            if (kind != STT_FUNC && kind != STT_GNU_IFUNC) || st_value == 0 {
                continue;
            }
            out.push(Symbol {
                name: r.cstr(str_off + st_name as usize),
                value: st_value + bias,
                size: st_size,
                ifunc: kind == STT_GNU_IFUNC,
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
