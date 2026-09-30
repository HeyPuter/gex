//! Linking a program on the host instead of running its dynamic linker.
//!
//! Loads the executable and everything it needs, assigns each object an
//! address, builds one symbol table across the lot, and applies every
//! relocation in native code. The guest then starts at the executable's entry
//! with its GOT already filled in, avoiding the per-process cost of
//! [`crate::dynamic`]'s cooperative mode.
//!
//! Two things are guest code and are left for the caller to run at startup:
//! `STT_GNU_IFUNC` resolvers (collected as `IRELATIVE`) and the `DT_INIT_ARRAY`
//! initialisers.
//!
//! What it cannot do: glibc's `libc.so.6` reads fields of `_rtld_global(_ro)`
//! that live in `ld.so`, and a field at an offset is not a symbol the ELF
//! locates. So on Debian this links every relocation and still dies in
//! `malloc`; it replaces the loader only for static-PIE and musl.

use crate::elf::{load, DynSym, Image, LoadError, Reloc};
use std::collections::HashMap;

// The relocation types an aarch64 program actually contains. Anything else is
// reported rather than ignored: a relocation nobody applied is a word the
// guest will read as zero, and the failure lands far away from the cause.
use crate::elf::reloc::{
    ABS64 as R_AARCH64_ABS64, COPY as R_AARCH64_COPY, GLOB_DAT as R_AARCH64_GLOB_DAT,
    IRELATIVE as R_AARCH64_IRELATIVE, JUMP_SLOT as R_AARCH64_JUMP_SLOT,
    RELATIVE as R_AARCH64_RELATIVE, TLSDESC as R_AARCH64_TLSDESC,
    TLS_DTPMOD64 as R_AARCH64_TLS_DTPMOD64, TLS_DTPREL64 as R_AARCH64_TLS_DTPREL64,
    TLS_TPREL64 as R_AARCH64_TLS_TPREL64,
};

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
    /// `(slot, offset)` pairs for TLS descriptors: `slot` is the two-word
    /// descriptor, `offset` the thread-pointer-relative value its resolver
    /// returns. The caller writes both words (the resolver is guest code, a
    /// two-instruction `_dl_tlsdesc_return`). Static case only, which is all a
    /// non-`dlopen` program produces.
    pub tlsdesc: Vec<(u64, u64)>,
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
    // x86-64 is variant II: the blocks sit *below* the thread pointer, each
    // module's block ending where the previous one begins, so an offset is
    // negative (stored here as the wrapping `u64` the relocation wants).
    let mut tls_size = if gex_abi::X86 { 0u64 } else { 16u64 };
    let mut tls_align = 8u64;
    for o in objects.iter_mut() {
        let Some((_, _, memsz, align)) = o.image.tls else { continue };
        let align = align.max(1);
        tls_align = tls_align.max(align);
        if gex_abi::X86 {
            tls_size = (tls_size + memsz + align - 1) / align * align;
            o.tls_offset = Some(tls_size.wrapping_neg());
        } else {
            tls_size = (tls_size + align - 1) / align * align;
            o.tls_offset = Some(tls_size);
            tls_size += memsz;
        }
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

    let mut tlsdesc: Vec<(u64, u64)> = Vec::new();
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
                    // Two words: a resolver and its argument. In the static
                    // case the resolver returns the argument unchanged, so the
                    // argument is the same thread-pointer offset `TPREL64`
                    // computes. The caller writes the pair (resolver is guest
                    // code).
                    let (base, off) = match found {
                        Some(d) if d.tls => {
                            (objects[d.obj].tls_offset.unwrap_or(0), d.value)
                        }
                        _ => (o.tls_offset.unwrap_or(0), 0),
                    };
                    tlsdesc.push((r.at, base + off + r.addend as u64));
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
    Ok(Linked {
        objects,
        entry,
        irelative,
        tlsdesc,
        init,
        tls_size,
        tls_align,
        unhandled,
        missing,
    })
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
    dirs.extend(crate::closure::DEFAULT_DIRS.iter().map(|s| s.to_string()));
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
