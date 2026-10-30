//! ELF64 reading and placement for aarch64 guests.

use std::fmt;

// ELF constants, named as in `elf.h`.
const ELFCLASS32: u8 = 1;
const ELFCLASS64: u8 = 2;
/// `e_machine` of an i386 executable: the 32-bit program an x86-64 guest
/// kernel runs in compatibility mode.
const EM_386: u16 = 3;
const ELFDATA2LSB: u8 = 1;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
/// The guest architecture's `e_machine`.
const EM_GUEST: u16 = gex_abi::ELF_MACHINE;

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
const DT_VERSYM: i64 = 0x6fff_fff0;
const DT_VERDEF: i64 = 0x6fff_fffc;
const DT_VERDEFNUM: i64 = 0x6fff_fffd;

/// Relocation types, for whichever guest architecture this build targets.
/// The loader only ever needs the handful below; the names are the generic
/// ones both ABIs share.
pub mod reloc {
    use gex_abi::X86;
    pub const ABS64: u32 = if X86 { 1 } else { 257 };
    pub const COPY: u32 = if X86 { 5 } else { 1024 };
    pub const GLOB_DAT: u32 = if X86 { 6 } else { 1025 };
    pub const JUMP_SLOT: u32 = if X86 { 7 } else { 1026 };
    pub const RELATIVE: u32 = if X86 { 8 } else { 1027 };
    pub const TLS_DTPMOD64: u32 = if X86 { 16 } else { 1028 };
    pub const TLS_DTPREL64: u32 = if X86 { 17 } else { 1029 };
    pub const TLS_TPREL64: u32 = if X86 { 18 } else { 1030 };
    pub const TLSDESC: u32 = if X86 { 36 } else { 1031 };
    pub const IRELATIVE: u32 = if X86 { 37 } else { 1032 };
}
use reloc::{GLOB_DAT as R_AARCH64_GLOB_DAT, IRELATIVE as R_AARCH64_IRELATIVE,
    JUMP_SLOT as R_AARCH64_JUMP_SLOT, RELATIVE as R_AARCH64_RELATIVE};

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
    /// Two independently placed ELF images claim the same guest addresses.
    ImageOverlap {
        first_start: u64,
        first_end: u64,
        second_start: u64,
        second_end: u64,
    },
    /// A segment lies in the runtime's own low region: the thread contexts
    /// and the dispatch directory, below [`gex_abi::DISPATCH_DIR_END`]. A
    /// fixed-address image there would overwrite them.
    InRuntimeRegion {
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
            LoadError::NotAarch64 => write!(f, "not an {} ELF file", gex_abi::ARCH_NAME),
            LoadError::UnsupportedType(t) => write!(f, "unsupported ELF type {t}"),
            LoadError::NeedsInterpreter(p) => {
                write!(f, "could not load the interpreter {p}")
            }
            LoadError::SegmentOutOfRange { vaddr, size } => write!(
                f,
                "segment at 0x{vaddr:x} (0x{size:x} bytes) does not fit in the guest address space"
            ),
            LoadError::ImageOverlap {
                first_start,
                first_end,
                second_start,
                second_end,
            } => write!(
                f,
                "ELF mappings overlap: 0x{first_start:x}..0x{first_end:x} and \
                 0x{second_start:x}..0x{second_end:x}"
            ),
            LoadError::InRuntimeRegion { vaddr, size } => write!(
                f,
                "segment at 0x{vaddr:x} (0x{size:x} bytes) lies in gex's reserved low region \
                 (below 0x{:x}: thread contexts and the dispatch directory); relink the \
                 program higher, e.g. with -Wl,--image-base=0x{:x}",
                gex_abi::DISPATCH_DIR_END,
                gex_abi::PIE_BASE
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

impl Segment {
    /// The byte at guest address `a`, if the mapping covers it: past
    /// `bytes` (`.bss`, or a PE section's trailing zeros, which
    /// `PeImage::segments` does not store) it is zero.
    pub fn byte(&self, a: u64) -> Option<u8> {
        let o = a.checked_sub(self.vaddr)?;
        if o >= self.mem_size.max(self.bytes.len() as u64) {
            return None;
        }
        Some(self.bytes.get(o as usize).copied().unwrap_or(0))
    }
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
    /// The version a defined symbol is exported under (`GLIBC_2.14` for
    /// `memcpy@@GLIBC_2.14`), from `DT_VERSYM`/`DT_VERDEF`; `None` for an
    /// unversioned object, an undefined symbol or the base version.
    pub version: Option<String>,
    /// Whether `version` is the symbol's default, what a link against the
    /// library binds (`name@@V`), rather than an older one kept for old
    /// binaries (`name@V`: glibc's memmove-like `memcpy@GLIBC_2.2.5`).
    pub default_version: bool,
}

impl DynSym {
    /// `name@@VERSION`, `name@VERSION` or `name`, as `nm -D` spells it.
    pub fn versioned_name(&self) -> String {
        match &self.version {
            Some(v) if self.default_version => format!("{}@@{v}", self.name),
            Some(v) => format!("{}@{v}", self.name),
            None => self.name.clone(),
        }
    }
}

/// The header a native wasm32 program's container carries 0x40 bytes below
/// its entry (`tools/wine-wasm/wasm-elf.py`; shims' `wasmprog`).
pub const WASM32_MAGIC: &[u8; 8] = b"GEXWASM\0";

/// The span `[lowest, highest)` the loaded segments of a file cover, and the
/// flags of its header, when it is a native wasm program's container: an ELF
/// of either class with [`WASM32_MAGIC`] 0x40 bytes below its entry, in a
/// loaded segment's file bytes ([`Image::is_wasm_program`], from the file
/// rather than a loaded image). The flags are the header's (offset 36) when
/// it has them, else 0. `read(offset, buf)` reads the file, answering the
/// count read. `None` for anything else, or unreadable.
pub fn wasm_program_span(read: &mut dyn FnMut(u64, &mut [u8]) -> Option<usize>) -> Option<(u64, u64, u32)> {
    let mut head = [0u8; 64];
    if read(0, &mut head) != Some(64) || &head[..4] != b"\x7fELF" {
        return None;
    }
    let e64 = match head[4] {
        1 => false,
        2 => true,
        _ => return None,
    };
    let u16_at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as u64;
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64;
    let word = |b: &[u8], o: usize| if e64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) } else { u32_at(b, o) };
    let at = word(&head, 24).wrapping_sub(0x40);
    let (phoff, phent, phnum) = if e64 {
        (word(&head, 32), u16_at(&head, 54), u16_at(&head, 56))
    } else {
        (word(&head, 28), u16_at(&head, 42), u16_at(&head, 44))
    };
    let need = if e64 { 56 } else { 32 };
    if phent < need || phnum == 0 || phnum > 64 {
        return None;
    }
    let mut ph = vec![0u8; (phent * phnum) as usize];
    if read(phoff, &mut ph) != Some(ph.len()) {
        return None;
    }
    let (mut lo, mut hi, mut header) = (u64::MAX, 0u64, None);
    for p in ph.chunks_exact(phent as usize) {
        if u32_at(p, 0) != 1 {
            continue;
        }
        let (off, vaddr, filesz, memsz) = if e64 {
            (word(p, 8), word(p, 16), word(p, 32), word(p, 40))
        } else {
            (word(p, 4), word(p, 8), word(p, 16), word(p, 20))
        };
        lo = lo.min(vaddr);
        hi = hi.max(vaddr.saturating_add(memsz));
        if header.is_none() && at >= vaddr && at.saturating_add(8) <= vaddr.saturating_add(filesz) {
            let mut h = [0u8; 40];
            let n = read(off + (at - vaddr), &mut h)?;
            if n < 8 || &h[..8] != WASM32_MAGIC {
                return None;
            }
            header = Some(if n >= 40 { u32::from_le_bytes(h[36..40].try_into().unwrap()) } else { 0 });
        }
    }
    header.map(|flags| (lo, hi, flags))
}

impl Image {
    /// Whether this is a native wasm32 program's container: an i386 image
    /// with [`WASM32_MAGIC`] 0x40 bytes below the entry.
    pub fn is_wasm32(&self) -> bool {
        self.class32 && self.is_wasm_program()
    }

    /// Whether this is a native wasm program's container of either class:
    /// [`WASM32_MAGIC`] 0x40 bytes below the entry (an ELF64 one holds an
    /// LP64 program, such as 64-bit Wine, lowered to wasm32 or not).
    pub fn is_wasm_program(&self) -> bool {
        let at = self.entry.wrapping_sub(0x40);
        self.segments.iter().any(|s| {
            at >= s.vaddr && at + 8 <= s.vaddr + s.bytes.len() as u64 && {
                let o = (at - s.vaddr) as usize;
                &s.bytes[o..o + 8] == WASM32_MAGIC
            }
        })
    }

    /// The defined, non-TLS dynamic symbols in `[lo, hi)` (unbiased) by every
    /// name a binding may use: `name@@VER`, `name@VER`, and the bare `name`
    /// for the default version (or an unversioned object), each with its
    /// unbiased value and whether it is an IFUNC. For export providers.
    pub fn exports_by_version(&self, lo: u64, hi: u64) -> std::collections::HashMap<String, (u64, bool)> {
        let mut m = std::collections::HashMap::new();
        for y in self.dynsyms.iter().filter(|y| !y.undefined && !y.tls && y.value >= lo && y.value < hi) {
            let v = (y.value, y.ifunc);
            m.insert(y.versioned_name(), v);
            if y.version.is_none() || y.default_version {
                m.insert(y.name.clone(), v);
            }
        }
        m
    }
}

/// An ELF file's GNU build-id (`NT_GNU_BUILD_ID` in a `PT_NOTE`), as
/// lower-case hex.
pub fn build_id(bytes: &[u8]) -> Option<String> {
    let rd = |o: usize, n: usize| -> Option<u64> {
        let b = bytes.get(o..o.checked_add(n)?)?;
        Some(b.iter().rev().fold(0u64, |a, &x| (a << 8) | x as u64))
    };
    if bytes.get(..4) != Some(&b"\x7fELF"[..]) || bytes.get(5) != Some(&1) {
        return None;
    }
    let c64 = bytes.get(4) == Some(&2);
    let (phoff, phentsize, phnum) = if c64 { (rd(0x20, 8)?, rd(0x36, 2)?, rd(0x38, 2)?) } else { (rd(0x1c, 4)?, rd(0x2a, 2)?, rd(0x2c, 2)?) };
    for i in 0..phnum {
        let ph = usize::try_from(phoff.checked_add(i * phentsize)?).ok()?;
        if rd(ph, 4)? != 4 {
            continue; // not PT_NOTE
        }
        let (off, size) = if c64 { (rd(ph + 8, 8)?, rd(ph + 32, 8)?) } else { (rd(ph + 4, 4)?, rd(ph + 16, 4)?) };
        let (mut p, end) = (usize::try_from(off).ok()?, usize::try_from(off.checked_add(size)?).ok()?);
        while p + 12 <= end {
            let (namesz, descsz, ty) = (rd(p, 4)? as usize, rd(p + 4, 4)? as usize, rd(p + 8, 4)?);
            let name = p + 12;
            let desc = name + namesz.div_ceil(4) * 4;
            if ty == 3 && bytes.get(name..name + namesz) == Some(&b"GNU\0"[..]) {
                return Some(bytes.get(desc..desc + descsz)?.iter().map(|b| format!("{b:02x}")).collect());
            }
            p = desc + descsz.div_ceil(4) * 4;
        }
    }
    None
}

/// An image holding only the numbers the auxiliary vector reports, for a
/// process that is restored rather than loaded.
pub fn headers_only(phdr_vaddr: u64, phentsize: u64, phnum: u64, entry: u64) -> Image {
    Image {
        bias: 0,
        class32: false,
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
    /// A 32-bit (ELFCLASS32, i386) image: its code is decoded in
    /// compatibility mode and its initial stack laid out with 4-byte words.
    pub class32: bool,
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
        let b = [b[0], b[1]];
        Ok(u16::from_le_bytes(b))
    }
    fn u32(&self, off: usize) -> Result<u32, LoadError> {
        let b = self.data.get(off..off + 4).ok_or(LoadError::TooShort)?;
        let b = [b[0], b[1], b[2], b[3]];
        Ok(u32::from_le_bytes(b))
    }
    fn u64(&self, off: usize) -> Result<u64, LoadError> {
        let b = self.data.get(off..off + 8).ok_or(LoadError::TooShort)?;
        let b = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
        Ok(u64::from_le_bytes(b))
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

/// What an ELF file's headers say, before any segment's bytes are read:
/// everything [`load`] and [`plan`] share.
struct Headers {
    class32: bool,
    pie: bool,
    bias: u64,
    entry: u64,
    e_phoff: usize,
    e_shoff: usize,
    e_phentsize: usize,
    e_phnum: usize,
    e_shentsize: usize,
    e_shnum: usize,
    /// Each `PT_LOAD`, in order: the segment with no bytes yet, and the file
    /// range its bytes come from, as `(offset, length)`.
    loads: Vec<(Segment, (usize, usize))>,
    brk: u64,
    text_lo: u64,
    text_hi: u64,
    phdr_vaddr: u64,
    dynamic: Option<(u64, u64)>,
    /// `PT_INTERP`'s file range: the string is read by the caller, since a
    /// planner holds only the head of the file.
    interp: Option<(usize, usize)>,
    tls: Option<(u64, u64, u64, u64)>,
}

/// Read and check the ELF and program headers in `r`.
fn headers(r: &Reader, pie_base: u64) -> Result<Headers, LoadError> {
    let data = r.data;
    if data.len() < 64 {
        return Err(LoadError::TooShort);
    }
    if &data[0..4] != b"\x7fELF" {
        return Err(LoadError::NotElf);
    }
    if r.u8(5)? != ELFDATA2LSB {
        return Err(LoadError::NotElf64Le);
    }
    // A 32-bit i386 image runs in the same process model as everything
    // else, on the i386 ABI: on the x86-64 guest any i386 program, in
    // compatibility mode; on the AArch64 guest only a native wasm32 program
    // (an i386 container around a wasm module, checked by the caller), which
    // has no i386 code to translate.
    let class32 = match r.u8(4)? {
        ELFCLASS64 => false,
        ELFCLASS32 => true,
        _ => return Err(LoadError::NotElf64Le),
    };
    let e_type = r.u16(16)?;
    let machine = r.u16(18)?;
    let want = if class32 {
        EM_386
    } else {
        EM_GUEST
    };
    if machine != want {
        return Err(LoadError::NotAarch64);
    }
    let pie = match e_type {
        ET_EXEC => false,
        ET_DYN => true,
        other => return Err(LoadError::UnsupportedType(other)),
    };
    let bias = if pie { pie_base } else { 0 };

    // The two header layouts differ only in the width of their address and
    // offset fields.
    let (e_entry, e_phoff, e_shoff, e_phentsize, e_phnum, e_shentsize, e_shnum) = if class32 {
        (
            r.u32(24)? as u64,
            r.u32(28)? as usize,
            r.u32(32)? as usize,
            r.u16(42)? as usize,
            r.u16(44)? as usize,
            r.u16(46)? as usize,
            r.u16(48)? as usize,
        )
    } else {
        (
            r.u64(24)?,
            r.u64(32)? as usize,
            r.u64(40)? as usize,
            r.u16(54)? as usize,
            r.u16(56)? as usize,
            r.u16(58)? as usize,
            r.u16(60)? as usize,
        )
    };

    let mut h = Headers {
        class32,
        pie,
        bias,
        entry: e_entry + bias,
        e_phoff,
        e_shoff,
        e_phentsize,
        e_phnum,
        e_shentsize,
        e_shnum,
        loads: Vec::new(),
        brk: 0,
        text_lo: u64::MAX,
        text_hi: 0,
        phdr_vaddr: 0,
        dynamic: None,
        interp: None,
        tls: None,
    };

    for i in 0..e_phnum {
        let off = e_phoff + i * e_phentsize;
        let p_type = r.u32(off)?;
        // Elf32_Phdr: type, offset, vaddr, paddr, filesz, memsz, flags, align.
        // Elf64_Phdr: type, flags, offset, vaddr, paddr, filesz, memsz, align.
        let (p_flags, p_offset, p_vaddr, p_filesz, p_memsz, p_align) = if class32 {
            (
                r.u32(off + 24)?,
                r.u32(off + 4)? as usize,
                r.u32(off + 8)? as u64,
                r.u32(off + 16)? as u64,
                r.u32(off + 20)? as u64,
                r.u32(off + 28)? as u64,
            )
        } else {
            (
                r.u32(off + 4)?,
                r.u64(off + 8)? as usize,
                r.u64(off + 16)?,
                r.u64(off + 32)?,
                r.u64(off + 40)?,
                r.u64(off + 48)?,
            )
        };

        match p_type {
            PT_INTERP => h.interp = Some((p_offset, p_filesz as usize)),
            PT_DYNAMIC => h.dynamic = Some((p_vaddr + bias, p_memsz)),
            PT_TLS => h.tls = Some((p_vaddr + bias, p_filesz, p_memsz, p_align)),
            PT_LOAD => {
                let vaddr = p_vaddr + bias;
                if vaddr.checked_add(p_memsz).is_none() || vaddr + p_memsz >= gex_abi::GUEST_LIMIT {
                    return Err(LoadError::SegmentOutOfRange {
                        vaddr,
                        size: p_memsz,
                    });
                }
                // lld links a static i386 executable at 0x10000 by default,
                // which is where gex keeps its contexts and dispatch
                // directory. Refuse it rather than load over them. Only a
                // fixed-address image: a relocatable one loaded at a low base
                // is being measured or parsed (`closure::resolve` sizes every
                // library at base 0), and where it really goes is the
                // loader's or the kernel's placement, which never picks there.
                if !pie && p_memsz != 0 && vaddr < gex_abi::DISPATCH_DIR_END {
                    return Err(LoadError::InRuntimeRegion {
                        vaddr,
                        size: p_memsz,
                    });
                }
                let executable = p_flags & PF_X != 0;
                if executable {
                    h.text_lo = h.text_lo.min(vaddr);
                    h.text_hi = h.text_hi.max(vaddr + p_memsz);
                }
                h.brk = h.brk.max(vaddr + p_memsz);
                h.loads.push((
                    Segment {
                        vaddr,
                        bytes: Vec::new(),
                        mem_size: p_memsz,
                        readable: p_flags & PF_R != 0,
                        writable: p_flags & PF_W != 0,
                        executable,
                    },
                    (p_offset, p_filesz as usize),
                ));
                // The program headers are conventionally inside the first
                // PT_LOAD; locate them by file offset.
                if (p_offset as u64) <= e_phoff as u64
                    && (e_phoff as u64) < p_offset as u64 + p_filesz
                {
                    h.phdr_vaddr = vaddr + (e_phoff as u64 - p_offset as u64);
                }
            }
            _ => {}
        }
    }

    if h.loads.is_empty() {
        return Err(LoadError::Malformed("no PT_LOAD segments"));
    }
    if h.text_lo == u64::MAX {
        return Err(LoadError::Malformed("no executable segment"));
    }
    Ok(h)
}

impl Headers {
    /// The image these headers describe, its segments as given.
    fn image(&self, segments: Vec<Segment>, interp: Option<String>) -> Image {
        Image {
            bias: self.bias,
            class32: self.class32,
            entry: self.entry,
            segments,
            phdr_vaddr: self.phdr_vaddr,
            phentsize: self.e_phentsize as u64,
            phnum: self.e_phnum as u64,
            // brk starts on the next page boundary above the image.
            brk: (self.brk + 0xfff) & !0xfff,
            text_range: (self.text_lo, self.text_hi),
            tls: self.tls,
            symbols: Vec::new(),
            named_relocs: Vec::new(),
            relocs: Vec::new(),
            init_array: Vec::new(),
            dynsyms: Vec::new(),
            irelative: Vec::new(),
            code: Vec::new(),
            pie: self.pie,
            interp,
            soname: None,
            needed: Vec::new(),
            runpath: Vec::new(),
        }
    }
}

/// Where each of an image's segments comes from in its file, as
/// `(offset, length)`, in [`Image::segments`] order: [`plan`]'s answer.
pub type FileRanges = Vec<(u64, u64)>;

/// What `execve` needs of an ELF file, and only that: its program headers,
/// read through `read_at` (fill the buffer from a file offset, answering how
/// many bytes it read). The segments come back with no bytes, each paired
/// with the file range they are to be read from -- straight into guest
/// memory, by the caller -- so the file is never read whole and nothing is
/// copied twice. Nothing past the program headers is parsed either: no
/// sections, symbols or dynamic table, which a process's own loader reads.
/// And no relocation is applied, as on Linux: `ld.so` and a static PIE
/// relocate themselves.
pub fn plan(
    read_at: &dyn Fn(u64, &mut [u8]) -> Option<usize>,
    pie_base: u64,
) -> Result<(Image, FileRanges), LoadError> {
    let read = |off: u64, len: usize| -> Result<Vec<u8>, LoadError> {
        let mut v = vec![0u8; len];
        let n = read_at(off, &mut v).ok_or(LoadError::TooShort)?;
        v.truncate(n);
        Ok(v)
    };
    // The headers are almost always in the first page; a file whose program
    // headers lie further in is read again, that far.
    let mut head = read(0, 4096)?;
    let need = {
        let r = Reader { data: &head };
        let class32 = r.u8(4)? == ELFCLASS32;
        let (phoff, phentsize, phnum) = if class32 {
            (r.u32(28)? as usize, r.u16(42)? as usize, r.u16(44)? as usize)
        } else {
            (r.u64(32)? as usize, r.u16(54)? as usize, r.u16(56)? as usize)
        };
        phoff.saturating_add(phentsize.saturating_mul(phnum))
    };
    if need > head.len() && need <= 1 << 20 {
        head = read(0, need)?;
    }
    let r = Reader { data: &head };
    let h = headers(&r, pie_base)?;
    let interp = match h.interp {
        Some((off, len)) => {
            let b = read(off as u64, len.min(4096))?;
            let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
            Some(String::from_utf8_lossy(&b[..end]).into_owned())
        }
        None => None,
    };
    let ranges = h.loads.iter().map(|(_, (o, l))| (*o as u64, *l as u64)).collect::<FileRanges>();
    let image = h.image(h.loads.iter().map(|(s, _)| s.clone()).collect(), interp);
    // An i386 image on a guest that runs no i386 code is only acceptable as
    // a native wasm32 program's container: its magic, 0x40 below the entry.
    if image.class32 && !gex_abi::X86 {
        let at = image.entry.wrapping_sub(0x40);
        let magic = image.segments.iter().zip(&ranges).find_map(|(s, (o, l))| {
            (at >= s.vaddr && at + 8 <= s.vaddr + l).then(|| read(o + (at - s.vaddr), 8).ok())?
        });
        if magic.as_deref() != Some(&WASM32_MAGIC[..]) {
            return Err(LoadError::NotAarch64);
        }
    }
    Ok((image, ranges))
}

/// Parse and place an aarch64 ELF image.
///
/// `pie_base` is the load bias used for `ET_DYN` images; `ET_EXEC` images are
/// placed at their own `p_vaddr`s.
pub fn load(data: &[u8], pie_base: u64) -> Result<Image, LoadError> {
    let r = Reader { data };
    let h = headers(&r, pie_base)?;
    let interp = h.interp.map(|(off, _)| r.cstr(off));
    let mut segments = Vec::with_capacity(h.loads.len());
    for (seg, (off, len)) in &h.loads {
        let mut seg = seg.clone();
        seg.bytes = r.slice(*off, *len)?.to_vec();
        segments.push(seg);
    }
    let (class32, bias, dynamic) = (h.class32, h.bias, h.dynamic);
    let (e_shoff, e_shentsize, e_shnum) = (h.e_shoff, h.e_shentsize, h.e_shnum);
    let mut image = h.image(segments, interp);

    if class32 && !gex_abi::X86 && !image.is_wasm32() {
        return Err(LoadError::NotAarch64);
    }

    if let (Some((dyn_vaddr, dyn_size)), false) = (dynamic, class32) {
        apply_relative_relocations(&mut image, dyn_vaddr, dyn_size);
        image.named_relocs = read_named_relocs(&image, dyn_vaddr, dyn_size);
        let (syms, relocs, inits) = read_dynamic(&image, dyn_vaddr, dyn_size);
        image.dynsyms = syms;
        image.relocs = relocs;
        image.init_array = inits;
        read_needed(&mut image, dyn_vaddr, dyn_size);
    }
    if let (Some((dyn_vaddr, dyn_size)), true) = (dynamic, class32) {
        image.dynsyms = read_dynsyms32(&image, dyn_vaddr, dyn_size);
    }
    // The dynamic section and the section tables are read in their 64-bit
    // layouts; a 32-bit image keeps only what its program headers say. Its
    // programs are static (or Wine's, whose PE loader is not ours), so no
    // relocation or symbol is lost that anything here would use.
    if !class32 {
        image.symbols = read_symbols(&r, e_shoff, e_shentsize, e_shnum, bias);
        image.code = read_code_sections(&r, e_shoff, e_shentsize, e_shnum, bias);
    } else {
        // i386: the function symbols, for function recovery (the function
        // tier) and names. The code sections stay unread, so what tier-1
        // discovers is unchanged.
        image.symbols = read_symbols32(&r, e_shoff, e_shentsize, e_shnum, bias);
    }
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

/// The dynamic symbol table at `symtab` (entries of `syment` bytes, names
/// in `strtab`), with each defined symbol's version from `.gnu.version`
/// (`versym`) and `.gnu.version_d` (`verdef`, `verdefnum` entries).
#[allow(clippy::too_many_arguments)]
fn read_dynsym_table(image: &Image, symtab: u64, strtab: u64, syment: u64, versym: Option<u64>, verdef: Option<u64>, verdefnum: u64, class32: bool) -> Vec<DynSym> {
    // The version definitions (`.gnu.version_d`): index -> name. Each
    // Elf_Verdef is {vd_version u16, vd_flags u16, vd_ndx u16, vd_cnt u16,
    // vd_hash u32, vd_aux u32, vd_next u32}; its first Elf_Verdaux
    // {vda_name u32, vda_next u32} names it.
    let mut versions = std::collections::HashMap::new();
    if let Some(mut at) = verdef {
        for _ in 0..verdefnum.min(1 << 16) {
            let (Some(ndx), Some(aux), Some(next)) =
                (read_u16(image, at + 4), read_u32(image, at + 12), read_u32(image, at + 16))
            else {
                break;
            };
            if let Some(name) = read_u32(image, at + aux as u64) {
                versions.insert(ndx, read_cstr(image, strtab + name as u64));
            }
            if next == 0 {
                break;
            }
            at += next as u64;
        }
    }

    // How many symbols there are is not written down anywhere: the table runs
    // to the end of `.dynstr`, which conventionally follows it. Reading until
    // the entry stops making sense is what every loader does.
    const STT_GNU_IFUNC: u8 = 10;
    const STT_TLS: u8 = 6;
    const STB_WEAK: u8 = 2;
    let mut syms = Vec::new();
    let limit = if strtab > symtab { (strtab - symtab) / syment } else { 4096 };
    for i in 0..limit {
        let at = symtab + i * syment;
        // Elf64_Sym {name u32, info u8, other u8, shndx u16, value u64,
        // size u64}; Elf32_Sym {name u32, value u32, size u32, info u8,
        // other u8, shndx u16}.
        let (st_name, st_info, st_shndx, value, size) = if class32 {
            let (Some(n), Some(v), Some(z), Some(i), Some(x)) =
                (read_u32(image, at), read_u32(image, at + 4), read_u32(image, at + 8), read_u8(image, at + 12), read_u16(image, at + 14))
            else {
                break;
            };
            (n, i, x, v as u64, z as u64)
        } else {
            let (Some(n), Some(i), Some(x), Some(v), Some(z)) =
                (read_u32(image, at), read_u8(image, at + 4), read_u16(image, at + 6), read_u64(image, at + 8), read_u64(image, at + 16))
            else {
                break;
            };
            (n, i, x, v, z)
        };
        let kind = st_info & 0xf;
        // Its version: `.gnu.version` holds an index per symbol (bit 15:
        // hidden, i.e. not the default); 0 and 1 are local and the base.
        let (version, default_version) = match versym.and_then(|v| read_u16(image, v + i * 2)) {
            Some(x) if st_shndx != 0 && (x & 0x7fff) > 1 => {
                (versions.get(&(x & 0x7fff)).cloned(), x & 0x8000 == 0)
            }
            _ => (None, false),
        };
        syms.push(DynSym {
            name: read_cstr(image, strtab + st_name as u64),
            value,
            size,
            undefined: st_shndx == 0,
            weak: (st_info >> 4) == STB_WEAK,
            ifunc: kind == STT_GNU_IFUNC,
            tls: kind == STT_TLS,
            version,
            default_version,
        });
    }
    syms
}

/// [`read_dynamic`]'s symbol table for a 32-bit image (Elf32_Dyn entries):
/// only the symbols, which is what export providers bind by; 32-bit images
/// are not linked here.
fn read_dynsyms32(image: &Image, dyn_vaddr: u64, dyn_size: u64) -> Vec<DynSym> {
    let (mut symtab, mut strtab, mut syment) = (None, None, 16u64);
    let (mut versym, mut verdef, mut verdefnum) = (None, None, 0u64);
    for i in 0..dyn_size / 8 {
        let at = dyn_vaddr + i * 8;
        let (Some(tag), Some(val)) = (read_u32(image, at), read_u32(image, at + 4)) else {
            break;
        };
        let val = val as u64;
        match tag as i32 as i64 {
            DT_NULL => break,
            DT_SYMTAB => symtab = Some(val + image.bias),
            DT_STRTAB => strtab = Some(val + image.bias),
            DT_SYMENT => syment = val,
            DT_VERSYM => versym = Some(val + image.bias),
            DT_VERDEF => verdef = Some(val + image.bias),
            DT_VERDEFNUM => verdefnum = val,
            _ => {}
        }
    }
    match (symtab, strtab) {
        (Some(symtab), Some(strtab)) if syment != 0 => read_dynsym_table(image, symtab, strtab, syment, versym, verdef, verdefnum, true),
        _ => Vec::new(),
    }
}

/// Everything a linker needs out of the dynamic section: the symbol table,
/// every relocation with its type and addend, and the initialisers.
/// ([`read_named_relocs`] answers a narrower question for code discovery.)
pub(crate) fn read_dynamic(image: &Image, dyn_vaddr: u64, dyn_size: u64)
    -> (Vec<DynSym>, Vec<Reloc>, Vec<u64>) {
    let (mut rela, mut relasz, mut relaent) = (None, 0u64, 24u64);
    let (mut jmprel, mut pltrelsz) = (None, 0u64);
    let (mut symtab, mut strtab, mut syment) = (None, None, 24u64);
    let (mut init_array, mut init_arraysz, mut init) = (None, 0u64, None);
    let (mut versym, mut verdef, mut verdefnum) = (None, None, 0u64);
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
            DT_VERSYM => versym = Some(val + image.bias),
            DT_VERDEF => verdef = Some(val + image.bias),
            DT_VERDEFNUM => verdefnum = val,
            _ => {}
        }
    }
    let (Some(symtab), Some(strtab)) = (symtab, strtab) else {
        return (Vec::new(), Vec::new(), Vec::new());
    };
    if syment == 0 || relaent < 24 {
        return (Vec::new(), Vec::new(), Vec::new());
    }

    let syms = read_dynsym_table(image, symtab, strtab, syment, versym, verdef, verdefnum, false);

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
/// deferred to startup. Applying `RELATIVE` twice is harmless (it
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


/// [`read_symbols`] for an `ELFCLASS32` image (`Elf32_Shdr`, `Elf32_Sym`).
fn read_symbols32(
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
        if sh_type != SHT_SYMTAB && sh_type != SHT_DYNSYM {
            continue;
        }
        let (Ok(sh_offset), Ok(sh_size), Ok(sh_link), Ok(sh_entsize)) =
            (r.u32(off + 16), r.u32(off + 20), r.u32(off + 24), r.u32(off + 36))
        else {
            continue;
        };
        if sh_entsize == 0 {
            continue;
        }
        let str_off = r.u32(e_shoff + sh_link as usize * e_shentsize + 16).unwrap_or(0) as usize;
        for k in 0..(sh_size / sh_entsize) as usize {
            let so = sh_offset as usize + k * sh_entsize as usize;
            let (Ok(st_name), Ok(st_value), Ok(st_size), Ok(st_info)) =
                (r.u32(so), r.u32(so + 4), r.u32(so + 8), r.u8(so + 12))
            else {
                continue;
            };
            let kind = st_info & 0xf;
            if (kind != STT_FUNC && kind != STT_GNU_IFUNC) || st_value == 0 {
                continue;
            }
            out.push(Symbol {
                name: r.cstr(str_off + st_name as usize),
                value: st_value as u64 + bias,
                size: st_size as u64,
                ifunc: kind == STT_GNU_IFUNC,
            });
        }
    }
    out
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

/// A minimal image for tests: one R+X `PT_LOAD` of 0x1000 bytes at `vaddr`.
#[cfg(test)]
pub(crate) fn test_image(e_type: u16, vaddr: u64) -> Vec<u8> {
    let mut v = vec![0u8; 0x1000];
    v[0..4].copy_from_slice(b"\x7fELF");
    v[4] = 2; // ELFCLASS64
    v[5] = 1; // little endian
    v[6] = 1;
    v[16..18].copy_from_slice(&e_type.to_le_bytes());
    v[18..20].copy_from_slice(&gex_abi::ELF_MACHINE.to_le_bytes());
    v[24..32].copy_from_slice(&vaddr.to_le_bytes()); // e_entry
    v[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    v[54..56].copy_from_slice(&56u16.to_le_bytes());
    v[56..58].copy_from_slice(&1u16.to_le_bytes());
    let p = 64;
    v[p..p + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    v[p + 4..p + 8].copy_from_slice(&5u32.to_le_bytes()); // PF_R | PF_X
    v[p + 16..p + 24].copy_from_slice(&vaddr.to_le_bytes());
    v[p + 32..p + 40].copy_from_slice(&0x1000u64.to_le_bytes());
    v[p + 40..p + 48].copy_from_slice(&0x1000u64.to_le_bytes());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_elf() {
        assert_eq!(load(&[0; 64], 0).unwrap_err(), LoadError::NotElf);
        assert_eq!(load(&[0; 8], 0).unwrap_err(), LoadError::TooShort);
    }

    /// An `ET_EXEC` with one executable `PT_LOAD` at `vaddr`.
    fn exec_at(vaddr: u64) -> Vec<u8> {
        test_image(ET_EXEC, vaddr)
    }

    /// A relocatable object linked at zero, as every shared library is, can
    /// be loaded at zero to measure it: nothing is placed there.
    #[test]
    fn a_relocatable_image_loads_at_any_base() {
        let so = test_image(ET_DYN, 0);
        assert_eq!(load(&so, 0).unwrap().brk, 0x1000);
        assert_eq!(load(&so, gex_abi::MMAP_BOTTOM).unwrap().bias, gex_abi::MMAP_BOTTOM);
    }

    /// lld's default base for a static i386 executable is 0x10000, inside
    /// the contexts and dispatch directory: refused, not loaded over them.
    #[test]
    fn a_fixed_image_in_the_runtime_region_is_refused() {
        assert_eq!(
            load(&exec_at(0x10000), 0).unwrap_err(),
            LoadError::InRuntimeRegion { vaddr: 0x10000, size: 0x1000 }
        );
        assert!(load(&exec_at(gex_abi::DISPATCH_DIR_END - 0x1000), 0).is_err());
        assert!(load(&exec_at(gex_abi::EXEC_BASE), 0).is_ok());
    }

    /// glibc exports `memcpy` twice: the default `memcpy@@GLIBC_2.14` and
    /// the memmove-like `memcpy@GLIBC_2.2.5` old binaries bound. Read from
    /// the host's x86-64 libc when it has one.
    #[test]
    fn versioned_dynamic_symbols() {
        let Ok(bytes) = std::fs::read("/lib/x86_64-linux-gnu/libc.so.6") else { return };
        // An x86-64 image loads only in an x86 build.
        if !gex_abi::X86 {
            return;
        }
        let image = load(&bytes, 0).unwrap();
        let memcpy: Vec<String> =
            image.dynsyms.iter().filter(|y| y.name == "memcpy").map(DynSym::versioned_name).collect();
        assert!(memcpy.contains(&"memcpy@@GLIBC_2.14".to_string()), "{memcpy:?}");
        assert!(memcpy.contains(&"memcpy@GLIBC_2.2.5".to_string()), "{memcpy:?}");
        let malloc = image.dynsyms.iter().find(|y| y.name == "malloc").unwrap();
        assert_eq!(malloc.versioned_name(), "malloc@@GLIBC_2.2.5");
        assert!(image.dynsyms.iter().any(|y| y.name == "strlen" && y.ifunc));
    }

    /// The same for an i386 libc (Elf32_Dyn and Elf32_Sym), when the host
    /// has one: `fopen` has two versions there.
    #[test]
    fn versioned_dynamic_symbols_32() {
        let Ok(bytes) = std::fs::read("/usr/lib/i386-linux-gnu/libc.so.6") else { return };
        if !gex_abi::X86 {
            return;
        }
        let image = load(&bytes, 0).unwrap();
        assert!(image.class32);
        let fopen: Vec<String> =
            image.dynsyms.iter().filter(|y| y.name == "fopen").map(DynSym::versioned_name).collect();
        assert!(fopen.contains(&"fopen@@GLIBC_2.1".to_string()), "{fopen:?}");
        assert!(fopen.contains(&"fopen@GLIBC_2.0".to_string()), "{fopen:?}");
        let malloc = image.dynsyms.iter().find(|y| y.name == "malloc").unwrap();
        assert_eq!(malloc.versioned_name(), "malloc@@GLIBC_2.0");
        assert!(malloc.value != 0 && malloc.size != 0);
        assert!(image.dynsyms.iter().any(|y| y.name == "strlen" && y.ifunc));
    }
}
