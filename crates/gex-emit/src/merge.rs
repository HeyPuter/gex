//! Concatenating translation units into one module.
//!
//! Every unit is [`crate::module::ModuleBuilder`]'s output with the same
//! imports in the same order and one function type for every block, so units
//! concatenate rather than link. A body may name another block directly, and
//! each unit carries a `gex.reloc` custom section giving the offset of every
//! such immediate (written at a fixed five bytes so patching cannot change a
//! body's length). So:
//!
//!   * types, imports, tables, memory and data come from the first unit,
//!     because they are identical in all of them;
//!   * function and code sections concatenate verbatim;
//!   * exports and element segments carry local function indices, which shift
//!     as bodies are appended, so those are renumbered.

/// Section ids we have to understand. The rest are copied from unit zero.
const SEC_FUNCTION: u8 = 3;
const SEC_EXPORT: u8 = 7;
const SEC_ELEMENT: u8 = 9;
const SEC_CODE: u8 = 10;

fn read_u32(b: &[u8], p: &mut usize) -> u32 {
    let mut v = 0u32;
    let mut shift = 0;
    loop {
        let byte = b[*p];
        *p += 1;
        v |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            return v;
        }
        shift += 7;
    }
}

fn write_u32(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            return;
        }
    }
}

/// `(id, payload)` for each section, in order.
fn sections(wasm: &[u8]) -> Option<Vec<(u8, &[u8])>> {
    if wasm.len() < 8 || &wasm[0..4] != b"\0asm" {
        return None;
    }
    let mut p = 8;
    let mut out = Vec::new();
    while p < wasm.len() {
        let id = wasm[p];
        p += 1;
        let len = read_u32(wasm, &mut p) as usize;
        if p + len > wasm.len() {
            return None;
        }
        out.push((id, &wasm[p..p + len]));
        p += len;
    }
    Some(out)
}

fn section(out: &mut Vec<u8>, id: u8, payload: &[u8]) {
    out.push(id);
    write_u32(out, payload.len() as u32);
    out.extend_from_slice(payload);
}

/// How many bytes `write_u32` will spend on `v`.
fn leb_len(mut v: u32) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

/// A section whose payload is a count followed by a body, written without
/// building the payload first.
///
/// The obvious spelling -- concatenate the count and the body into a `Vec`,
/// then `section()` that -- copies the body a second time, and for the code
/// section the body is the whole merged module. In the browser those copies are
/// not transient: the runtime's heap takes spans from an arena that never gives
/// one back, so each was megabytes of a guest process's address space gone for
/// good. Both lengths are known here, so neither copy is needed.
fn section_counted(out: &mut Vec<u8>, id: u8, count: u32, body: &[u8]) {
    out.push(id);
    write_u32(out, (leb_len(count) + body.len()) as u32);
    write_u32(out, count);
    out.extend_from_slice(body);
}

/// The offsets a unit recorded for its own local-function immediates, as
/// positions in its code section's payload *after* the body count.
fn relocs_of(secs: &[(u8, &[u8])]) -> Vec<u32> {
    for (id, payload) in secs {
        if *id != 0 {
            continue;
        }
        let mut p = 0;
        let len = read_u32(payload, &mut p) as usize;
        if &payload[p..p + len] != crate::wasm::RELOC_SECTION.as_bytes() {
            continue;
        }
        p += len;
        let n = read_u32(payload, &mut p) as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(u32::from_le_bytes(payload[p..p + 4].try_into().unwrap()));
            p += 4;
        }
        return out;
    }
    Vec::new()
}

/// How many functions a unit imports, which is where its local indices start.
fn import_funcs(secs: &[(u8, &[u8])]) -> u32 {
    let Some((_, imports)) = secs.iter().find(|(id, _)| *id == 2) else {
        return 0;
    };
    let mut p = 0;
    let n = read_u32(imports, &mut p);
    let mut funcs = 0;
    for _ in 0..n {
        for _ in 0..2 {
            let len = read_u32(imports, &mut p) as usize;
            p += len;
        }
        let kind = imports[p];
        p += 1;
        match kind {
            0x00 => {
                read_u32(imports, &mut p);
                funcs += 1;
            }
            0x01 => {
                p += 1; // reftype
                let flags = imports[p];
                p += 1;
                read_u32(imports, &mut p);
                if flags != 0 {
                    read_u32(imports, &mut p);
                }
            }
            0x02 => {
                let flags = imports[p];
                p += 1;
                read_u32(imports, &mut p);
                if flags & 0x01 != 0 {
                    read_u32(imports, &mut p);
                }
            }
            _ => {
                p += 1;
                read_u32(imports, &mut p);
            }
        }
    }
    funcs
}

/// Merge `units` into one module, or `None` if any of them is not the shape
/// this expects.
///
/// The first unit's types, imports, tables, memory and data become the merged
/// module's; a mismatch shows up as a differing import count and is rejected.
pub fn merge(units: &[&[u8]]) -> Option<Vec<u8>> {
    if units.is_empty() {
        return None;
    }
    if units.len() == 1 {
        return Some(units[0].to_vec());
    }
    let parsed: Vec<Vec<(u8, &[u8])>> = units.iter().map(|u| sections(u)).collect::<Option<_>>()?;
    // A profiled unit (`crate::profile`) carries a global and a data-count
    // section of its own, and its region descriptors; concatenating those is
    // not implemented, so refuse rather than produce a wrong module.
    if parsed.iter().any(|p| p.iter().any(|(id, _)| *id == 6 || *id == 12)) {
        return None;
    }
    // Nor are dense segments (`crate::dense`): each unit's is at its own base.
    if units.iter().any(|u| crate::dense::locate(u).is_some()) {
        return None;
    }
    let n_imports = import_funcs(&parsed[0]);
    for p in &parsed[1..] {
        if import_funcs(p) != n_imports {
            return None;
        }
    }

    // Sized from the inputs, never grown from empty.
    //
    // These are concatenations: each ends up about as long as the same section
    // across every unit, and that is known here. Growing from zero instead
    // reached the final size through a chain of doublings -- and in the browser
    // the runtime's heap takes its spans from an arena that never gives one
    // back, and cannot serve a request from two spans, so every doubling
    // stranded the one before it. A 64 MiB merge cost about 300 MiB of address
    // space that way, and since there is one memory per guest process and it
    // only grows, that was ~200 MiB on every process in the tab: a `$(echo)`
    // child held 199 MiB of heap spans with 0 MiB live in them, an `apt-get
    // install` peaked at 11 GB, and apt's forked `http` method died of an
    // allocation failure.
    //
    // A `+ len/2 + 64` for the renumbered sections: an index that moves up can
    // need another LEB byte, so their output can exceed their input slightly.
    // Overshooting the capacity is one realloc, not a chain.
    let cap_of = |id: u8| -> usize {
        parsed
            .iter()
            .filter_map(|secs| secs.iter().find(|(i, _)| *i == id))
            .map(|(_, payload)| payload.len())
            .sum()
    };
    // Functions and code, concatenated; and the offset each unit's local
    // indices move by, which is every earlier unit's local function count.
    let mut funcs = Vec::with_capacity(cap_of(SEC_FUNCTION) + 64);
    let mut n_funcs = 0u32;
    let mut n_bodies = 0u32;
    let mut offset_of = Vec::with_capacity(parsed.len());
    // The merged code payload's length, before a byte of it is written.
    //
    // The bodies go straight into `out` below rather than into a `code` vector
    // that is then copied in, and writing a section head needs its length
    // first. It is exactly each unit's code payload minus the body count it
    // starts with.
    let mut code_len = 0usize;
    for secs in &parsed {
        offset_of.push(n_funcs);
        if let Some((_, payload)) = secs.iter().find(|(id, _)| *id == SEC_FUNCTION) {
            let mut p = 0;
            let n = read_u32(payload, &mut p);
            n_funcs += n;
            funcs.extend_from_slice(&payload[p..]);
        }
        if let Some((_, payload)) = secs.iter().find(|(id, _)| *id == SEC_CODE) {
            let mut p = 0;
            let n = read_u32(payload, &mut p);
            n_bodies += n;
            code_len += payload.len() - p;
        }
    }
    if n_funcs != n_bodies {
        return None;
    }

    // Exports: unit zero's whole set, then any name from a later unit that is
    // not already taken -- which is every block, since a block's export name
    // is its guest address, and none of `enter` or the miss stub, which every
    // unit declares under the same name.
    let mut names: Vec<String> = Vec::new();
    let mut exports = Vec::with_capacity(cap_of(SEC_EXPORT) * 3 / 2 + 64);
    let mut n_exports = 0u32;
    for (i, secs) in parsed.iter().enumerate() {
        let Some((_, payload)) = secs.iter().find(|(id, _)| *id == SEC_EXPORT) else {
            continue;
        };
        let mut p = 0;
        let n = read_u32(payload, &mut p);
        for _ in 0..n {
            let len = read_u32(payload, &mut p) as usize;
            let name = String::from_utf8_lossy(&payload[p..p + len]).into_owned();
            p += len;
            let kind = payload[p];
            p += 1;
            let idx = read_u32(payload, &mut p);
            if names.contains(&name) {
                continue;
            }
            names.push(name.clone());
            n_exports += 1;
            write_u32(&mut exports, name.len() as u32);
            exports.extend_from_slice(name.as_bytes());
            exports.push(kind);
            write_u32(
                &mut exports,
                if kind == 0x00 && idx >= n_imports {
                    idx + offset_of[i]
                } else {
                    idx
                },
            );
        }
    }

    // Element segments, renumbered the same way. A segment's offset is an
    // absolute slot base and is already unique across processes, so segments
    // never collide and none has to be dropped.
    let mut elems = Vec::with_capacity(cap_of(SEC_ELEMENT) * 3 / 2 + 64);
    let mut n_elems = 0u32;
    for (i, secs) in parsed.iter().enumerate() {
        let Some((_, payload)) = secs.iter().find(|(id, _)| *id == SEC_ELEMENT) else {
            continue;
        };
        let mut p = 0;
        let n = read_u32(payload, &mut p);
        for _ in 0..n {
            let start = p;
            let flag = payload[p];
            p += 1;
            if flag == 0x02 {
                read_u32(payload, &mut p);
            }
            if flag == 0x01 {
                // Passive: an elemkind and then the vector, with no offset
                // expression. Treating it like an active segment's
                // `i32.const N end` reads three bytes out of the funcref
                // vector and walks the parse off the end of the section.
                p += 1; // elemkind
            } else {
                // The offset expression: `i32.const N end`.
                p += 1;
                read_u32(payload, &mut p);
                p += 1;
                if flag == 0x02 {
                    p += 1; // elemkind
                }
            }
            elems.extend_from_slice(&payload[start..p]);
            let count = read_u32(payload, &mut p);
            write_u32(&mut elems, count);
            for _ in 0..count {
                let f = read_u32(payload, &mut p);
                write_u32(
                    &mut elems,
                    if f >= n_imports { f + offset_of[i] } else { f },
                );
            }
            n_elems += 1;
        }
    }

    let mut out = Vec::with_capacity(units.iter().map(|u| u.len()).sum::<usize>() + 64);
    out.extend_from_slice(b"\0asm");
    out.extend_from_slice(&1u32.to_le_bytes());
    for (id, payload) in &parsed[0] {
        match *id {
            SEC_FUNCTION => section_counted(&mut out, SEC_FUNCTION, n_funcs, &funcs),
            SEC_CODE => {
                // Written where it belongs, once.
                //
                // A merged code section is the whole of every unit's code, and
                // it used to be built in a `code` vector and then copied into a
                // payload vector and then into `out` -- three copies of tens of
                // megabytes, each claiming a span of the runtime's arena that
                // nothing ever gives back. `code_len` is known above, so the
                // section head can be written first and the bodies appended
                // straight into `out`.
                out.push(SEC_CODE);
                write_u32(&mut out, (leb_len(n_bodies) + code_len) as u32);
                write_u32(&mut out, n_bodies);
                for (i, secs) in parsed.iter().enumerate() {
                    let Some((_, payload)) =
                        secs.iter().find(|(id, _)| *id == SEC_CODE)
                    else {
                        continue;
                    };
                    let mut p = 0;
                    let _ = read_u32(payload, &mut p);
                    // Where this unit's bodies land, so its recorded offsets
                    // can be found again.
                    let base = out.len();
                    out.extend_from_slice(&payload[p..]);
                    // And every immediate in them that names a local function,
                    // moved by this unit's offset. The bodies themselves are
                    // copied unread -- only these five bytes at a time are
                    // touched.
                    let shift = offset_of[i];
                    if shift != 0 {
                        for r in relocs_of(secs) {
                            let at = base + (r as usize - p);
                            if at + 5 > out.len() {
                                return None;
                            }
                            let mut q = at;
                            let cur = read_u32(&out, &mut q);
                            crate::wasm::patch_u32_fixed(&mut out, at, cur + shift);
                        }
                    }
                }
            }
            SEC_EXPORT => section_counted(&mut out, SEC_EXPORT, n_exports, &exports),
            SEC_ELEMENT => section_counted(&mut out, SEC_ELEMENT, n_elems, &elems),
            // Types, imports, tables, memory, globals, data, custom: one
            // unit's are every unit's.
            other => section(&mut out, other, payload),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb_round_trips() {
        for v in [0u32, 1, 127, 128, 300, 65535, u32::MAX] {
            let mut b = Vec::new();
            write_u32(&mut b, v);
            let mut p = 0;
            assert_eq!(read_u32(&b, &mut p), v);
            assert_eq!(p, b.len());
        }
    }

    #[test]
    fn one_unit_is_itself() {
        let m = b"\0asm\x01\0\0\0".to_vec();
        assert_eq!(merge(&[&m]).unwrap(), m);
    }

    #[test]
    fn rejects_a_non_module() {
        assert!(merge(&[b"not wasm", b"nor this"]).is_none());
    }
}
