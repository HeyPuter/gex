//! Concatenating translation units into one module.
//!
//! A process that can identify its own pages finds, in the shared store, every
//! unit anybody has translated of the libraries it is running. Installing them
//! one at a time means a `WebAssembly.compile` and an `instantiate` each:
//! measured at about seventeen hundred per process and three and a half
//! seconds, against three hundred milliseconds of actual translation. Finding
//! the code had become dearer than lifting it.
//!
//! These are not arbitrary modules, so this is concatenation rather than
//! linking. Every unit is [`crate::module::ModuleBuilder`]'s output with the
//! same imports in the same order and one function type for every block. A
//! body *may* name another block directly -- that tail call is worth keeping,
//! and forbidding it was measured at twice the translation work -- so each
//! unit carries a `gex.reloc` custom section giving the offset of every such
//! immediate, written at a fixed five bytes so patching cannot change a
//! body's length. So:
//!
//!   * types, imports, tables, memory and data come from the first unit,
//!     because they are identical in all of them;
//!   * function and code sections concatenate verbatim;
//!   * exports and element segments carry local function indices, which shift
//!     as bodies are appended, so those are renumbered.
//!
//! Bodies are copied without inspection. That is the whole point of the
//! position-independent flag: without it a body can `return_call` a sibling
//! block by index, and a merge would have to rewrite instruction streams.

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
/// The first unit decides everything but the functions: its types, imports,
/// tables, memory and data are the merged module's. That is safe because they
/// are emitted by one builder from one ABI, and it is checked to the extent
/// that a mismatch shows up as a different import count.
pub fn merge(units: &[&[u8]]) -> Option<Vec<u8>> {
    if units.is_empty() {
        return None;
    }
    if units.len() == 1 {
        return Some(units[0].to_vec());
    }
    let parsed: Vec<Vec<(u8, &[u8])>> = units.iter().map(|u| sections(u)).collect::<Option<_>>()?;
    let n_imports = import_funcs(&parsed[0]);
    for p in &parsed[1..] {
        if import_funcs(p) != n_imports {
            return None;
        }
    }

    // Functions and code, concatenated; and the offset each unit's local
    // indices move by, which is every earlier unit's local function count.
    let mut funcs = Vec::new();
    let mut n_funcs = 0u32;
    let mut code = Vec::new();
    let mut n_bodies = 0u32;
    let mut offset_of = Vec::with_capacity(parsed.len());
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
            // Where this unit's bodies land in the merged payload, so its
            // recorded offsets can be found again.
            let base = code.len();
            code.extend_from_slice(&payload[p..]);
            // And every immediate in them that names a local function, moved
            // by this unit's offset. The bodies themselves are copied
            // unread -- only these five bytes at a time are touched.
            let shift = *offset_of.last().unwrap();
            if shift != 0 {
                for r in relocs_of(secs) {
                    let at = base + (r as usize - p);
                    if at + 5 > code.len() {
                        return None;
                    }
                    let mut v = 0u32;
                    let mut q = at;
                    let cur = read_u32(&code, &mut q);
                    v += cur;
                    crate::wasm::patch_u32_fixed(&mut code, at, v + shift);
                }
            }
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
    let mut exports = Vec::new();
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
    let mut elems = Vec::new();
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
            // The offset expression: `i32.const N end`.
            p += 1;
            read_u32(payload, &mut p);
            p += 1;
            if flag == 0x02 {
                p += 1; // elemkind
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
            SEC_FUNCTION => {
                let mut s = Vec::with_capacity(funcs.len() + 5);
                write_u32(&mut s, n_funcs);
                s.extend_from_slice(&funcs);
                section(&mut out, SEC_FUNCTION, &s);
            }
            SEC_CODE => {
                let mut s = Vec::with_capacity(code.len() + 5);
                write_u32(&mut s, n_bodies);
                s.extend_from_slice(&code);
                section(&mut out, SEC_CODE, &s);
            }
            SEC_EXPORT => {
                let mut s = Vec::with_capacity(exports.len() + 5);
                write_u32(&mut s, n_exports);
                s.extend_from_slice(&exports);
                section(&mut out, SEC_EXPORT, &s);
            }
            SEC_ELEMENT => {
                let mut s = Vec::with_capacity(elems.len() + 5);
                write_u32(&mut s, n_elems);
                s.extend_from_slice(&elems);
                section(&mut out, SEC_ELEMENT, &s);
            }
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
