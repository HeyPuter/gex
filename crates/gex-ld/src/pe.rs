//! PE32 and PE32+ images: parsing, placement, base relocation, and binding a
//! program's imports across its DLL closure.
//!
//! Two layers:
//!
//! - [`parse`] reads one image into a [`PeImage`]: headers, sections, and
//!   every data directory gex cares about (base relocations, imports, bound
//!   imports, delay imports, exports and forwarders, TLS, the resource index
//!   and the `.pdata` exception table). Nothing is placed yet; addresses are
//!   RVAs, or VAs at the image's *preferred* base where the format stores VAs.
//! - [`load_closure`] follows a program's imports (plus forwarder targets and
//!   delay-loaded DLLs) through the executable's directory and the configured
//!   search directories, places every image under [`PeLayout`]'s rules,
//!   applies base relocations, and fills every IAT slot, either with the
//!   address of an export in another loaded image or with a **placeholder**.
//!
//! # Placement rules
//!
//! Guest code has to live where the dispatch directory can name it, so an
//! image must fit in `[EXEC_BASE, MMAP_TOP)` (0x40_0000 to 0x8000_0000) and
//! stay clear of the Linux-layout stack (`STACK_TOP - STACK_SIZE ..
//! STACK_TOP`). An image whose preferred base satisfies that, and does not
//! collide with an image already placed, is mapped there unchanged. Any other
//! image is rebased to the lowest 64 KiB-aligned gap at or above `EXEC_BASE`;
//! that includes every PE32+ image with a high preferred base (MinGW and
//! MSVC link x64 executables at 0x1_4000_0000). An image that must move but
//! cannot (`IMAGE_FILE_RELOCS_STRIPPED`, or no `.reloc` directory and no
//! `DYNAMIC_BASE` flag; see [`PeImage::relocatable`]) is refused, naming its
//! preferred base.
//!
//! [`PeLayout::pack`] (the default) rebases every relocatable image into that
//! low packing even when its preferred base would fit. Linear memory has to
//! exist up to the highest touched address, so a DLL that prefers 0x6xxx_xxxx
//! would cost every process ~1.7 GiB; packing avoids that at the price of
//! relocating. `pack: false` honours
//! preferred bases that fit.
//!
//! # Placeholder imports
//!
//! System DLLs ([`is_system_dll`]: kernel32, ntdll, user32, msvcrt, the
//! `api-ms-win-*` API sets, ...) are never loaded from disk, so an import
//! from one cannot be bound to code here. Each distinct
//! `(dll, name-or-ordinal)` pair instead gets one entry in a
//! [`PlaceholderTable`], and every IAT slot importing it is filled with that
//! entry's address. The same happens, with a different [`PlaceholderReason`],
//! for an import whose (non-system) DLL was not found, or whose DLL does not
//! export the symbol.
//!
//! The format:
//!
//! - **Range:** `[PLACEHOLDER_BASE, PLACEHOLDER_END)` = `[0x30_0000,
//!   0x40_0000)`, the 1 MiB between the end of the dispatch directory
//!   (`gex_abi::DISPATCH_DIR_END`) and `EXEC_BASE`. No image, stack or mmap
//!   ever lands there, and nothing is translated there, so a jump into it
//!   reaches the host's dispatch-miss path the same way the kernel's
//!   `SIGRETURN_TRAMPOLINE` does.
//! - **Entries:** entry `i` is at `PLACEHOLDER_BASE + i * PLACEHOLDER_STRIDE`
//!   (16 bytes each, so at most 65,536). 16 bytes is room for a small stub;
//!   each entry also
//!   owns its own dispatch slot on both guest ISAs.
//! - **Table:** [`PlaceholderTable::entries`], indexed by `i`: the DLL name
//!   (lower-cased, `.dll` appended when an import or forwarder omitted it),
//!   the import by name (with its hint) or by ordinal, why it is a
//!   placeholder, the forwarder chain that led there if any, and every slot
//!   holding it as `(module index, slot VA, delay?)`. Delay-import slots are
//!   recorded but never written (their thunks stay in place; see below).
//!   [`PlaceholderTable::lookup`] maps an address back to its entry and
//!   [`Placeholder::label`] gives the `DLL!name` / `DLL!#ordinal` string a
//!   loud failure should print.
//! - **Executing one:** a placeholder is never bound here, so executing
//!   one must fail with the entry's label. A *data* import read through a placeholder reads the
//!   (unbacked) placeholder range, not the data.
//!
//! # Delay imports
//!
//! Delay-import slots keep the linker's thunks (relocated like any other
//! pointer), because the helper (`__delayLoadHelper2`) binds them at run time
//! through `LoadLibrary`/`GetProcAddress`. The loader still reports, for each
//! one, where it *would* resolve: a loaded image's export or a placeholder.
//! Delay-loaded DLLs that are found on disk join the closure, marked
//! [`Reason::Delay`], so a run-time `LoadLibrary` finds them translated.

use crate::elf::Segment;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// `IMAGE_FILE_MACHINE_I386`.
pub const MACHINE_I386: u16 = 0x14c;
/// `IMAGE_FILE_MACHINE_AMD64`.
pub const MACHINE_AMD64: u16 = 0x8664;
/// `IMAGE_FILE_MACHINE_ARM64`.
pub const MACHINE_ARM64: u16 = 0xaa64;

/// `IMAGE_FILE_RELOCS_STRIPPED`.
pub const FILE_RELOCS_STRIPPED: u16 = 0x0001;
/// `IMAGE_FILE_DLL`.
pub const FILE_DLL: u16 = 0x2000;

/// `IMAGE_DLLCHARACTERISTICS_DYNAMIC_BASE`.
pub const DLLCHAR_DYNAMIC_BASE: u16 = 0x0040;

/// Data-directory indices.
pub mod dir {
    pub const EXPORT: usize = 0;
    pub const IMPORT: usize = 1;
    pub const RESOURCE: usize = 2;
    pub const EXCEPTION: usize = 3;
    pub const SECURITY: usize = 4;
    pub const BASERELOC: usize = 5;
    pub const TLS: usize = 9;
    pub const LOAD_CONFIG: usize = 10;
    pub const BOUND_IMPORT: usize = 11;
    pub const IAT: usize = 12;
    pub const DELAY_IMPORT: usize = 13;
    pub const CLR: usize = 14;
}

/// Base-relocation types.
pub mod rel {
    pub const ABSOLUTE: u8 = 0;
    pub const HIGHLOW: u8 = 3;
    pub const DIR64: u8 = 10;
}

/// Section characteristics.
pub const SCN_MEM_EXECUTE: u32 = 0x2000_0000;
pub const SCN_MEM_READ: u32 = 0x4000_0000;
pub const SCN_MEM_WRITE: u32 = 0x8000_0000;
pub const SCN_CNT_CODE: u32 = 0x0000_0020;

/// Where placeholder imports live. See the module documentation.
pub const PLACEHOLDER_BASE: u64 = 0x0030_0000;
/// One past the placeholder range.
pub const PLACEHOLDER_END: u64 = gex_abi::EXEC_BASE;
/// Bytes per placeholder entry.
pub const PLACEHOLDER_STRIDE: u64 = 16;
/// How many placeholders the range holds.
pub const PLACEHOLDER_CAPACITY: u64 = (PLACEHOLDER_END - PLACEHOLDER_BASE) / PLACEHOLDER_STRIDE;

const _: () = {
    // The range sits in the gap between the dispatch directory and the
    // lowest image, and inside the directory's reach.
    assert!(PLACEHOLDER_BASE >= gex_abi::DISPATCH_DIR_END);
    assert!(PLACEHOLDER_END <= gex_abi::EXEC_BASE);
    assert!(PLACEHOLDER_END <= gex_abi::MMAP_TOP);
    assert!(PLACEHOLDER_STRIDE >= gex_abi::INST_ALIGN);
};

/// Windows' allocation granularity, which is what rebased images align to.
pub const IMAGE_ALIGN: u64 = 0x10000;

const PAGE: u64 = 4096;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// What kind of failure a [`PeError`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeErrorKind {
    /// Not a PE image, or one whose structures contradict themselves.
    Malformed,
    /// A real PE feature gex does not handle. The message names it.
    Unsupported,
    /// The image cannot be placed under gex's address-space rules.
    Layout,
}

/// Why a PE image or closure could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeError {
    pub kind: PeErrorKind,
    pub msg: String,
    /// The file it concerns, when known.
    pub file: Option<String>,
}

impl PeError {
    fn malformed(msg: impl Into<String>) -> Self {
        PeError { kind: PeErrorKind::Malformed, msg: msg.into(), file: None }
    }
    fn unsupported(msg: impl Into<String>) -> Self {
        PeError { kind: PeErrorKind::Unsupported, msg: msg.into(), file: None }
    }
    fn layout(msg: impl Into<String>) -> Self {
        PeError { kind: PeErrorKind::Layout, msg: msg.into(), file: None }
    }
    /// The same error, attributed to a file.
    pub fn in_file(mut self, path: &str) -> Self {
        self.file.get_or_insert_with(|| path.to_string());
        self
    }
}

impl fmt::Display for PeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(p) = &self.file {
            write!(f, "{p}: ")?;
        }
        match self.kind {
            PeErrorKind::Malformed => write!(f, "malformed PE image: {}", self.msg),
            PeErrorKind::Unsupported => write!(f, "unsupported PE feature: {}", self.msg),
            PeErrorKind::Layout => write!(f, "PE placement: {}", self.msg),
        }
    }
}

impl std::error::Error for PeError {}

type Result<T> = std::result::Result<T, PeError>;

// ---------------------------------------------------------------------------
// Parsed structures
// ---------------------------------------------------------------------------

/// One section header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub rva: u32,
    pub vsize: u32,
    pub raw_offset: u32,
    pub raw_size: u32,
    pub characteristics: u32,
}

impl Section {
    pub fn readable(&self) -> bool {
        self.characteristics & SCN_MEM_READ != 0
    }
    pub fn writable(&self) -> bool {
        self.characteristics & SCN_MEM_WRITE != 0
    }
    pub fn executable(&self) -> bool {
        self.characteristics & SCN_MEM_EXECUTE != 0
    }
    /// Bytes the section occupies from its RVA, which is what the
    /// section-overlap and SizeOfImage checks use: what Windows maps
    /// (`VirtualSize`, or the raw size when that is 0). Raw data past
    /// `VirtualSize` is in the file only: Godot keeps its whole game (a
    /// 700 MB `pck` section of VirtualSize 8) there.
    pub fn extent(&self) -> u32 {
        self.mapped_size()
    }
    /// The size Windows maps: `VirtualSize`, or the raw size when that is 0.
    fn mapped_size(&self) -> u32 {
        if self.vsize == 0 {
            self.raw_size
        } else {
            self.vsize
        }
    }
    /// True when `rva` lies in the part of the section that is mapped.
    pub fn contains(&self, rva: u32) -> bool {
        rva >= self.rva && (rva - self.rva) < self.mapped_size()
    }
}

/// One base relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BaseReloc {
    pub rva: u32,
    /// `IMAGE_REL_BASED_*`; only [`rel::HIGHLOW`] and [`rel::DIR64`] are kept
    /// (`ABSOLUTE` is padding and anything else is refused at parse time).
    pub kind: u8,
}

/// How many relocations of each kind an image has.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelocCounts {
    pub highlow: usize,
    pub dir64: usize,
    /// `IMAGE_REL_BASED_ABSOLUTE` padding entries, skipped.
    pub absolute: usize,
    /// Relocation blocks.
    pub blocks: usize,
}

/// How an import names what it wants.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ImportName {
    Name { name: String, hint: u16 },
    Ordinal(u16),
}

impl ImportName {
    /// `name`, or `#ordinal`: the spelling of a forwarder and of a label.
    pub fn label(&self) -> String {
        match self {
            ImportName::Name { name, .. } => name.clone(),
            ImportName::Ordinal(o) => format!("#{o}"),
        }
    }
    /// The key two imports of the same thing share: the hint is advisory.
    fn key(&self) -> ImportKey {
        match self {
            ImportName::Name { name, .. } => ImportKey::Name(name.clone()),
            ImportName::Ordinal(o) => ImportKey::Ordinal(*o),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum ImportKey {
    Name(String),
    Ordinal(u16),
}

/// One imported symbol and the IAT slot it binds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportSymbol {
    pub import: ImportName,
    /// RVA of the IAT slot.
    pub iat_rva: u32,
    /// RVA of the import-lookup (name) table entry it was read from.
    pub lookup_rva: u32,
}

/// One `IMAGE_IMPORT_DESCRIPTOR`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportDll {
    /// As the image spells it.
    pub dll: String,
    /// `OriginalFirstThunk`, or 0 when the image has none (names are then
    /// read from the IAT itself).
    pub int_rva: u32,
    /// `FirstThunk`.
    pub iat_rva: u32,
    /// Nonzero when the IAT was pre-bound (`-1` for new-style binding, with
    /// the details in the bound-import directory). The loader always
    /// rebinds from the name table, so this is informational.
    pub timestamp: u32,
    pub forwarder_chain: u32,
    pub symbols: Vec<ImportSymbol>,
}

/// One `IMAGE_BOUND_IMPORT_DESCRIPTOR` (directory 11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundImport {
    pub dll: String,
    pub timestamp: u32,
    /// `IMAGE_BOUND_FORWARDER_REF`s: DLLs the bound DLL forwarded into.
    pub forwarders: Vec<(String, u32)>,
}

/// One `IMAGE_DELAYLOAD_DESCRIPTOR`, with every RVA already converted from a
/// VA when the descriptor is the old VA-based kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelayImportDll {
    pub dll: String,
    /// `Attributes`; bit 0 set means the fields were RVAs.
    pub attributes: u32,
    pub descriptor_rva: u32,
    pub module_handle_rva: u32,
    pub iat_rva: u32,
    pub int_rva: u32,
    pub bound_iat_rva: u32,
    pub unload_iat_rva: u32,
    pub timestamp: u32,
    pub symbols: Vec<ImportSymbol>,
}

/// One exported function (one entry in the export address table).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Export {
    /// Biased ordinal: what an importer names.
    pub ordinal: u32,
    /// Every name pointing at it (usually zero or one).
    pub names: Vec<String>,
    /// Zero for an unused table entry.
    pub rva: u32,
    /// `DLL.name` or `DLL.#ordinal`, when the RVA points inside the export
    /// directory.
    pub forwarder: Option<String>,
}

/// The export directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Exports {
    /// The name the DLL gives itself.
    pub dll_name: String,
    pub ordinal_base: u32,
    /// Index `i` has ordinal `ordinal_base + i`.
    pub entries: Vec<Export>,
    by_name: HashMap<String, usize>,
}

impl Exports {
    /// The entry an import resolves to.
    pub fn find(&self, import: &ImportName) -> Option<&Export> {
        let i = match import {
            ImportName::Name { name, .. } => *self.by_name.get(name)?,
            ImportName::Ordinal(o) => (*o as u32).checked_sub(self.ordinal_base)? as usize,
        };
        self.entries.get(i).filter(|e| e.rva != 0)
    }
}

/// The TLS directory, with its VAs at the preferred base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsDir {
    pub start_raw_va: u64,
    pub end_raw_va: u64,
    pub index_va: u64,
    pub callbacks_va: u64,
    pub zero_fill: u32,
    pub characteristics: u32,
}

/// A resource directory name or ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResId {
    Id(u32),
    Name(String),
}

impl fmt::Display for ResId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResId::Id(i) => write!(f, "{i}"),
            ResId::Name(n) => write!(f, "\"{n}\""),
        }
    }
}

/// One leaf of the resource tree. The data itself is not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resource {
    /// The path from the root: normally type, name, language.
    pub path: Vec<ResId>,
    pub data_rva: u32,
    pub size: u32,
    pub codepage: u32,
}

/// One `RUNTIME_FUNCTION` from the exception directory (`.pdata`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeFunction {
    pub begin_rva: u32,
    /// AMD64: the end RVA. ARM64 entries have none; this is `begin_rva` plus
    /// the packed function length when the unwind data is packed, else 0.
    pub end_rva: u32,
    /// AMD64: RVA of the `UNWIND_INFO`. ARM64: the raw second word.
    pub unwind: u32,
    /// AMD64: the `UNWIND_INFO` flags (`UNW_FLAG_EHANDLER`=1,
    /// `UNW_FLAG_UHANDLER`=2, `UNW_FLAG_CHAININFO`=4), or `None` when the
    /// unwind info is not in the file.
    pub unwind_flags: Option<u8>,
}

/// A parsed PE image, not yet placed.
#[derive(Clone, Debug)]
pub struct PeImage {
    pub machine: u16,
    /// PE32+ (`0x20b` optional header).
    pub is64: bool,
    pub characteristics: u16,
    pub dll_characteristics: u16,
    pub subsystem: u16,
    pub timestamp: u32,
    pub preferred_base: u64,
    pub image_size: u32,
    pub headers_size: u32,
    pub section_align: u32,
    pub file_align: u32,
    pub entry_rva: u32,
    /// Offset of the optional header in the file (and the mapped image).
    pub opt_offset: usize,
    /// `(rva, size)` of each data directory.
    pub dirs: Vec<(u32, u32)>,
    pub sections: Vec<Section>,
    pub relocs: Vec<BaseReloc>,
    pub reloc_counts: RelocCounts,
    pub imports: Vec<ImportDll>,
    pub bound_imports: Vec<BoundImport>,
    pub delay_imports: Vec<DelayImportDll>,
    pub exports: Exports,
    pub tls: Option<TlsDir>,
    pub resources: Vec<Resource>,
    pub pdata: Vec<RuntimeFunction>,
    /// The file's bytes.
    pub data: Vec<u8>,
    /// A mixed-mode (C++/CLI) image's CLR side, which is not run: see
    /// [`ClrMixed`].
    pub clr: Option<ClrMixed>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn rd16(d: &[u8], o: usize) -> Result<u16> {
    d.get(o..o + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| PeError::malformed(format!("read of 2 bytes at file offset {o:#x} past the end")))
}
fn rd32(d: &[u8], o: usize) -> Result<u32> {
    d.get(o..o + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| PeError::malformed(format!("read of 4 bytes at file offset {o:#x} past the end")))
}
fn rd64(d: &[u8], o: usize) -> Result<u64> {
    d.get(o..o + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| PeError::malformed(format!("read of 8 bytes at file offset {o:#x} past the end")))
}

/// True when `bytes` starts like a PE image (an `MZ` header whose
/// `e_lfanew` names a `PE\0\0` signature).
pub fn is_pe(bytes: &[u8]) -> bool {
    if bytes.len() < 64 || &bytes[..2] != b"MZ" {
        return false;
    }
    let off = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    bytes.get(off..off + 4) == Some(b"PE\0\0")
}

const COMIMAGE_FLAGS_ILONLY: u32 = 0x1;
const COMIMAGE_FLAGS_NATIVE_ENTRYPOINT: u32 = 0x10;

/// A mixed-mode (C++/CLI) image loaded as native code, with no CLR: a game's
/// bug-reporter DLL, say, whose exports only call into managed code. Its managed entries return 0 ([`PeImage::map`]).
#[derive(Clone, Debug)]
pub struct ClrMixed {
    /// The native entry point (`COMIMAGE_FLAGS_NATIVE_ENTRYPOINT`), run as
    /// the DLL's entry point; none means no entry point.
    pub native_entry: Option<u32>,
    /// VTableFixups: (RVA, slot count, slot width).
    pub fixups: Vec<(u32, u16, u8)>,
}

/// Parse a PE32 or PE32+ image.
pub fn parse(bytes: &[u8]) -> Result<PeImage> {
    let d = bytes;
    if d.len() < 64 {
        return Err(PeError::malformed("truncated DOS header"));
    }
    if &d[..2] != b"MZ" {
        return Err(PeError::malformed("no MZ signature"));
    }
    let pe_off = rd32(d, 0x3c)? as usize;
    if d.get(pe_off..pe_off + 4) != Some(b"PE\0\0") {
        return Err(PeError::malformed("no PE signature at e_lfanew"));
    }
    let coff = pe_off + 4;
    let machine = rd16(d, coff)?;
    let nsections = rd16(d, coff + 2)? as usize;
    let timestamp = rd32(d, coff + 4)?;
    let opt_size = rd16(d, coff + 16)? as usize;
    let characteristics = rd16(d, coff + 18)?;
    let opt = coff + 20;
    if opt_size < 96 || opt + opt_size + nsections * 40 > d.len() {
        return Err(PeError::malformed("truncated optional header or section table"));
    }
    let magic = rd16(d, opt)?;
    let is64 = match magic {
        0x10b => false,
        0x20b => true,
        0x107 => return Err(PeError::unsupported("ROM images (optional-header magic 0x107)")),
        m => return Err(PeError::malformed(format!("unknown optional-header magic {m:#x}"))),
    };
    match (machine, is64) {
        (MACHINE_I386, false) | (MACHINE_AMD64, true) | (MACHINE_ARM64, true) => {}
        (MACHINE_I386 | MACHINE_AMD64 | MACHINE_ARM64, _) => {
            return Err(PeError::malformed(format!(
                "machine {machine:#x} with a {} optional header",
                if is64 { "PE32+" } else { "PE32" }
            )))
        }
        (m, _) => {
            return Err(PeError::unsupported(format!(
                "machine type {m:#x} (only i386, AMD64 and ARM64 images load)"
            )))
        }
    }
    if is64 && opt_size < 112 {
        return Err(PeError::malformed("truncated PE32+ optional header"));
    }
    let entry_rva = rd32(d, opt + 16)?;
    let preferred_base = if is64 { rd64(d, opt + 24)? } else { rd32(d, opt + 28)? as u64 };
    let section_align = rd32(d, opt + 32)?;
    let file_align = rd32(d, opt + 36)?;
    let image_size = rd32(d, opt + 56)?;
    let headers_size = rd32(d, opt + 60)?;
    let subsystem = rd16(d, opt + 68)?;
    let dll_characteristics = rd16(d, opt + 70)?;
    let nrva_off = opt + if is64 { 108 } else { 92 };
    let nrva = rd32(d, nrva_off)? as usize;
    if nrva > (opt + opt_size).saturating_sub(nrva_off + 4) / 8 {
        return Err(PeError::malformed("data directories exceed the optional header"));
    }
    if image_size == 0 || image_size > 1 << 30 {
        return Err(PeError::malformed(format!("SizeOfImage {image_size:#x} out of range")));
    }
    if section_align == 0 || !section_align.is_power_of_two() {
        return Err(PeError::malformed(format!("SectionAlignment {section_align:#x}")));
    }
    if preferred_base % IMAGE_ALIGN != 0 {
        return Err(PeError::malformed(format!(
            "ImageBase {preferred_base:#x} is not 64 KiB aligned"
        )));
    }
    if headers_size as usize > d.len() || headers_size > image_size {
        return Err(PeError::malformed("SizeOfHeaders exceeds the file or the image"));
    }
    // A native-subsystem image (a driver: Wine's mountmgr.sys, ntoskrnl.exe's
    // own) parses like any other; it is a program's loader that refuses to
    // start one. Under 32-bit Wine, winedevice.exe loads them, and
    // their units are built and placed from this parse.
    match subsystem {
        10..=13 => return Err(PeError::unsupported(format!("EFI images (subsystem {subsystem})"))),
        _ => {}
    }
    let mut dirs = Vec::with_capacity(nrva);
    for i in 0..nrva {
        dirs.push((rd32(d, nrva_off + 4 + i * 8)?, rd32(d, nrva_off + 8 + i * 8)?));
    }

    let sec_off = opt + opt_size;
    let mut sections = Vec::with_capacity(nsections);
    for i in 0..nsections {
        let o = sec_off + i * 40;
        let raw_name = &d[o..o + 8];
        let name_len = raw_name.iter().position(|&b| b == 0).unwrap_or(8);
        let s = Section {
            name: String::from_utf8_lossy(&raw_name[..name_len]).into_owned(),
            vsize: rd32(d, o + 8)?,
            rva: rd32(d, o + 12)?,
            raw_size: rd32(d, o + 16)?,
            raw_offset: rd32(d, o + 20)?,
            characteristics: rd32(d, o + 36)?,
        };
        if s.rva as u64 + s.extent() as u64 > image_size as u64 {
            return Err(PeError::malformed(format!("section {} exceeds SizeOfImage", s.name)));
        }
        if s.raw_size != 0 && s.raw_offset as u64 + s.raw_size as u64 > d.len() as u64 {
            return Err(PeError::malformed(format!("section {} exceeds the file", s.name)));
        }
        sections.push(s);
    }
    let mut occupied: Vec<(u32, u32)> = sections
        .iter()
        .filter(|s| s.extent() != 0)
        .map(|s| (s.rva, s.rva + s.extent()))
        .collect();
    occupied.sort();
    let mut previous = headers_size;
    for (start, end) in occupied {
        if start < previous {
            return Err(PeError::malformed("sections overlap each other or the headers"));
        }
        previous = end;
    }
    for (i, (rva, size)) in dirs.iter_mut().enumerate() {
        // The certificate table is addressed by file offset; the rest by RVA.
        let bound = if i == dir::SECURITY { d.len() as u64 } else { image_size as u64 };
        if *rva != 0 && (*rva as u64 >= bound || *size as u64 > bound - *rva as u64) {
            // The certificate table is never mapped, and Windows' loader
            // does not look at it. Shipped executables whose signature was
            // cut off after signing still point past the
            // file's end: such an image has no certificate table.
            if i == dir::SECURITY {
                (*rva, *size) = (0, 0);
                continue;
            }
            return Err(PeError::malformed(format!("data directory {i} exceeds the image")));
        }
    }

    let mut pe = PeImage {
        machine,
        is64,
        characteristics,
        dll_characteristics,
        subsystem,
        timestamp,
        preferred_base,
        image_size,
        headers_size,
        section_align,
        file_align,
        entry_rva,
        opt_offset: opt,
        dirs,
        sections,
        relocs: Vec::new(),
        reloc_counts: RelocCounts::default(),
        imports: Vec::new(),
        bound_imports: Vec::new(),
        delay_imports: Vec::new(),
        exports: Exports::default(),
        tls: None,
        resources: Vec::new(),
        pdata: Vec::new(),
        data: d.to_vec(),
        clr: None,
    };
    if pe.dir(dir::CLR).0 != 0 {
        pe.clr = Some(pe.parse_clr()?);
    }
    if entry_rva != 0 && !pe.sections.iter().any(|s| s.executable() && s.contains(entry_rva)) {
        return Err(PeError::malformed(format!(
            "entry point RVA {entry_rva:#x} is not in an executable section"
        )));
    }
    pe.parse_relocs()?;
    pe.imports = pe.parse_imports()?;
    // Only a hint (the loader rebinds every import from its name table), and
    // Windows ignores one that does not hold: some programs name a
    // directory in the headers that is not one (no terminator within the
    // image), and Windows runs them. Refusing the whole image would leave
    // every such program without a function-tier unit.
    pe.bound_imports = pe.parse_bound_imports().unwrap_or_default();
    pe.delay_imports = pe.parse_delay_imports()?;
    pe.exports = pe.parse_exports()?;
    pe.tls = pe.parse_tls()?;
    pe.resources = pe.parse_resources()?;
    pe.pdata = pe.parse_pdata()?;
    Ok(pe)
}

impl PeImage {
    /// `(rva, size)` of data directory `i`, or zeros when absent.
    pub fn dir(&self, i: usize) -> (u32, u32) {
        self.dirs.get(i).copied().unwrap_or((0, 0))
    }

    pub fn is_dll(&self) -> bool {
        self.characteristics & FILE_DLL != 0
    }

    /// Whether the image can be moved. It must not have been linked with its
    /// relocations stripped, and it must either have a relocation directory
    /// or declare itself movable (`DYNAMIC_BASE`): linkers omit `.reloc`
    /// from an image with no absolute addresses at all (a small x64 DLL, all
    /// RIP-relative), and Windows moves such an image with no fixups. An
    /// image with neither is treated as fixed.
    pub fn relocatable(&self) -> bool {
        self.characteristics & FILE_RELOCS_STRIPPED == 0
            && (self.dir(dir::BASERELOC).0 != 0 || self.dll_characteristics & DLLCHAR_DYNAMIC_BASE != 0)
    }

    /// Why [`Self::relocatable`] is false, for an error message.
    fn fixed_reason(&self) -> &'static str {
        if self.characteristics & FILE_RELOCS_STRIPPED != 0 {
            " (IMAGE_FILE_RELOCS_STRIPPED)"
        } else {
            " (no .reloc directory, and not linked /DYNAMICBASE)"
        }
    }

    fn ptr_size(&self) -> u32 {
        if self.is64 {
            8
        } else {
            4
        }
    }

    /// The bytes a section spans in the image: its mapped size up to the
    /// section alignment. Raw data past that is in the file only.
    fn mapped_span(&self, s: &Section) -> u32 {
        let a = self.section_align.max(1) as u64;
        (s.mapped_size() as u64).div_ceil(a).saturating_mul(a).min(u32::MAX as u64) as u32
    }

    /// The CLR header of an image with a COM descriptor: an IL-only image is
    /// refused (it needs a CLR); a mixed-mode one is kept as native code.
    fn parse_clr(&self) -> Result<ClrMixed> {
        let (rva, _) = self.dir(dir::CLR);
        let h = self.bytes_at(rva, 72)?;
        let r32 = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
        let flags = r32(16);
        if flags & COMIMAGE_FLAGS_ILONLY != 0 {
            return Err(PeError::unsupported(".NET/CLR images (IL only: a CLR is needed)"));
        }
        let native_entry = (flags & COMIMAGE_FLAGS_NATIVE_ENTRYPOINT != 0).then(|| r32(20));
        let (vrva, vsize) = (r32(48), r32(52));
        let mut fixups = Vec::new();
        if vrva != 0 && vsize != 0 {
            let t = self.bytes_at(vrva, vsize)?;
            for e in t.chunks_exact(8) {
                let at = u32::from_le_bytes(e[0..4].try_into().unwrap());
                let count = u16::from_le_bytes(e[4..6].try_into().unwrap());
                let ty = u16::from_le_bytes(e[6..8].try_into().unwrap());
                let width = if ty & 2 != 0 { 8 } else { 4 };
                if !self.in_image(at, count as u32 * width) {
                    return Err(PeError::malformed(format!("CLR vtable fixup at {at:#x} exceeds the image")));
                }
                fixups.push((at, count, width as u8));
            }
        }
        Ok(ClrMixed { native_entry, fixups })
    }

    /// Make a mapped mixed-mode image a native one: its native entry point
    /// (the CRT's DllMain) in place of the CLR's, no COM descriptor (so
    /// ntdll does not hand it to mscoree), and every managed entry (the
    /// vtable fixup slots that native code calls through) a function that
    /// returns 0, found in the image's own code (`xor eax, eax; ret`).
    fn unmanage(&self, img: &mut [u8], base: u64, clr: &ClrMixed) -> Result<()> {
        let o = self.opt_offset;
        img[o + 16..o + 20].copy_from_slice(&clr.native_entry.unwrap_or(0).to_le_bytes());
        let dd = o + if self.is64 { 112 } else { 96 } + dir::CLR * 8;
        img[dd..dd + 8].fill(0);
        if clr.fixups.is_empty() {
            return Ok(());
        }
        let stub = self
            .sections
            .iter()
            .filter(|s| s.executable())
            .find_map(|s| {
                let (a, n) = (s.rva as usize, (s.raw_size.min(self.mapped_span(s)) as usize).min(img.len() - s.rva as usize));
                img[a..a + n].windows(3).position(|w| matches!(w, [0x33 | 0x31, 0xc0, 0xc3])).map(|i| (a + i) as u64)
            })
            .ok_or_else(|| PeError::unsupported("a mixed-mode image with no `xor eax, eax; ret` to stand in for its managed code"))?;
        let target = base + stub;
        for &(at, count, width) in &clr.fixups {
            for i in 0..count as usize {
                let p = at as usize + i * width as usize;
                if width == 8 {
                    img[p..p + 8].copy_from_slice(&target.to_le_bytes());
                } else {
                    img[p..p + 4].copy_from_slice(&(target as u32).to_le_bytes());
                }
            }
        }
        Ok(())
    }

    /// File offset of an RVA, if the file holds it (headers or a section's
    /// raw data).
    pub fn rva_to_offset(&self, rva: u32) -> Option<usize> {
        if rva < self.headers_size {
            return Some(rva as usize).filter(|&o| o < self.data.len());
        }
        for s in &self.sections {
            if rva >= s.rva && rva - s.rva < s.raw_size.min(self.mapped_span(s)) {
                let off = s.raw_offset as usize + (rva - s.rva) as usize;
                return Some(off).filter(|&o| o < self.data.len());
            }
        }
        None
    }

    /// `n` bytes at an RVA, all within one run of file data.
    fn bytes_at(&self, rva: u32, n: u32) -> Result<&[u8]> {
        let err = || PeError::malformed(format!("RVA {rva:#x}+{n:#x} is not backed by file data"));
        let off = self.rva_to_offset(rva).ok_or_else(err)?;
        if n > 0 {
            let last = self.rva_to_offset(rva.checked_add(n - 1).ok_or_else(err)?).ok_or_else(err)?;
            if last != off + n as usize - 1 {
                return Err(err());
            }
        }
        Ok(&self.data[off..off + n as usize])
    }

    /// `n` bytes at an RVA inside the image, where any not backed by file
    /// data (a section's virtual tail past its raw data) read as the zeros
    /// the loader maps there. A packed image's exception directory lies in
    /// such memory: its unpacker writes the table at run time.
    fn bytes_or_zeros(&self, rva: u32, n: u32) -> Result<std::borrow::Cow<'_, [u8]>> {
        if let Ok(b) = self.bytes_at(rva, n) {
            return Ok(std::borrow::Cow::Borrowed(b));
        }
        if !self.in_image(rva, n) {
            return Err(PeError::malformed(format!("RVA {rva:#x}+{n:#x} is outside the image")));
        }
        Ok(std::borrow::Cow::Owned(
            (rva..rva + n).map(|r| self.rva_to_offset(r).map_or(0, |o| self.data[o])).collect(),
        ))
    }

    /// A read at an RVA that may lie in zero-filled memory (uninitialised
    /// data), which reads as zero.
    fn u32_at(&self, rva: u32) -> Result<u32> {
        if self.rva_to_offset(rva).is_none() && self.in_image(rva, 4) {
            return Ok(0);
        }
        Ok(u32::from_le_bytes(self.bytes_at(rva, 4)?.try_into().unwrap()))
    }
    fn u64_at(&self, rva: u32) -> Result<u64> {
        if self.rva_to_offset(rva).is_none() && self.in_image(rva, 8) {
            return Ok(0);
        }
        Ok(u64::from_le_bytes(self.bytes_at(rva, 8)?.try_into().unwrap()))
    }
    fn u16_at(&self, rva: u32) -> Result<u16> {
        Ok(u16::from_le_bytes(self.bytes_at(rva, 2)?.try_into().unwrap()))
    }
    fn ptr_at(&self, rva: u32) -> Result<u64> {
        if self.is64 {
            self.u64_at(rva)
        } else {
            self.u32_at(rva).map(u64::from)
        }
    }
    fn in_image(&self, rva: u32, n: u32) -> bool {
        (rva as u64) + (n as u64) <= self.image_size as u64
    }

    fn cstr_at(&self, rva: u32) -> Result<String> {
        let off = self
            .rva_to_offset(rva)
            .ok_or_else(|| PeError::malformed(format!("string at RVA {rva:#x} is not in the file")))?;
        let tail = &self.data[off..];
        let end = tail
            .iter()
            .take(4096)
            .position(|&b| b == 0)
            .ok_or_else(|| PeError::malformed(format!("unterminated string at RVA {rva:#x}")))?;
        Ok(String::from_utf8_lossy(&tail[..end]).into_owned())
    }

    /// Convert a VA at the preferred base to an RVA.
    fn va_to_rva(&self, va: u64, what: &str) -> Result<u32> {
        va.checked_sub(self.preferred_base)
            .filter(|&r| r < self.image_size as u64)
            .map(|r| r as u32)
            .ok_or_else(|| PeError::malformed(format!("{what} VA {va:#x} is outside the image")))
    }

    fn parse_relocs(&mut self) -> Result<()> {
        let (rva, size) = self.dir(dir::BASERELOC);
        if rva == 0 || size == 0 {
            return Ok(());
        }
        let block = self.bytes_at(rva, size)?.to_vec();
        let mut off = 0usize;
        let mut out = Vec::new();
        let mut counts = RelocCounts::default();
        while off + 8 <= block.len() {
            let page = u32::from_le_bytes(block[off..off + 4].try_into().unwrap());
            let len = u32::from_le_bytes(block[off + 4..off + 8].try_into().unwrap()) as usize;
            if len == 0 && page == 0 {
                // Some linkers pad the directory with an empty block.
                break;
            }
            if len < 8 || !len.is_multiple_of(2) || off + len > block.len() {
                return Err(PeError::malformed(format!(
                    "base-relocation block at {:#x} has size {len:#x}",
                    rva as usize + off
                )));
            }
            counts.blocks += 1;
            for i in 0..(len - 8) / 2 {
                let w = u16::from_le_bytes(block[off + 8 + i * 2..off + 10 + i * 2].try_into().unwrap());
                let kind = (w >> 12) as u8;
                let at = page.wrapping_add((w & 0xfff) as u32);
                match kind {
                    rel::ABSOLUTE => counts.absolute += 1,
                    rel::HIGHLOW | rel::DIR64 => {
                        let width = if kind == rel::DIR64 { 8 } else { 4 };
                        if !self.in_image(at, width) {
                            return Err(PeError::malformed(format!(
                                "base relocation at RVA {at:#x} is outside the image"
                            )));
                        }
                        if kind == rel::DIR64 {
                            if !self.is64 {
                                return Err(PeError::unsupported(format!(
                                    "DIR64 base relocation in a PE32 image (RVA {at:#x})"
                                )));
                            }
                            counts.dir64 += 1;
                        } else {
                            counts.highlow += 1;
                        }
                        out.push(BaseReloc { rva: at, kind });
                    }
                    k => {
                        let name = match k {
                            1 => "HIGH",
                            2 => "LOW",
                            4 => "HIGHADJ",
                            5 => "MIPS_JMPADDR/ARM_MOV32",
                            7 => "THUMB_MOV32",
                            9 => "MIPS_JMPADDR16",
                            _ => "unknown",
                        };
                        return Err(PeError::unsupported(format!(
                            "base relocation type {k} ({name}) at RVA {at:#x}"
                        )));
                    }
                }
            }
            off += len;
        }
        self.relocs = out;
        self.reloc_counts = counts;
        Ok(())
    }

    /// Read an import-lookup-table entry list starting at `table`.
    fn read_thunks(&self, table: u32, iat: u32, what: &str) -> Result<Vec<ImportSymbol>> {
        let w = self.ptr_size();
        let flag = if self.is64 { 1u64 << 63 } else { 1u64 << 31 };
        let mut out = Vec::new();
        for j in 0.. {
            if j > self.image_size / w {
                return Err(PeError::malformed(format!("unterminated {what} lookup table")));
            }
            let at = table + j * w;
            let thunk = self.ptr_at(at)?;
            if thunk == 0 {
                break;
            }
            let import = if thunk & flag != 0 {
                if thunk & !(flag | 0xffff) != 0 {
                    return Err(PeError::malformed(format!(
                        "{what} ordinal thunk {thunk:#x} has stray bits"
                    )));
                }
                ImportName::Ordinal(thunk as u16)
            } else {
                if thunk > u32::MAX as u64 {
                    return Err(PeError::malformed(format!("{what} name thunk {thunk:#x}")));
                }
                let hn = thunk as u32;
                ImportName::Name { hint: self.u16_at(hn)?, name: self.cstr_at(hn + 2)? }
            };
            out.push(ImportSymbol { import, iat_rva: iat + j * w, lookup_rva: at });
        }
        Ok(out)
    }

    fn parse_imports(&self) -> Result<Vec<ImportDll>> {
        let (rva, _) = self.dir(dir::IMPORT);
        if rva == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for i in 0.. {
            let o = rva + i * 20;
            // The terminator is not always inside the directory's stated
            // size, so only the image bounds the walk.
            if !self.in_image(o, 20) {
                return Err(PeError::malformed("unterminated import directory"));
            }
            let f = self.bytes_at(o, 20)?;
            let word = |k: usize| u32::from_le_bytes(f[k * 4..k * 4 + 4].try_into().unwrap());
            let (oft, ts, chain, name, ft) = (word(0), word(1), word(2), word(3), word(4));
            if oft == 0 && name == 0 && ft == 0 {
                break;
            }
            let dll = self.cstr_at(name)?;
            if ts != 0 && oft == 0 {
                return Err(PeError::unsupported(format!(
                    "bound imports from {dll} without an import name table (OriginalFirstThunk = 0)"
                )));
            }
            let symbols = self.read_thunks(if oft != 0 { oft } else { ft }, ft, "import")?;
            out.push(ImportDll {
                dll,
                int_rva: oft,
                iat_rva: ft,
                timestamp: ts,
                forwarder_chain: chain,
                symbols,
            });
        }
        Ok(out)
    }

    fn parse_bound_imports(&self) -> Result<Vec<BoundImport>> {
        let (rva, size) = self.dir(dir::BOUND_IMPORT);
        if rva == 0 || size == 0 {
            return Ok(Vec::new());
        }
        // Usually in the headers, after the section table.
        let d = self.bytes_at(rva, size)?;
        let name_at = |off: u16| -> Result<String> { self.cstr_at(rva + off as u32) };
        let mut out = Vec::new();
        let mut o = 0usize;
        while o + 8 <= d.len() {
            let ts = u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
            let name = u16::from_le_bytes(d[o + 4..o + 6].try_into().unwrap());
            let nref = u16::from_le_bytes(d[o + 6..o + 8].try_into().unwrap()) as usize;
            if ts == 0 && name == 0 && nref == 0 {
                return Ok(out);
            }
            let mut forwarders = Vec::new();
            for k in 0..nref {
                let r = o + 8 + k * 8;
                let f = d.get(r..r + 8).ok_or_else(|| PeError::malformed("truncated bound-import forwarder"))?;
                let fts = u32::from_le_bytes(f[0..4].try_into().unwrap());
                let fname = u16::from_le_bytes(f[4..6].try_into().unwrap());
                forwarders.push((name_at(fname)?, fts));
            }
            out.push(BoundImport { dll: name_at(name)?, timestamp: ts, forwarders });
            o += 8 + nref * 8;
        }
        Err(PeError::malformed("unterminated bound-import directory"))
    }

    fn parse_delay_imports(&self) -> Result<Vec<DelayImportDll>> {
        let (rva, size) = self.dir(dir::DELAY_IMPORT);
        if rva == 0 && size == 0 {
            return Ok(Vec::new());
        }
        if rva == 0 {
            return Err(PeError::malformed("delay-import directory with no RVA"));
        }
        let mut out = Vec::new();
        for i in 0.. {
            let o = rva + i * 32;
            if !self.in_image(o, 32) {
                return Err(PeError::malformed("unterminated delay-import directory"));
            }
            let f = self.bytes_at(o, 32)?;
            let w: Vec<u32> = (0..8).map(|k| u32::from_le_bytes(f[k * 4..k * 4 + 4].try_into().unwrap())).collect();
            if w.iter().all(|&x| x == 0) {
                break;
            }
            let attributes = w[0];
            let rva_based = attributes & 1 != 0;
            if attributes & !1 != 0 {
                return Err(PeError::unsupported(format!(
                    "delay-import attributes {attributes:#x}"
                )));
            }
            if !rva_based && self.is64 {
                return Err(PeError::unsupported("VA-based (version 1) delay imports in a PE32+ image"));
            }
            let conv = |v: u32, what: &str| -> Result<u32> {
                if v == 0 || rva_based {
                    Ok(v)
                } else {
                    self.va_to_rva(v as u64, what)
                }
            };
            let name = conv(w[1], "delay-import name")?;
            let hmod = conv(w[2], "delay-import module handle")?;
            let iat = conv(w[3], "delay-import IAT")?;
            let int = conv(w[4], "delay-import name table")?;
            let bound = conv(w[5], "delay-import bound IAT")?;
            let unload = conv(w[6], "delay-import unload IAT")?;
            if name == 0 || iat == 0 || int == 0 {
                return Err(PeError::malformed("delay-import descriptor lacks a name or thunk table"));
            }
            let dll = self.cstr_at(name)?;
            let mut symbols = self.read_thunks(int, iat, "delay-import")?;
            if !rva_based {
                // Version-1 name thunks are VAs too.
                for s in &mut symbols {
                    if let ImportName::Name { .. } = s.import {
                        let raw = self.ptr_at(s.lookup_rva)?;
                        let hn = self.va_to_rva(raw, "delay-import hint/name")?;
                        s.import = ImportName::Name { hint: self.u16_at(hn)?, name: self.cstr_at(hn + 2)? };
                    }
                }
            }
            out.push(DelayImportDll {
                dll,
                attributes,
                descriptor_rva: o,
                module_handle_rva: hmod,
                iat_rva: iat,
                int_rva: int,
                bound_iat_rva: bound,
                unload_iat_rva: unload,
                timestamp: w[7],
                symbols,
            });
        }
        Ok(out)
    }

    fn parse_exports(&self) -> Result<Exports> {
        let (rva, size) = self.dir(dir::EXPORT);
        if rva == 0 {
            return Ok(Exports::default());
        }
        let h = self.bytes_at(rva, 40)?;
        let w = |k: usize| u32::from_le_bytes(h[k..k + 4].try_into().unwrap());
        let (name_rva, base, nfunc, nname, funcs_rva, names_rva, ords_rva) =
            (w(12), w(16), w(20), w(24), w(28), w(32), w(36));
        if nfunc > self.image_size / 4 || nname > self.image_size / 4 {
            return Err(PeError::malformed("export directory counts exceed the image"));
        }
        let dll_name = if name_rva != 0 { self.cstr_at(name_rva)? } else { String::new() };
        let mut entries = Vec::with_capacity(nfunc as usize);
        for i in 0..nfunc {
            let frva = self.u32_at(funcs_rva + i * 4)?;
            let forwarder = if frva != 0 && frva >= rva && frva < rva + size {
                Some(self.cstr_at(frva)?)
            } else {
                None
            };
            entries.push(Export { ordinal: base + i, names: Vec::new(), rva: frva, forwarder });
        }
        let mut by_name = HashMap::new();
        for i in 0..nname {
            let nrva = self.u32_at(names_rva + i * 4)?;
            let idx = self.u16_at(ords_rva + i * 2)? as usize;
            if idx >= entries.len() {
                return Err(PeError::malformed(format!("export name {i} names ordinal index {idx}")));
            }
            let name = self.cstr_at(nrva)?;
            by_name.entry(name.clone()).or_insert(idx);
            entries[idx].names.push(name);
        }
        Ok(Exports { dll_name, ordinal_base: base, entries, by_name })
    }

    fn parse_tls(&self) -> Result<Option<TlsDir>> {
        let (rva, _) = self.dir(dir::TLS);
        if rva == 0 {
            return Ok(None);
        }
        let t = if self.is64 {
            let b = self.bytes_at(rva, 40)?;
            let q = |k: usize| u64::from_le_bytes(b[k..k + 8].try_into().unwrap());
            let d = |k: usize| u32::from_le_bytes(b[k..k + 4].try_into().unwrap());
            TlsDir {
                start_raw_va: q(0),
                end_raw_va: q(8),
                index_va: q(16),
                callbacks_va: q(24),
                zero_fill: d(32),
                characteristics: d(36),
            }
        } else {
            let b = self.bytes_at(rva, 24)?;
            let d = |k: usize| u32::from_le_bytes(b[k..k + 4].try_into().unwrap());
            TlsDir {
                start_raw_va: d(0) as u64,
                end_raw_va: d(4) as u64,
                index_va: d(8) as u64,
                callbacks_va: d(12) as u64,
                zero_fill: d(16),
                characteristics: d(20),
            }
        };
        if t.end_raw_va < t.start_raw_va {
            return Err(PeError::malformed("TLS directory's raw data ends before it starts"));
        }
        for (va, what) in [
            (t.start_raw_va, "TLS raw data"),
            (t.index_va, "TLS index"),
            (t.callbacks_va, "TLS callback table"),
        ] {
            if va != 0 {
                self.va_to_rva(va, what)?;
            }
        }
        Ok(Some(t))
    }

    fn parse_resources(&self) -> Result<Vec<Resource>> {
        let (rva, size) = self.dir(dir::RESOURCE);
        if rva == 0 || size == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        self.walk_resources(rva, size, 0, &mut Vec::new(), &mut seen, &mut out)?;
        Ok(out)
    }

    fn walk_resources(
        &self,
        root: u32,
        size: u32,
        dir_off: u32,
        path: &mut Vec<ResId>,
        seen: &mut HashSet<u32>,
        out: &mut Vec<Resource>,
    ) -> Result<()> {
        if path.len() > 8 || !seen.insert(dir_off) {
            return Err(PeError::malformed("resource directory is cyclic or too deep"));
        }
        if dir_off.saturating_add(16) > size {
            return Err(PeError::malformed("resource directory outside its data directory"));
        }
        let h = self.bytes_at(root + dir_off, 16)?;
        let named = u16::from_le_bytes([h[12], h[13]]) as u32;
        let ids = u16::from_le_bytes([h[14], h[15]]) as u32;
        for k in 0..named + ids {
            let e = self.bytes_at(root + dir_off + 16 + k * 8, 8)?;
            let name = u32::from_le_bytes(e[0..4].try_into().unwrap());
            let target = u32::from_le_bytes(e[4..8].try_into().unwrap());
            let id = if name & 0x8000_0000 != 0 {
                let so = name & 0x7fff_ffff;
                let len = self.u16_at(root + so)? as u32;
                let raw = self.bytes_at(root + so + 2, len * 2)?;
                let units: Vec<u16> = raw.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                ResId::Name(String::from_utf16_lossy(&units))
            } else {
                ResId::Id(name)
            };
            path.push(id);
            if target & 0x8000_0000 != 0 {
                self.walk_resources(root, size, target & 0x7fff_ffff, path, seen, out)?;
            } else {
                if target.saturating_add(16) > size {
                    return Err(PeError::malformed("resource data entry outside its data directory"));
                }
                let de = self.bytes_at(root + target, 16)?;
                let q = |k: usize| u32::from_le_bytes(de[k..k + 4].try_into().unwrap());
                let (data_rva, dsize, codepage) = (q(0), q(4), q(8));
                if !self.in_image(data_rva, dsize) {
                    return Err(PeError::malformed("resource data outside the image"));
                }
                out.push(Resource { path: path.clone(), data_rva, size: dsize, codepage });
            }
            path.pop();
        }
        Ok(())
    }

    fn parse_pdata(&self) -> Result<Vec<RuntimeFunction>> {
        let (rva, size) = self.dir(dir::EXCEPTION);
        if rva == 0 || size == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        match self.machine {
            MACHINE_AMD64 => {
                if size % 12 != 0 {
                    return Err(PeError::malformed("AMD64 exception directory size is not a multiple of 12"));
                }
                // (an all-zero entry, as the zeros of an unbacked table are, is
                // no function)
                let b = self.bytes_or_zeros(rva, size)?;
                for c in b.chunks(12) {
                    let w = |k: usize| u32::from_le_bytes(c[k..k + 4].try_into().unwrap());
                    let (begin, end, unwind) = (w(0), w(4), w(8));
                    if begin == 0 && end == 0 {
                        continue;
                    }
                    // Bit 0 of the unwind RVA marks a chained entry in some
                    // images; the info itself is at the aligned address.
                    let unwind_flags = self.bytes_at(unwind & !1, 1).ok().map(|b| b[0] >> 3);
                    out.push(RuntimeFunction { begin_rva: begin, end_rva: end, unwind, unwind_flags });
                }
            }
            MACHINE_ARM64 => {
                if size % 8 != 0 {
                    return Err(PeError::malformed("ARM64 exception directory size is not a multiple of 8"));
                }
                let b = self.bytes_or_zeros(rva, size)?;
                for c in b.chunks(8) {
                    let begin = u32::from_le_bytes(c[0..4].try_into().unwrap());
                    let word = u32::from_le_bytes(c[4..8].try_into().unwrap());
                    if begin == 0 && word == 0 {
                        continue;
                    }
                    let end = if word & 3 != 0 { begin + ((word >> 2) & 0x7ff) * 4 } else { 0 };
                    out.push(RuntimeFunction { begin_rva: begin, end_rva: end, unwind: word, unwind_flags: None });
                }
            }
            m => {
                return Err(PeError::unsupported(format!(
                    "an exception directory on machine {m:#x} (only AMD64 and ARM64 define one)"
                )))
            }
        }
        Ok(out)
    }

    /// The image as Windows would map it at `base`: headers and sections
    /// copied into a zeroed `SizeOfImage` buffer, every base relocation
    /// applied, and the optional header's `ImageBase` updated. Imports are
    /// left as they are in the file.
    pub fn map(&self, base: u64) -> Result<Vec<u8>> {
        if !base.is_multiple_of(IMAGE_ALIGN) {
            return Err(PeError::layout(format!("base {base:#x} is not 64 KiB aligned")));
        }
        if !self.is64 && base + self.image_size as u64 > 1 << 32 {
            return Err(PeError::layout(format!("PE32 image at {base:#x} would cross 4 GiB")));
        }
        let delta = base.wrapping_sub(self.preferred_base);
        if delta != 0 && !self.relocatable() {
            return Err(PeError::layout(format!(
                "the image prefers base {:#x} and must move to {base:#x}, but has no base relocations{}",
                self.preferred_base,
                self.fixed_reason()
            )));
        }
        let mut img = vec![0u8; self.image_size as usize];
        let hs = self.headers_size as usize;
        img[..hs].copy_from_slice(&self.data[..hs]);
        for s in &self.sections {
            if s.raw_size == 0 || s.rva as usize >= img.len() {
                continue;
            }
            // Raw data beyond the mapped size (to its last page) is not
            // part of the image.
            let n = (s.raw_size.min(self.mapped_span(s)) as usize).min(img.len() - s.rva as usize);
            let src = &self.data[s.raw_offset as usize..s.raw_offset as usize + n];
            img[s.rva as usize..s.rva as usize + n].copy_from_slice(src);
        }
        if delta != 0 {
            for r in &self.relocs {
                let at = r.rva as usize;
                if r.kind == rel::DIR64 {
                    let v = u64::from_le_bytes(img[at..at + 8].try_into().unwrap()).wrapping_add(delta);
                    img[at..at + 8].copy_from_slice(&v.to_le_bytes());
                } else {
                    let v = u32::from_le_bytes(img[at..at + 4].try_into().unwrap()).wrapping_add(delta as u32);
                    img[at..at + 4].copy_from_slice(&v.to_le_bytes());
                }
            }
        }
        if let Some(clr) = &self.clr {
            self.unmanage(&mut img, base, clr)?;
        }
        let o = self.opt_offset;
        if self.is64 {
            img[o + 24..o + 32].copy_from_slice(&base.to_le_bytes());
        } else {
            img[o + 28..o + 32].copy_from_slice(&(base as u32).to_le_bytes());
        }
        Ok(img)
    }

    /// The image's mappings at `base`, with each section's protection. A
    /// section spans up to the next section-alignment boundary. An image whose
    /// section alignment is below the page size (so protections cannot be
    /// honoured page by page) is one read/write/execute mapping, which is
    /// what Windows does with such images too.
    pub fn segments(&self, base: u64, img: &[u8]) -> Vec<Segment> {
        let size = align_up(self.image_size as u64, PAGE);
        let seg = |start: u64, end: u64, r: bool, w: bool, x: bool| {
            let end = end.min(img.len() as u64);
            let mut bytes = img[start as usize..end as usize].to_vec();
            while bytes.last() == Some(&0) {
                bytes.pop();
            }
            Segment { vaddr: base + start, bytes, mem_size: 0, readable: r, writable: w, executable: x }
        };
        if (self.section_align as u64) < PAGE {
            let mut s = seg(0, size, true, true, true);
            s.mem_size = size;
            return vec![s];
        }
        let mut out = Vec::new();
        let mut hs = seg(0, self.headers_size as u64, true, false, false);
        hs.mem_size = align_up(self.headers_size as u64, PAGE);
        out.push(hs);
        let mut secs: Vec<&Section> = self.sections.iter().filter(|s| s.mapped_size() != 0).collect();
        secs.sort_by_key(|s| s.rva);
        for s in secs {
            let start = s.rva as u64;
            let end = align_up(start + s.mapped_size() as u64, self.section_align as u64).min(size);
            let mut g = seg(start, end, s.readable(), s.writable(), s.executable());
            g.mem_size = align_up(end, PAGE) - start;
            out.push(g);
        }
        out
    }
}

fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Where images may go. See the module documentation.
#[derive(Clone, Debug)]
pub struct PeLayout {
    /// Lowest address an image may occupy.
    pub floor: u64,
    /// One past the highest address an image may occupy.
    pub ceiling: u64,
    /// Ranges no image may overlap.
    pub reserved: Vec<(u64, u64)>,
    /// Rebase every relocatable image into the low packing, even when its
    /// preferred base would fit. On by default; see the module docs.
    pub pack: bool,
    /// Fixed bases by lower-case module name. A pinned image goes exactly
    /// there, the floor, ceiling and reserved ranges notwithstanding (only
    /// overlap with other images is still refused): this is how a layout
    /// from somewhere else (a previous run) is reproduced.
    pub pins: BTreeMap<String, u64>,
}

impl Default for PeLayout {
    fn default() -> Self {
        PeLayout {
            floor: gex_abi::EXEC_BASE,
            ceiling: gex_abi::MMAP_TOP,
            reserved: vec![(gex_abi::STACK_TOP - gex_abi::STACK_SIZE, gex_abi::STACK_TOP)],
            pack: true,
            pins: BTreeMap::new(),
        }
    }
}

/// Assigns addresses, remembering what is taken.
struct Placer<'a> {
    layout: &'a PeLayout,
    taken: Vec<(u64, u64)>,
}

impl Placer<'_> {
    fn free(&self, start: u64, end: u64) -> bool {
        !self.taken.iter().any(|&(a, b)| start < b && a < end)
    }
    fn allowed(&self, start: u64, end: u64) -> bool {
        start >= self.layout.floor
            && end <= self.layout.ceiling
            && !self.layout.reserved.iter().any(|&(a, b)| start < b && a < end)
            && self.free(start, end)
    }
    fn first_fit(&self, size: u64) -> Option<u64> {
        let mut at = align_up(self.layout.floor, IMAGE_ALIGN);
        while at + size <= self.layout.ceiling {
            let end = at + size;
            let blocker = self
                .taken
                .iter()
                .chain(self.layout.reserved.iter())
                .filter(|&&(a, b)| at < b && a < end)
                .map(|&(_, b)| b)
                .max();
            match blocker {
                None => return Some(at),
                Some(b) => at = align_up(b, IMAGE_ALIGN),
            }
        }
        None
    }

    /// Where `pe` goes, and whether that is a move.
    fn place(&mut self, name: &str, pe: &PeImage) -> Result<u64> {
        let size = align_up(pe.image_size as u64, IMAGE_ALIGN);
        let base = if let Some(&pin) = self.layout.pins.get(name) {
            if !self.free(pin, pin + size) {
                return Err(PeError::layout(format!("pinned base {pin:#x} overlaps another image")));
            }
            pin
        } else {
            let pref = pe.preferred_base;
            let keep = pref.checked_add(size).is_some_and(|end| self.allowed(pref, end))
                && !(self.layout.pack && pe.relocatable());
            if keep {
                pref
            } else {
                let at = self.first_fit(size).ok_or_else(|| {
                    PeError::layout(format!(
                        "no room for {size:#x} bytes below {:#x}",
                        self.layout.ceiling
                    ))
                })?;
                if !pe.relocatable() {
                    // `map` would refuse too, but here the reason is known.
                    let why = if pref.saturating_add(size) > self.layout.ceiling || pref < self.layout.floor {
                        format!(
                            "outside gex's code range [{:#x}, {:#x})",
                            self.layout.floor, self.layout.ceiling
                        )
                    } else {
                        "taken by another image or reserved".to_string()
                    };
                    return Err(PeError::layout(format!(
                        "preferred base {pref:#x} is {why}, and the image has no base relocations to move it{}",
                        pe.fixed_reason()
                    )));
                }
                at
            }
        };
        self.taken.push((base, base + size));
        Ok(base)
    }
}

// ---------------------------------------------------------------------------
// System DLLs
// ---------------------------------------------------------------------------

/// DLLs provided by the operating system (Wine's, at run time), which this
/// loader never loads from disk. Lower-case, with `.dll`.
pub const SYSTEM_DLLS: &[&str] = &[
    "ntdll.dll", "kernel32.dll", "kernelbase.dll", "user32.dll", "gdi32.dll", "gdiplus.dll",
    "win32u.dll", "advapi32.dll", "sechost.dll", "msvcrt.dll", "ucrtbase.dll", "msvcp_win.dll",
    "combase.dll", "ole32.dll", "oleaut32.dll", "rpcrt4.dll", "shell32.dll", "shlwapi.dll",
    "shcore.dll", "comctl32.dll", "comdlg32.dll", "uxtheme.dll", "dwmapi.dll", "imm32.dll",
    "version.dll", "winmm.dll", "dsound.dll", "dinput.dll", "dinput8.dll", "xinput1_1.dll",
    "xinput1_2.dll", "xinput1_3.dll", "xinput1_4.dll", "xinput9_1_0.dll", "opengl32.dll",
    "glu32.dll", "ddraw.dll", "d3d8.dll", "d3d9.dll", "d3d10.dll", "d3d10_1.dll", "d3d11.dll",
    "d3d12.dll", "dxgi.dll", "ws2_32.dll", "wsock32.dll", "mswsock.dll", "iphlpapi.dll",
    "dnsapi.dll", "winhttp.dll", "wininet.dll", "urlmon.dll", "netapi32.dll", "secur32.dll",
    "sspicli.dll", "bcrypt.dll", "ncrypt.dll", "crypt32.dll", "wintrust.dll", "setupapi.dll",
    "cfgmgr32.dll", "hid.dll", "psapi.dll", "dbghelp.dll", "userenv.dll", "powrprof.dll",
    "winspool.drv", "mpr.dll", "avrt.dll", "mfplat.dll", "mfreadwrite.dll", "mf.dll",
    "normaliz.dll", "msimg32.dll", "usp10.dll", "msacm32.dll", "msvfw32.dll", "avifil32.dll",
    "olepro32.dll", "oledlg.dll", "wtsapi32.dll", "credui.dll", "dwrite.dll", "d2d1.dll",
    "windowscodecs.dll", "propsys.dll", "mmdevapi.dll", "ksuser.dll", "dciman32.dll",
];

/// Whether a DLL is a system DLL (see [`SYSTEM_DLLS`]), including the
/// `api-ms-win-*` and `ext-ms-*` API-set contracts.
pub fn is_system_dll(name: &str) -> bool {
    let n = normalize_dll(name);
    n.starts_with("api-ms-win-") || n.starts_with("ext-ms-") || SYSTEM_DLLS.contains(&n.as_str())
}

/// The key a DLL is known by: the basename, lower-cased, with `.dll`
/// appended when there is no extension (forwarders and `LoadLibrary` omit it).
pub fn normalize_dll(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).to_ascii_lowercase();
    if base.contains('.') {
        base
    } else {
        format!("{base}.dll")
    }
}

// ---------------------------------------------------------------------------
// Placeholders
// ---------------------------------------------------------------------------

/// Why an import is a placeholder rather than an address in a loaded image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceholderReason {
    /// A system DLL, provided at run time.
    System,
    /// A non-system DLL not found in any search directory.
    MissingDll,
    /// The DLL was loaded but does not export the symbol.
    MissingExport,
}

/// One IAT slot that holds a placeholder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaceholderSlot {
    /// Index into [`PeProgram::modules`].
    pub module: usize,
    /// Guest address of the IAT slot.
    pub slot: u64,
    /// A delay-import slot: recorded, not written (it keeps its thunk).
    pub delay: bool,
}

/// One entry in the placeholder table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placeholder {
    pub index: u32,
    /// `PLACEHOLDER_BASE + index * PLACEHOLDER_STRIDE`.
    pub addr: u64,
    /// Normalized (lower-case, with extension).
    pub dll: String,
    pub import: ImportName,
    pub reason: PlaceholderReason,
    /// When the import reached this DLL through forwarders: the chain it
    /// followed, as `dll!name` steps, starting with what was imported.
    pub via: Vec<String>,
    pub slots: Vec<PlaceholderSlot>,
}

impl Placeholder {
    /// `DLL!name` or `DLL!#ordinal`: what a loud failure prints.
    pub fn label(&self) -> String {
        format!("{}!{}", self.dll, self.import.label())
    }
}

/// Every placeholder import of a program. See the module documentation.
#[derive(Clone, Debug, Default)]
pub struct PlaceholderTable {
    pub entries: Vec<Placeholder>,
    index: HashMap<(String, ImportKey), u32>,
}

impl PlaceholderTable {
    /// First address of the range.
    pub const BASE: u64 = PLACEHOLDER_BASE;
    /// One past the range.
    pub const END: u64 = PLACEHOLDER_END;
    /// Bytes per entry.
    pub const STRIDE: u64 = PLACEHOLDER_STRIDE;

    /// The entry an address is inside, if any.
    pub fn lookup(&self, addr: u64) -> Option<&Placeholder> {
        if !(PLACEHOLDER_BASE..PLACEHOLDER_END).contains(&addr) {
            return None;
        }
        self.entries.get(((addr - PLACEHOLDER_BASE) / PLACEHOLDER_STRIDE) as usize)
    }

    fn get(&mut self, dll: &str, import: &ImportName, reason: PlaceholderReason, via: &[String]) -> Result<u32> {
        let key = (dll.to_string(), import.key());
        if let Some(&i) = self.index.get(&key) {
            return Ok(i);
        }
        let i = self.entries.len() as u32;
        if i as u64 >= PLACEHOLDER_CAPACITY {
            return Err(PeError::layout(format!(
                "more than {PLACEHOLDER_CAPACITY} distinct placeholder imports"
            )));
        }
        self.entries.push(Placeholder {
            index: i,
            addr: PLACEHOLDER_BASE + i as u64 * PLACEHOLDER_STRIDE,
            dll: dll.to_string(),
            import: import.clone(),
            reason,
            via: via.to_vec(),
            slots: Vec::new(),
        });
        self.index.insert(key, i);
        Ok(i)
    }
}

// ---------------------------------------------------------------------------
// The closure
// ---------------------------------------------------------------------------

/// Why a module is in the closure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The program itself.
    Executable,
    /// Named in another module's import table.
    Import { by: String },
    /// The target of another module's export forwarder.
    Forwarder { by: String },
    /// Named in another module's delay-import table.
    Delay { by: String },
}

/// Where an import resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// An export of a loaded image.
    Image {
        /// Index into [`PeProgram::modules`].
        module: usize,
        /// The export's address.
        va: u64,
        /// The export was reached through forwarders: the chain it followed.
        via: Vec<String>,
    },
    /// An index into [`PeProgram::placeholders`].
    Placeholder(u32),
}

/// One bound IAT slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundSlot {
    /// As the importing image spells it.
    pub dll: String,
    pub import: ImportName,
    /// Guest address of the IAT slot.
    pub slot: u64,
    pub target: Target,
}

/// One delay-import slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelaySlot {
    pub dll: String,
    pub import: ImportName,
    /// Guest address of the delay IAT slot.
    pub slot: u64,
    /// What the slot holds until the helper runs: the relocated thunk.
    pub thunk: u64,
    /// Guest address of the descriptor and of its module-handle cell.
    pub descriptor: u64,
    pub module_handle: u64,
    /// Where it would resolve once the helper binds it.
    pub target: Target,
}

/// One image of a loaded program.
#[derive(Clone, Debug)]
pub struct PeModule {
    /// Where it was found.
    pub path: String,
    /// Lower-case basename: the key imports find it by.
    pub name: String,
    pub reason: Reason,
    pub pe: PeImage,
    pub base: u64,
    /// The mapped image: relocated, IAT bound, `ImageBase` updated.
    pub image: Vec<u8>,
    pub segments: Vec<Segment>,
    /// `pe.relocs` were applied (the image moved).
    pub rebased: bool,
    pub imports: Vec<BoundSlot>,
    pub delay_imports: Vec<DelaySlot>,
    /// TLS callbacks, as guest addresses, in call order.
    pub tls_callbacks: Vec<u64>,
}

impl PeModule {
    /// The entry point (`main`'s CRT startup for an executable, `DllMain`
    /// for a DLL), if it has one.
    pub fn entry(&self) -> Option<u64> {
        (self.pe.entry_rva != 0).then(|| self.base + self.pe.entry_rva as u64)
    }
    /// One past the last byte it occupies.
    pub fn end(&self) -> u64 {
        self.base + align_up(self.pe.image_size as u64, IMAGE_ALIGN)
    }
    /// Exports as `(label, address)`, labels being names and `#ordinal`s,
    /// forwarders excluded.
    pub fn export_addresses(&self) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        for e in self.pe.exports.entries.iter().filter(|e| e.rva != 0 && e.forwarder.is_none()) {
            let va = self.base + e.rva as u64;
            for n in &e.names {
                out.push((n.clone(), va));
            }
            out.push((format!("#{}", e.ordinal), va));
        }
        out
    }
}

/// A PE program and its DLL closure, placed and bound.
#[derive(Clone, Debug)]
pub struct PeProgram {
    /// The executable first.
    pub modules: Vec<PeModule>,
    pub placeholders: PlaceholderTable,
    /// Non-system DLLs that were named but not found, with who named them.
    pub missing: Vec<(String, String)>,
}

impl PeProgram {
    /// The executable's entry point.
    pub fn entry(&self) -> u64 {
        self.modules[0].entry().unwrap_or(0)
    }
    /// Every range an image occupies, sorted: what an allocator must avoid.
    pub fn occupied(&self) -> Vec<(u64, u64)> {
        let mut v: Vec<(u64, u64)> = self.modules.iter().map(|m| (m.base, m.end())).collect();
        v.sort();
        v
    }
    /// The module holding a guest address.
    pub fn module_at(&self, va: u64) -> Option<&PeModule> {
        self.modules.iter().find(|m| m.base <= va && va < m.end())
    }
}

/// How to build a closure.
#[derive(Clone, Debug, Default)]
pub struct PeOptions {
    /// Searched after the executable's own directory, in order.
    pub search_dirs: Vec<String>,
    pub layout: PeLayout,
    /// System DLLs to load from disk after all (e.g. Wine's PE builds),
    /// lower-case with `.dll`.
    pub native_dlls: HashSet<String>,
    /// Extra DLLs to load (as `LoadLibrary` would at run time), by path.
    pub extra_dlls: Vec<String>,
    /// Do not follow delay imports into the closure.
    pub skip_delay_dlls: bool,
}

/// How the closure reads files: guest paths, like [`crate::closure`].
pub trait PeFiles {
    fn read(&self, path: &str) -> Option<Vec<u8>>;
    /// Entry names in a directory, for case-insensitive lookup. An empty
    /// list falls back to trying the name as spelled and lower-cased.
    fn list(&self, dir: &str) -> Vec<String>;
}

/// The host file system.
pub struct HostFiles;

impl PeFiles for HostFiles {
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }
    fn list(&self, dir: &str) -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|rd| rd.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect())
            .unwrap_or_default()
    }
}

fn dirname(path: &str) -> &str {
    match path.rfind(['/', '\\']) {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => ".",
    }
}

/// Finds DLLs by case-insensitive name in the search directories.
struct Finder<'a> {
    files: &'a dyn PeFiles,
    dirs: Vec<String>,
    listings: HashMap<String, HashMap<String, String>>,
}

impl Finder<'_> {
    fn find(&mut self, key: &str) -> Option<(String, Vec<u8>)> {
        for d in self.dirs.clone() {
            let listing = self
                .listings
                .entry(d.clone())
                .or_insert_with(|| {
                    self.files.list(&d).into_iter().map(|n| (n.to_ascii_lowercase(), n)).collect()
                })
                .clone();
            let candidates: Vec<String> = match listing.get(key) {
                Some(n) => vec![n.clone()],
                None if listing.is_empty() => vec![key.to_string()],
                None => vec![],
            };
            for n in candidates {
                let p = format!("{}/{}", d.trim_end_matches('/'), n);
                if let Some(b) = self.files.read(&p) {
                    return Some((p, b));
                }
            }
        }
        None
    }
}

/// Load `exe_path` and every DLL it reaches, place, relocate and bind them.
pub fn load_closure(exe_path: &str, files: &dyn PeFiles, opts: &PeOptions) -> Result<PeProgram> {
    let exe_bytes = files
        .read(exe_path)
        .ok_or_else(|| PeError::malformed("cannot read the file").in_file(exe_path))?;
    let mut dirs = vec![dirname(exe_path).to_string()];
    dirs.extend(opts.search_dirs.iter().cloned());
    let mut finder = Finder { files, dirs, listings: HashMap::new() };
    let wants_disk = |key: &str| !is_system_dll(key) || opts.native_dlls.contains(key);

    // Parse the closure breadth-first.
    let mut parsed: Vec<(String, String, Reason, PeImage)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut missing = Vec::new();
    let exe = parse(&exe_bytes).map_err(|e| e.in_file(exe_path))?;
    if exe.is_dll() {
        return Err(PeError::malformed("a DLL, not an executable").in_file(exe_path));
    }
    let exe_name = normalize_dll(exe_path);
    seen.insert(exe_name.clone());
    parsed.push((exe_path.to_string(), exe_name, Reason::Executable, exe));
    let mut queue: Vec<(String, Reason)> = Vec::new();
    let mut extras_queued = false;
    let mut next = 0usize;
    let mut qi = 0usize;
    loop {
        // Enqueue what the most recently parsed modules name.
        while next < parsed.len() {
            let (_, name, _, pe) = &parsed[next];
            for d in &pe.imports {
                queue.push((d.dll.clone(), Reason::Import { by: name.clone() }));
            }
            for e in &pe.exports.entries {
                if let Some(f) = &e.forwarder {
                    if let Some((dll, _)) = f.rsplit_once('.') {
                        queue.push((dll.to_string(), Reason::Forwarder { by: name.clone() }));
                    }
                }
            }
            if !opts.skip_delay_dlls {
                for d in &pe.delay_imports {
                    queue.push((d.dll.clone(), Reason::Delay { by: name.clone() }));
                }
            }
            next += 1;
        }
        if qi >= queue.len() {
            // Extra DLLs come after everything the program names, as a
            // `LoadLibrary` at run time would.
            if extras_queued || opts.extra_dlls.is_empty() {
                break;
            }
            extras_queued = true;
            for p in &opts.extra_dlls {
                queue.push((p.clone(), Reason::Import { by: "(extra)".into() }));
            }
            continue;
        }
        let (spec, reason) = queue[qi].clone();
        qi += 1;
        let key = normalize_dll(&spec);
        if seen.contains(&key) || !wants_disk(&key) {
            continue;
        }
        seen.insert(key.clone());
        let found = if spec.contains('/') {
            files.read(&spec).map(|b| (spec.clone(), b))
        } else {
            finder.find(&key)
        };
        let Some((path, bytes)) = found else {
            let by = match &reason {
                Reason::Import { by } | Reason::Forwarder { by } | Reason::Delay { by } => by.clone(),
                Reason::Executable => String::new(),
            };
            missing.push((key, by));
            continue;
        };
        let pe = parse(&bytes).map_err(|e| e.in_file(&path))?;
        if !pe.is_dll() {
            return Err(PeError::malformed("imported as a DLL but is not one").in_file(&path));
        }
        if pe.is64 != parsed[0].3.is64 || pe.machine != parsed[0].3.machine {
            return Err(PeError::unsupported(format!(
                "machine {:#x} mixed with an executable for {:#x}",
                pe.machine, parsed[0].3.machine
            ))
            .in_file(&path));
        }
        parsed.push((path, key, reason, pe));
    }

    // Place and map.
    let mut placer = Placer { layout: &opts.layout, taken: Vec::new() };
    let mut modules = Vec::with_capacity(parsed.len());
    for (path, name, reason, pe) in parsed {
        let base = placer.place(&name, &pe).map_err(|e| e.in_file(&path))?;
        let image = pe.map(base).map_err(|e| e.in_file(&path))?;
        modules.push(PeModule {
            path,
            name,
            reason,
            rebased: base != pe.preferred_base,
            base,
            image,
            segments: Vec::new(),
            imports: Vec::new(),
            delay_imports: Vec::new(),
            tls_callbacks: Vec::new(),
            pe,
        });
    }

    // Bind.
    let by_name: HashMap<String, usize> =
        modules.iter().enumerate().map(|(i, m)| (m.name.clone(), i)).collect();
    let mut table = PlaceholderTable::default();
    for mi in 0..modules.len() {
        let mut bound = Vec::new();
        for d in &modules[mi].pe.imports {
            for s in &d.symbols {
                let target = resolve(&modules, &by_name, &opts.native_dlls, &mut table, &d.dll, &s.import)?;
                let slot = modules[mi].base + s.iat_rva as u64;
                bound.push(BoundSlot { dll: d.dll.clone(), import: s.import.clone(), slot, target });
            }
        }
        let mut delayed = Vec::new();
        for d in &modules[mi].pe.delay_imports {
            for s in &d.symbols {
                let target = resolve(&modules, &by_name, &opts.native_dlls, &mut table, &d.dll, &s.import)?;
                let m = &modules[mi];
                let thunk = read_ptr(&m.image, s.iat_rva as usize, m.pe.is64);
                delayed.push(DelaySlot {
                    dll: d.dll.clone(),
                    import: s.import.clone(),
                    slot: m.base + s.iat_rva as u64,
                    thunk,
                    descriptor: m.base + d.descriptor_rva as u64,
                    module_handle: m.base + d.module_handle_rva as u64,
                    target,
                });
            }
        }
        let m = &mut modules[mi];
        for b in &bound {
            let value = match &b.target {
                Target::Image { va, .. } => *va,
                Target::Placeholder(i) => {
                    table.entries[*i as usize].slots.push(PlaceholderSlot { module: mi, slot: b.slot, delay: false });
                    table.entries[*i as usize].addr
                }
            };
            write_ptr(&mut m.image, (b.slot - m.base) as usize, m.pe.is64, value);
        }
        for d in &delayed {
            if let Target::Placeholder(i) = d.target {
                table.entries[i as usize].slots.push(PlaceholderSlot { module: mi, slot: d.slot, delay: true });
            }
            let off = (d.thunk.wrapping_sub(m.base)) as u32;
            if !m.pe.sections.iter().any(|s| s.executable() && s.contains(off)) {
                return Err(PeError::malformed(format!(
                    "delay-import thunk {:#x} for {}!{} is not in an executable section",
                    d.thunk,
                    d.dll,
                    d.import.label()
                ))
                .in_file(&m.path));
            }
        }
        m.imports = bound;
        m.delay_imports = delayed;
        m.tls_callbacks = tls_callbacks(m).map_err(|e| e.in_file(&m.path))?;
        m.segments = m.pe.segments(m.base, &m.image);
    }
    Ok(PeProgram { modules, placeholders: table, missing })
}

fn read_ptr(img: &[u8], at: usize, is64: bool) -> u64 {
    if is64 {
        u64::from_le_bytes(img[at..at + 8].try_into().unwrap())
    } else {
        u32::from_le_bytes(img[at..at + 4].try_into().unwrap()) as u64
    }
}

fn write_ptr(img: &mut [u8], at: usize, is64: bool, v: u64) {
    if is64 {
        img[at..at + 8].copy_from_slice(&v.to_le_bytes());
    } else {
        img[at..at + 4].copy_from_slice(&(v as u32).to_le_bytes());
    }
}

/// Resolve `dll!import`, following forwarders.
fn resolve(
    modules: &[PeModule],
    by_name: &HashMap<String, usize>,
    native: &HashSet<String>,
    table: &mut PlaceholderTable,
    dll: &str,
    import: &ImportName,
) -> Result<Target> {
    let mut key = normalize_dll(dll);
    let mut import = import.clone();
    let mut via: Vec<String> = Vec::new();
    for _ in 0..32 {
        let loaded = by_name.get(&key).copied();
        let Some(mi) = loaded else {
            let reason = if is_system_dll(&key) && !native.contains(&key) {
                PlaceholderReason::System
            } else {
                PlaceholderReason::MissingDll
            };
            return table.get(&key, &import, reason, &via).map(Target::Placeholder);
        };
        let m = &modules[mi];
        let Some(e) = m.pe.exports.find(&import) else {
            return table
                .get(&key, &import, PlaceholderReason::MissingExport, &via)
                .map(Target::Placeholder);
        };
        let Some(fwd) = &e.forwarder else {
            return Ok(Target::Image { module: mi, va: m.base + e.rva as u64, via });
        };
        via.push(format!("{key}!{}", import.label()));
        let (tdll, tsym) = fwd
            .rsplit_once('.')
            .ok_or_else(|| PeError::malformed(format!("forwarder {fwd:?} has no DLL part")).in_file(&m.path))?;
        key = normalize_dll(tdll);
        import = match tsym.strip_prefix('#') {
            Some(n) => ImportName::Ordinal(
                n.parse().map_err(|_| PeError::malformed(format!("forwarder {fwd:?}")).in_file(&m.path))?,
            ),
            None => ImportName::Name { name: tsym.to_string(), hint: 0 },
        };
    }
    Err(PeError::malformed(format!("forwarder chain from {dll}!{} is cyclic or longer than 32", via[0])))
}

/// A placed module's TLS callbacks, read from the relocated image.
fn tls_callbacks(m: &PeModule) -> Result<Vec<u64>> {
    let Some(t) = &m.pe.tls else { return Ok(Vec::new()) };
    if t.callbacks_va == 0 {
        return Ok(Vec::new());
    }
    let w = if m.pe.is64 { 8 } else { 4 };
    let mut off = (t.callbacks_va - m.pe.preferred_base) as usize;
    let mut out = Vec::new();
    loop {
        if off + w > m.image.len() || out.len() >= 1024 {
            return Err(PeError::malformed("TLS callback table is unterminated"));
        }
        let cb = read_ptr(&m.image, off, m.pe.is64);
        if cb == 0 {
            return Ok(out);
        }
        let rva = cb.wrapping_sub(m.base);
        if rva > u32::MAX as u64 || !m.pe.sections.iter().any(|s| s.executable() && s.contains(rva as u32)) {
            return Err(PeError::malformed(format!(
                "TLS callback {cb:#x} is outside the executable sections"
            )));
        }
        out.push(cb);
        off += w;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dll_names_normalize() {
        assert_eq!(normalize_dll("KERNEL32.dll"), "kernel32.dll");
        assert_eq!(normalize_dll("libb"), "libb.dll");
        assert_eq!(normalize_dll("C:\\x\\Foo.DRV"), "foo.drv");
        assert!(is_system_dll("KERNEL32"));
        assert!(is_system_dll("api-ms-win-crt-stdio-l1-1-0.dll"));
        assert!(!is_system_dll("msvcr120.dll"));
    }

    #[test]
    fn placeholders_are_deduplicated_and_found_by_address() {
        let mut t = PlaceholderTable::default();
        let n = |s: &str| ImportName::Name { name: s.into(), hint: 0 };
        let a = t.get("kernel32.dll", &n("HeapAlloc"), PlaceholderReason::System, &[]).unwrap();
        let b = t.get("kernel32.dll", &ImportName::Name { name: "HeapAlloc".into(), hint: 9 }, PlaceholderReason::System, &[]).unwrap();
        let c = t.get("kernel32.dll", &ImportName::Ordinal(7), PlaceholderReason::System, &[]).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        let e = t.lookup(PLACEHOLDER_BASE + PLACEHOLDER_STRIDE + 3).unwrap();
        assert_eq!(e.label(), "kernel32.dll!#7");
        assert!(t.lookup(PLACEHOLDER_BASE + 2 * PLACEHOLDER_STRIDE).is_none());
        assert!(t.lookup(gex_abi::EXEC_BASE).is_none());
    }

    #[test]
    fn first_fit_skips_images_and_reserved_ranges() {
        let layout = PeLayout::default();
        let mut p = Placer { layout: &layout, taken: vec![(0x40_0000, 0x42_0000)] };
        assert_eq!(p.first_fit(0x1_0000), Some(0x42_0000));
        p.taken.push((0x42_0000, gex_abi::STACK_TOP - gex_abi::STACK_SIZE));
        // The stack is reserved, so the next gap is above it.
        assert_eq!(p.first_fit(0x1_0000), Some(gex_abi::STACK_TOP));
    }

    #[test]
    fn non_pe_input_is_refused() {
        assert_eq!(parse(b"\x7fELF").unwrap_err().kind, PeErrorKind::Malformed);
        assert!(!is_pe(&[0u8; 128]));
    }
}
