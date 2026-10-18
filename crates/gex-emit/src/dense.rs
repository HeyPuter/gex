//! Dense block tables: tier-1 dispatch without a table slot per four bytes
//! of guest code (the browser; docs/merge-plan.md §9.2, "tables index
//! functions, not 4-byte slots").
//!
//! The page directory and the slot numbering stay as they are: a page's
//! directory entry names a dispatch table `k` and a base slot, and an
//! address's slot is the base plus its offset in the page. What changes is
//! what a slot is. Classically it is an entry of the funcref table `k`, so
//! every thread's table is as long as the highest slot numbered (one per
//! four bytes of every page of code ever translated, millions), and in the
//! browser every Worker has its own copy on the V8 heap. Dense, it is a
//! `u32` in the process's linear memory -- the *slot map* of table `k` --
//! holding an index into one more table, `blocks`, which has one entry per
//! block (or shared-slot trampoline) the process installed:
//!
//! ```text
//! entry = DISPATCH_DIR[page]; k = entry >> 24; slot = (entry & mask) + off
//! map   = MAPS[k - FIRST_TABLE]                  ;; 0: table k is classic
//! map ? return_call_indirect blocks[map[slot]] : return_call_indirect table_k[slot]
//! ```
//!
//! A slot nobody installed reads 0, and `blocks[0]` is the miss stub, as
//! table 0's low slots are.
//!
//! * The slot maps are process-wide (one memory per process), so a block is
//!   numbered once per process, not per thread; each Worker's `blocks` table
//!   is as long as the number of blocks, about a tenth of the slots.
//! * A module's dense blocks are one active element segment on `blocks` at
//!   a constant offset, its *dense base*, and its custom section
//!   [`SECTION`] lists the slot of each element in order. The host assigns
//!   the base when the module becomes the process's (and writes it into the
//!   module with [`set_base`]) and writes the slot maps when it publishes it.
//! * Only tables `FIRST_TABLE..` can be dense: tables below are a closure's
//!   (numbered by the build), and stay classic.
//!
//! Off unless the host turns it on ([`set`]); natively nothing changes. The
//! dispatch code is emitted dense-capable whenever it is on, and falls back
//! to the classic table when a map word is zero, so a host can leave any
//! table classic.

use std::sync::atomic::{AtomicBool, Ordering};

/// The first dispatch table that may be dense.
pub const FIRST_TABLE: usize = 4;

/// How many tables may be dense.
pub const TABLES: usize = gex_abi::DISPATCH_TABLES - FIRST_TABLE;

/// `[u32; TABLES]` in the reserved low region: the address of each dense
/// table's slot map, or 0 for a classic table. Between `BODY_LIMITS`
/// (RESERVED_END - 320, 32 bytes) and `NEST_EXITS` (RESERVED_END - 256).
pub const MAPS: u64 = gex_abi::RESERVED_END - 288;

const _: () = {
    assert!(MAPS >= gex_abi::BODY_LIMITS + 4 * gex_abi::DISPATCH_TABLES as u64);
    assert!(MAPS + 4 * TABLES as u64 <= gex_abi::NEST_EXITS);
};

/// The slots one dense table's map covers (`u32` each).
pub const MAP_SLOTS: u64 = gex_abi::DISPATCH_TABLE_MAX_SLOTS;

/// The `blocks` table's name (an export of the image module, an import of
/// every other).
pub const TABLE_NAME: &str = "blocks";

/// Its table index in every tier-1 module: after the dispatch tables.
pub const TABLE_INDEX: u32 = gex_abi::DISPATCH_TABLES as u32;

/// The custom section a module lists its dense elements in:
/// `u32 base, u32 count, u32 entry * count` (little-endian), each entry a
/// directory-style `table << 24 | slot`. `base` is the module's dense base,
/// the same number as its element segment's offset, both written by
/// [`set_base`].
pub const SECTION: &str = "gex.dense";

/// The imported i32 global a runtime module's dense element segment is
/// placed at, when it has one (rather than a constant offset): its value is
/// the section's `base`, which the host passes at instantiation.
pub const GLOBAL: &str = "dense_base";

static ON: AtomicBool = AtomicBool::new(false);

/// Emit dense dispatch and dense element segments from now on.
pub fn set(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Whether directory table `k` may be dense.
pub fn table(k: usize) -> bool {
    on() && k >= FIRST_TABLE
}

/// A module's dense elements, as [`SECTION`] holds them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Section {
    pub base: u32,
    /// Directory-style `table << 24 | slot`, one per element, in order.
    pub entries: Vec<u32>,
}

impl Section {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(8 + 4 * self.entries.len());
        v.extend_from_slice(&self.base.to_le_bytes());
        v.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for e in &self.entries {
            v.extend_from_slice(&e.to_le_bytes());
        }
        v
    }

    pub fn decode(b: &[u8]) -> Option<Section> {
        let word = |i: usize| b.get(4 * i..4 * i + 4).map(|w| u32::from_le_bytes(w.try_into().unwrap()));
        let base = word(0)?;
        let n = word(1)? as usize;
        if b.len() != 8 + 4 * n {
            return None;
        }
        Some(Section { base, entries: (0..n).map(|i| word(2 + i).unwrap()).collect() })
    }
}

/// Where a module's dense parts are: the [`SECTION`] payload (its `base`
/// word first) and the element segment's offset immediate (a five-byte
/// `i32.const` LEB), as byte ranges of the module.
#[derive(Clone, Copy, Debug)]
pub struct Located {
    pub section: (usize, usize),
    /// The segment's constant offset, or `None` where it is
    /// [`GLOBAL`]'s value.
    pub offset_imm: Option<usize>,
}

fn leb(b: &[u8], p: &mut usize) -> Option<u32> {
    let mut v = 0u32;
    let mut shift = 0;
    loop {
        let byte = *b.get(*p)?;
        *p += 1;
        v |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 28 {
            return None;
        }
    }
}

/// Find a module's dense parts, or `None` when it has none.
pub fn locate(wasm: &[u8]) -> Option<Located> {
    locate_parts(wasm).and_then(|(s, o)| Some(Located { section: s?, offset_imm: o? }))
}

/// Whether a module names [`SECTION`] at all (a module that does, but that
/// [`locate`] cannot read, must not be instantiated at base 0).
pub fn has_section(wasm: &[u8]) -> bool {
    locate_parts(wasm).is_none_or(|(s, _)| s.is_some())
}

/// The dense section's payload range, and the dense segment's constant
/// offset (`Some(None)` for one at [`GLOBAL`]), each `None` when absent.
#[allow(clippy::type_complexity)]
fn locate_parts(wasm: &[u8]) -> Option<(Option<(usize, usize)>, Option<Option<usize>>)> {
    if wasm.len() < 8 || &wasm[..4] != b"\0asm" {
        return None;
    }
    let mut p = 8;
    let mut section = None;
    let mut offset_imm = None;
    while p < wasm.len() {
        let id = wasm[p];
        p += 1;
        let len = leb(wasm, &mut p)? as usize;
        let end = p.checked_add(len)?;
        if end > wasm.len() {
            return None;
        }
        if id == 0 {
            let mut q = p;
            let n = leb(wasm, &mut q)? as usize;
            if wasm.get(q..q + n) == Some(SECTION.as_bytes()) {
                section = Some((q + n, end));
            }
        } else if id == 9 {
            let mut q = p;
            let count = leb(wasm, &mut q)?;
            for _ in 0..count {
                let flags = leb(wasm, &mut q)?;
                let mut table = 0;
                if flags == 2 || flags == 6 {
                    table = leb(wasm, &mut q)?;
                }
                let mut imm = None;
                if flags & 1 == 0 {
                    // The offset expression: `i32.const n` or `global.get g`,
                    // then `end` (an immediate may hold the byte 0x0b).
                    let op = *wasm.get(q)?;
                    if op != 0x41 && op != 0x23 {
                        return None;
                    }
                    if op == 0x41 {
                        imm = Some(q + 1);
                    }
                    q += 1;
                    leb(wasm, &mut q)?;
                    if *wasm.get(q)? != 0x0b {
                        return None;
                    }
                    q += 1;
                }
                if flags & 3 != 0 {
                    q += 1; // elemkind (flags 4..7 would be a reftype; never emitted)
                }
                let n = leb(wasm, &mut q)?;
                if flags & 4 != 0 {
                    return None; // expression elements: never emitted
                }
                for _ in 0..n {
                    leb(wasm, &mut q)?;
                }
                if flags == 2 && table == TABLE_INDEX {
                    offset_imm = Some(imm);
                }
            }
        }
        p = end;
    }
    Some((section, offset_imm))
}

/// Write `base` into a module located by [`locate`]: the section's base word
/// and the segment's offset.
pub fn set_base(wasm: &mut [u8], at: &Located, base: u32) {
    wasm[at.section.0..at.section.0 + 4].copy_from_slice(&base.to_le_bytes());
    let Some(imm) = at.offset_imm else {
        return;
    };
    let imm = &mut wasm[imm..imm + 5];
    let mut v = base as i32;
    for (i, b) in imm.iter_mut().enumerate() {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        *b = if i < 4 { byte | 0x80 } else { byte & 0x7f };
    }
}

/// The section of a located module.
pub fn section(wasm: &[u8], at: &Located) -> Option<Section> {
    Section::decode(&wasm[at.section.0..at.section.1])
}

/// Emit the dispatch to table `k`'s slot in local `index` with `ctx` in local
/// `ctx`: through `blocks` when `k`'s map word is set, else the classic table.
/// `scratch` is an i32 local this may overwrite. For [`table`]`(k)` only.
pub fn emit_arm(f: &mut crate::wasm::FuncBody, k: usize, ctx: u32, index: u32, scratch: u32, block_ty: u32) {
    emit_arm_at(f, k, ctx, index, scratch, block_ty, k as u32, TABLE_INDEX);
}

/// [`emit_arm`] in a module whose dispatch table `k` is table `classic` and
/// whose `blocks` is table `blocks`.
#[allow(clippy::too_many_arguments)]
pub fn emit_arm_at(f: &mut crate::wasm::FuncBody, k: usize, ctx: u32, index: u32, scratch: u32, block_ty: u32, classic: u32, blocks: u32) {
    debug_assert!(k >= FIRST_TABLE);
    f.addr_const(0);
    f.load(0x28, 2, (MAPS + 4 * (k - FIRST_TABLE) as u64) as u32); // i32.load
    f.local_tee(scratch);
    f.if_void();
    f.local_get(ctx);
    f.local_get(scratch);
    f.local_get(index);
    f.i32_const(2);
    f.byte(0x74); // i32.shl
    f.byte(0x6a); // i32.add
    f.addr_i32(); // the map is in the runtime's low memory
    f.load(0x28, 2, 0); // i32.load: the block's index in `blocks`
    f.return_call_indirect(block_ty, blocks);
    f.end();
    f.local_get(ctx);
    f.local_get(index);
    f.return_call_indirect(block_ty, classic);
}

/// `(ctx) -> ()`: tail-call the block for `ctx::LAST_TARGET`, as tier-1's
/// dispatch does, dense-capable: a code-section entry (size, locals, code)
/// for a module whose dispatch tables are tables `first..` and whose
/// `blocks` is table `blocks` (the function table's runtime module, whose
/// bridge carries on in tier-1 through such a function).
pub fn dispatch_body(block_ty: u32, first: u32, blocks: u32) -> Vec<u8> {
    use crate::wasm::{FuncBody, ValType};
    use gex_abi::{ctx, DISPATCH_DIR, DISPATCH_MAX_PAGES, DISPATCH_PAGE};
    let mut f = FuncBody::new(1);
    let target = f.add_local(ValType::I64);
    let page = f.add_local(ValType::I64);
    let entry = f.add_local(ValType::I32);
    let index = f.add_local(ValType::I32);
    f.local_get(0);
    f.addr_i32();
    f.load(0x29, 3, ctx::LAST_TARGET as u32);
    f.local_set(target);
    f.local_get(target);
    f.i64_const(DISPATCH_PAGE.trailing_zeros() as i64);
    f.byte(0x88); // i64.shr_u
    f.local_set(page);
    f.local_get(page);
    f.byte(0xa7); // i32.wrap_i64
    f.i32_const(4);
    f.byte(0x6c); // i32.mul
    f.addr_i32();
    f.load(0x28, 2, DISPATCH_DIR as u32);
    f.i32_const(0);
    f.local_get(page);
    f.i64_const(DISPATCH_MAX_PAGES as i64);
    f.byte(0x54); // i64.lt_u
    f.select();
    f.local_tee(entry);
    f.i32_const(gex_abi::DISPATCH_SLOT_MASK as i32);
    f.byte(0x71); // i32.and
    f.local_get(target);
    f.i64_const((DISPATCH_PAGE - 1) as i64);
    f.byte(0x83); // i64.and
    f.i64_const(gex_abi::DISPATCH_SHIFT as i64);
    f.byte(0x88); // i64.shr_u
    f.byte(0xa7); // i32.wrap_i64
    f.byte(0x6a); // i32.add
    f.local_set(index);
    f.local_get(entry);
    f.i32_const(gex_abi::DISPATCH_SLOT_BITS as i32);
    f.byte(0x76); // i32.shr_u
    f.local_set(entry);
    let n = gex_abi::DISPATCH_TABLES as u32;
    for _ in 0..n {
        f.block_void();
    }
    f.local_get(entry);
    let targets: Vec<u32> = (0..n).map(|k| n - 1 - k).collect();
    f.br_table(&targets, n - 1);
    for k in (0..n).rev() {
        f.end();
        if k as usize >= FIRST_TABLE {
            emit_arm_at(&mut f, k as usize, 0, index, entry, block_ty, first + k, blocks);
            continue;
        }
        f.local_get(0);
        f.local_get(index);
        f.return_call_indirect(block_ty, first + k);
    }
    f.finish()
}

/// The `blocks` indices for bridging `entries` (dispatch slots, `table << 24
/// | slot`), each slot keeping the index it was first given (`given`): a slot
/// bridged again gets the same one, so re-bridging numbers nothing.
/// `alloc(n)` hands out `n` fresh consecutive indices and is called at most
/// once, only for slots never bridged before.
pub fn bridge_indices(given: &mut std::collections::HashMap<u32, u32>, entries: &[u32], alloc: impl FnOnce(u32) -> u32) -> Vec<u32> {
    let fresh = entries.iter().filter(|e| !given.contains_key(e)).collect::<std::collections::BTreeSet<_>>().len() as u32;
    let mut next = if fresh == 0 { 0 } else { alloc(fresh) };
    entries
        .iter()
        .map(|&e| {
            *given.entry(e).or_insert_with(|| {
                next += 1;
                next - 1
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_finds_the_dense_segment_past_an_offset_of_0x0b() {
        use crate::wasm::{ElementSegment, FuncType, Module};
        let mut m = Module::new();
        let t = m.func_type(FuncType::new(vec![], vec![]));
        for i in 0..=TABLE_INDEX {
            m.import_table("gex", &format!("t{i}"), 0);
        }
        let f = m.declare_func(t);
        m.define_func(f, vec![2, 0, 0x0b]);
        let seg = |table, offset, padded| ElementSegment { table, offset, page_base: 0, funcs: vec![f], passive: false, guest_page: 0, padded, offset_global: None };
        m.element(seg(0, 11, false));
        m.element(seg(4, 0x0b0b, false));
        m.element(seg(TABLE_INDEX, 0, true));
        let mut wasm = m.encode_with_custom(&[(SECTION, Section { base: 0, entries: vec![(4 << 24) | 7] }.encode())]);
        let at = locate(&wasm).expect("located");
        set_base(&mut wasm, &at, 0x1234_567);
        let again = locate(&wasm).expect("located again");
        assert_eq!(section(&wasm, &again).unwrap().base, 0x1234_567);
        let mut q = again.offset_imm.expect("a constant offset");
        let mut v = 0u32;
        let mut shift = 0;
        loop {
            let b = wasm[q];
            q += 1;
            v |= ((b & 0x7f) as u32) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        assert_eq!(v, 0x1234_567);
        assert_eq!(wasm[q], 0x0b);
    }

    #[test]
    fn section_roundtrips() {
        let s = Section { base: 77, entries: vec![(4 << 24) | 5, (4 << 24) | 1029] };
        assert_eq!(Section::decode(&s.encode()), Some(s));
    }

    #[test]
    fn bridging_a_slot_again_keeps_its_index() {
        use std::collections::HashMap;
        let mut given = HashMap::new();
        let mut next = 100u32;
        let mut calls = 0;
        let mut alloc = |n: u32| {
            calls += 1;
            let b = next;
            next += n;
            b
        };
        let first = bridge_indices(&mut given, &[7, 9], &mut alloc);
        assert_eq!(first, vec![100, 101]);
        // The same slots again (a re-bridge): the same indices, nothing numbered.
        for _ in 0..1000 {
            assert_eq!(bridge_indices(&mut given, &[9, 7], &mut alloc), vec![101, 100]);
        }
        // One new slot beside an old one: only the new one is numbered.
        assert_eq!(bridge_indices(&mut given, &[7, 12], &mut alloc), vec![100, 102]);
        drop(alloc);
        assert_eq!(calls, 2);
        assert_eq!(next, 103, "the dense size grew past the slots bridged");
    }
}
