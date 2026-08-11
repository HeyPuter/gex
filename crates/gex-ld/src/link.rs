//! Linking a program instead of running its dynamic linker.
//!
//! [`crate::dynamic`] implements PLAN §6.4's *cooperative mode*: gex places
//! the executable and the interpreter and then jumps to the interpreter,
//! which does the real work -- searching for libraries, mapping them, hashing
//! symbols and applying relocations -- as translated guest code, once per
//! process.
//!
//! Measured on the demo site, that is half of a warm `ls`: 420ms of a 925ms
//! process, of which only about twenty milliseconds is inside syscalls. The
//! rest is glibc's linker computing, one emulated instruction at a time, an
//! answer gex already has. A closure pins every library at a fixed address at
//! build time; the layout is not discovered at run time, it is decided before
//! the run exists.
//!
//! So this module does the linking on the host. It loads the executable and
//! everything it needs, assigns each object an address, builds one symbol
//! table across the lot, and applies every relocation in native code. The
//! guest starts at the executable's entry with its GOT already filled in.
//!
//! Two things it cannot do, and does not pretend to:
//!
//! * **`STT_GNU_IFUNC`**. glibc resolves `memcpy`, `memmove` and `memset`
//!   through a resolver function chosen by the hardware it finds. The
//!   resolver is guest code and has to run, so `IRELATIVE` relocations are
//!   collected here and left for the caller to call at startup. There are a
//!   few dozen, against tens of thousands of ordinary relocations.
//! * **The initialisers**. `DT_INIT_ARRAY` is guest code too, and the same
//!   applies: collected in order, run by the caller before `main`.
//!
//! And one it cannot do at all, which is worth writing down because it is
//! what PLAN §6.4 was pointing at and it is not a matter of effort.
//!
//! glibc's `libc.so.6` reads `_rtld_global` and `_rtld_global_ro`, two
//! structures that live in `ld.so`. Resolving them is easy -- they are
//! ordinary symbols, and the linker loads the interpreter like any other
//! library so that it can define them. *Filling them in* is not: `GLRO
//! (dl_pagesize)`, which `malloc` divides by, is a field at some offset
//! inside `_rtld_global_ro`, and a field is not a symbol. Nothing in the ELF
//! says where it is. The same goes for `struct pthread`, which the thread
//! pointer must point just past, and for `__libc_early_init`, which `ld.so`
//! calls to bring stdio and locale up. All three are private layouts that
//! change between glibc releases and are readable only by being glibc.
//!
//! So on a Debian rootfs this module links every relocation correctly and the
//! program still dies in `malloc`. It replaces the loader for static-PIE and
//! for musl, where the loader state has no such private interface. For glibc
//! the way to stop paying for the interpreter is not to reimplement it but to
//! run it once and keep what it produced, and this module is the half of that
//! which decides where everything goes.

use crate::elf::{load, DynSym, Image, LoadError, Reloc};
use std::collections::HashMap;

// The relocation types an aarch64 program actually contains. Anything else is
// reported rather than ignored: a relocation nobody applied is a word the
// guest will read as zero, and the failure lands far away from the cause.
const R_AARCH64_ABS64: u32 = 257;
const R_AARCH64_COPY: u32 = 1024;
const R_AARCH64_GLOB_DAT: u32 = 1025;
const R_AARCH64_JUMP_SLOT: u32 = 1026;
const R_AARCH64_RELATIVE: u32 = 1027;
const R_AARCH64_TLS_TPREL64: u32 = 1030;
const R_AARCH64_TLSDESC: u32 = 1031;
const R_AARCH64_IRELATIVE: u32 = 1032;
const R_AARCH64_TLS_DTPMOD64: u32 = 1028;
const R_AARCH64_TLS_DTPREL64: u32 = 1029;

/// One object of a linked program.
pub struct Object {
    /// The guest path it was loaded from.
    pub path: String,
    pub image: Image,
    /// Offset of this object's block within the static TLS area, when it has
    /// a `PT_TLS`. Measured *downwards* from the thread pointer, as aarch64
    /// variant I wants it.
    pub tls_offset: Option<u64>,
}

/// A program with every relocation applied.
pub struct Linked {
    pub objects: Vec<Object>,
    /// Where to start: the executable's entry, not an interpreter's.
    pub entry: u64,
    /// `(slot, resolver)` pairs the guest must run and store, in order.
    pub irelative: Vec<(u64, u64)>,
    /// Initialisers to call before `main`, dependencies first.
    pub init: Vec<u64>,
    /// How much static TLS the program needs, and how it is aligned.
    pub tls_size: u64,
    pub tls_align: u64,
    /// Relocations no rule here covers, as `(object, type, count)`. Empty on
    /// a program this module can honestly claim to have linked.
    pub unhandled: Vec<(String, u32, usize)>,
    /// Symbols nothing defined, which a `weak` reference tolerates and a
    /// strong one does not.
    pub missing: Vec<String>,
}

/// Where a symbol ended up.
#[derive(Clone, Copy)]
struct Def {
    value: u64,
    /// Index into `objects`, for TLS relocations that need the defining
    /// object's module rather than an address.
    obj: usize,
    tls: bool,
    ifunc: bool,
}

/// Load `path` and everything it needs, and relocate the lot.
///
/// `read` resolves a guest path to bytes. `place` is asked where each shared
/// object goes and must answer the same way the run will -- for a closure
/// that is the pinned address, and getting it wrong produces a program whose
/// every cross-object call is off by the difference.
pub fn link(
    bytes: &[u8],
    path: &str,
    pie_base: u64,
    read: &dyn Fn(&str) -> Option<Vec<u8>>,
    place: &mut dyn FnMut(&str, u64) -> u64,
) -> Result<Linked, LoadError> {
    let exe = load(bytes, pie_base)?;
    let mut objects = vec![Object { path: path.to_string(), image: exe, tls_offset: None }];

    // Breadth first, which is the order a symbol lookup has to see: the
    // executable wins over any library, and a library over anything it
    // itself needs. glibc's `ld.so` resolves in exactly this order and a
    // program that defines its own `malloc` depends on it.
    let mut seen: Vec<String> = vec![path.to_string()];
    let mut queue: Vec<String> = objects[0].image.needed.clone();
    if let Some(interp) = objects[0].image.interp.clone() {
        // The interpreter is loaded and relocated and never entered. glibc's
        // libc reaches into it for `_rtld_global` and friends, so leaving it
        // out leaves those undefined; loading it costs a relocation pass and
        // answers them.
        queue.push(interp);
    }
    let mut i = 0;
    while i < queue.len() {
        let name = queue[i].clone();
        i += 1;
        if seen.iter().any(|s| s.ends_with(&name) || *s == name) {
            continue;
        }
        let Some((found, data)) = find(&name, &objects, read) else {
            continue;
        };
        let span = {
            let probe = load(&data, 0)?;
            probe.brk
        };
        let at = place(&found, span);
        let img = load(&data, at)?;
        queue.extend(img.needed.iter().cloned());
        seen.push(found.clone());
        objects.push(Object { path: found, image: img, tls_offset: None });
    }

    // The static TLS layout, before relocation, because `TPREL64` needs it.
    //
    // aarch64 is variant I: the thread pointer points at the thread control
    // block and each module's block sits above it, so an offset is positive
    // and the first block starts after two pointers of reserved header.
    let mut tls_size = 16u64;
    let mut tls_align = 8u64;
    for o in objects.iter_mut() {
        let Some((_, _, memsz, align)) = o.image.tls else { continue };
        let align = align.max(1);
        tls_align = tls_align.max(align);
        tls_size = (tls_size + align - 1) / align * align;
        o.tls_offset = Some(tls_size);
        tls_size += memsz;
    }

    // One symbol table across every object, in load order, so the first
    // definition wins.
    let mut defs: HashMap<String, Def> = HashMap::new();
    for (n, o) in objects.iter().enumerate() {
        for sym in &o.image.dynsyms {
            if sym.undefined || sym.name.is_empty() {
                continue;
            }
            defs.entry(sym.name.clone()).or_insert(Def {
                value: if sym.tls { sym.value } else { sym.value + o.image.bias },
                obj: n,
                tls: sym.tls,
                ifunc: sym.ifunc,
            });
        }
    }

    let mut irelative = Vec::new();
    let mut unhandled: HashMap<(String, u32), usize> = HashMap::new();
    let mut missing = Vec::new();
    let mut writes: Vec<(usize, u64, u64)> = Vec::new();

    for (n, o) in objects.iter().enumerate() {
        let bias = o.image.bias;
        for r in &o.image.relocs {
            let sym: Option<&DynSym> = o.image.dynsyms.get(r.sym as usize);
            let name = sym.map(|s| s.name.as_str()).unwrap_or("");
            let found = if name.is_empty() { None } else { defs.get(name).copied() };
            match r.kind {
                R_AARCH64_RELATIVE => {
                    writes.push((n, r.at, bias.wrapping_add(r.addend as u64)));
                }
                R_AARCH64_GLOB_DAT | R_AARCH64_JUMP_SLOT | R_AARCH64_ABS64 => {
                    match found {
                        Some(d) if !d.tls => {
                            writes.push((n, r.at, d.value.wrapping_add(r.addend as u64)));
                        }
                        _ => {
                            // An undefined weak is zero by definition and is
                            // how glibc asks "is this feature present".
                            if sym.map(|s| s.weak).unwrap_or(false) {
                                writes.push((n, r.at, 0));
                            } else if !name.is_empty() {
                                missing.push(format!("{name} for {}", o.path));
                            }
                        }
                    }
                }
                R_AARCH64_IRELATIVE => {
                    irelative.push((r.at, bias.wrapping_add(r.addend as u64)));
                }
                R_AARCH64_TLS_TPREL64 => {
                    // The offset from the thread pointer: the defining
                    // object's block, plus the symbol's offset in it.
                    let (base, off) = match found {
                        Some(d) if d.tls => (
                            objects[d.obj].tls_offset.unwrap_or(0),
                            d.value,
                        ),
                        _ => (o.tls_offset.unwrap_or(0), 0),
                    };
                    writes.push((n, r.at, base + off + r.addend as u64));
                }
                R_AARCH64_TLS_DTPMOD64 => {
                    // Module id. One module per object, numbered from one,
                    // which is what `__tls_get_addr` will be handed.
                    let m = found.map(|d| d.obj).unwrap_or(n) as u64 + 1;
                    writes.push((n, r.at, m));
                }
                R_AARCH64_TLS_DTPREL64 => {
                    let off = found.filter(|d| d.tls).map(|d| d.value).unwrap_or(0);
                    writes.push((n, r.at, off.wrapping_add(r.addend as u64)));
                }
                R_AARCH64_COPY => {
                    // The executable's copy of a library's variable. The
                    // bytes are copied after every other object is placed,
                    // which is why this is a second pass below.
                }
                R_AARCH64_TLSDESC => {
                    // Two words: a resolver and its argument. The static case
                    // is the only one a non-`dlopen` program produces, and
                    // its resolver returns the argument unchanged -- so the
                    // guest must still call it, and it is left for the caller
                    // alongside the ifuncs.
                    *unhandled.entry((o.path.clone(), r.kind)).or_insert(0) += 1;
                }
                other => {
                    *unhandled.entry((o.path.clone(), other)).or_insert(0) += 1;
                }
            }
        }
    }

    for (n, at, value) in writes {
        write(&mut objects[n].image, at, value);
    }

    // `COPY` last: it moves bytes out of a library that the loop above has
    // just finished relocating.
    let mut copies: Vec<(usize, u64, u64, u64)> = Vec::new();
    for (n, o) in objects.iter().enumerate() {
        for r in o.image.relocs.iter().filter(|r| r.kind == R_AARCH64_COPY) {
            let Some(sym) = o.image.dynsyms.get(r.sym as usize) else { continue };
            // The definition to copy from is the *library's*. The global
            // table cannot answer this one: it holds the executable's own
            // `.bss` placeholder, which is the destination, so look past it.
            let from = objects.iter().enumerate().skip(1).find_map(|(m, other)| {
                if m == n {
                    return None;
                }
                let d = other
                    .image
                    .dynsyms
                    .iter()
                    .find(|s| !s.undefined && s.name == sym.name)?;
                Some(d.value + other.image.bias)
            });
            let Some(from) = from else { continue };
            copies.push((n, r.at, from, sym.size));
        }
    }
    for (n, at, from, size) in copies {
        for k in (0..size).step_by(8) {
            let take = objects
                .iter()
                .find_map(|o| read_at(&o.image, from + k))
                .unwrap_or(0);
            write(&mut objects[n].image, at + k, take);
        }
    }

    // Initialisers run dependencies first, which is the reverse of the order
    // they were loaded in.
    let mut init = Vec::new();
    for o in objects.iter().skip(1).rev() {
        init.extend(o.image.init_array.iter().copied());
    }
    init.extend(objects[0].image.init_array.iter().copied());

    let entry = objects[0].image.entry;
    let mut unhandled: Vec<(String, u32, usize)> =
        unhandled.into_iter().map(|((p, k), c)| (p, k, c)).collect();
    unhandled.sort();
    missing.sort();
    missing.dedup();
    Ok(Linked { objects, entry, irelative, init, tls_size, tls_align, unhandled, missing })
}

/// Find a library by soname, the way `ld.so` would.
fn find(
    name: &str,
    objects: &[Object],
    read: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> Option<(String, Vec<u8>)> {
    if name.starts_with('/') {
        return read(name).map(|b| (name.to_string(), b));
    }
    let mut dirs: Vec<String> = Vec::new();
    for o in objects {
        for d in &o.image.runpath {
            dirs.push(d.clone());
        }
    }
    dirs.extend(
        [
            "/lib/aarch64-linux-gnu",
            "/usr/lib/aarch64-linux-gnu",
            "/lib",
            "/usr/lib",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    for d in dirs {
        let p = format!("{}/{name}", d.trim_end_matches('/'));
        if let Some(b) = read(&p) {
            return Some((p, b));
        }
    }
    None
}

fn read_at(image: &Image, vaddr: u64) -> Option<u64> {
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

/// Write into a segment, growing its bytes if the fixup lands in the part of
/// a `PT_LOAD` that has no file contents.
///
/// `.bss` is inside `mem_size` and outside `bytes`, and a `RELATIVE`
/// relocation into it is ordinary -- glibc's `__libc_stack_end` is one. A
/// write that silently did nothing would leave a pointer reading as zero.
fn write(image: &mut Image, vaddr: u64, value: u64) {
    for seg in &mut image.segments {
        if vaddr >= seg.vaddr && vaddr + 8 <= seg.vaddr + seg.mem_size {
            let off = (vaddr - seg.vaddr) as usize;
            if seg.bytes.len() < off + 8 {
                seg.bytes.resize(off + 8, 0);
            }
            seg.bytes[off..off + 8].copy_from_slice(&value.to_le_bytes());
            return;
        }
    }
}
