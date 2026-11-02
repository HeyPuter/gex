//! Dense block tables: tier-1 dispatch without a table slot per four bytes
//! of guest code (the browser, "tables index
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
//! holding a word `entry << 8 | arm` for one more table, `blocks`. A module
//! with `n` dense blocks (or shared-slot trampolines) puts `ceil(n / 256)`
//! *entry functions* in `blocks`, each `(ctx, arm) -> ()` and a `br_table`
//! on `arm` to a `return_call` of the block ([`entry_body`]), so a Worker's
//! `blocks` has one entry per 256 blocks the process installed:
//!
//! ```text
//! entry = DISPATCH_DIR[page]; k = entry >> 24; slot = (entry & mask) + off
//! map   = MAPS[k - FIRST_TABLE]                  ;; 0: table k is classic
//! map ? (w = map[slot]; return_call_indirect blocks[w >> 8](ctx, w & 255))
//!     : return_call_indirect table_k[slot]
//! ```
//!
//! A slot nobody installed reads 0, and `blocks[0]` is the miss stub (as an
//! entry: the host wraps it), as table 0's low slots are; so is an arm past
//! its entry's blocks (a word left pointing past a module's last block),
//! which the `br_table` sends there.
//!
//! * The slot maps are process-wide (one memory per process), so a block is
//!   numbered once per process, not per thread; each Worker's `blocks` table
//!   is as long as the number of entries.
//! * A module's entry functions are one active element segment on `blocks`
//!   at a constant offset, its *dense base*, and its custom section
//!   [`SECTION`] lists the slot of each block in order. The host assigns
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

/// The blocks one entry of `blocks` dispatches among.
pub const ARMS: u32 = 256;

/// How many `blocks` entries `n` dense blocks take.
pub fn span(n: u32) -> u32 {
    n.div_ceil(ARMS)
}

/// The `blocks` index a slot map word names (0 for none).
pub fn index_of(word: u32) -> u32 {
    word >> 8
}

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
    /// Directory-style `table << 24 | slot`, one per block, in order: block
    /// `j` is arm `j % 256` of index `base + j / 256`.
    pub entries: Vec<u32>,
}

impl Section {
    /// How many `blocks` indices from `base` the module fills.
    pub fn span(&self) -> u32 {
        span(self.entries.len() as u32)
    }

    /// The slot map word for block `j`.
    pub fn word(&self, j: usize) -> u32 {
        ((self.base + j as u32 / ARMS) << 8) | (j as u32 % ARMS)
    }

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
/// `ctx`: through `blocks` when `k`'s map word is set (the word's entry,
/// called with its arm), else the classic table. `scratch` is an i32 local
/// this may overwrite; `entry_ty` the module's `(ctx, arm) -> ()`. For
/// [`table`]`(k)` only.
pub fn emit_arm(f: &mut crate::wasm::FuncBody, k: usize, ctx: u32, index: u32, scratch: u32, block_ty: u32, entry_ty: u32) {
    emit_arm_at(f, k, ctx, index, scratch, block_ty, entry_ty, k as u32, TABLE_INDEX);
}

/// [`emit_arm`] in a module whose dispatch table `k` is table `classic` and
/// whose `blocks` is table `blocks`.
#[allow(clippy::too_many_arguments)]
pub fn emit_arm_at(f: &mut crate::wasm::FuncBody, k: usize, ctx: u32, index: u32, scratch: u32, block_ty: u32, entry_ty: u32, classic: u32, blocks: u32) {
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
    f.load(0x28, 2, 0); // i32.load: the slot's map word
    f.local_tee(scratch);
    f.i32_const(ARMS as i32 - 1);
    f.byte(0x71); // i32.and: the arm
    f.local_get(scratch);
    f.i32_const(8);
    f.byte(0x76); // i32.shr_u: the entry
    f.return_call_indirect(entry_ty, blocks);
    f.end();
    f.local_get(ctx);
    f.local_get(index);
    f.return_call_indirect(block_ty, classic);
}

/// An entry function's body (code-section entry: size, locals, code):
/// `(ctx, arm) -> ()`, a `br_table` on `arm`
/// to a `return_call` of `funcs[arm]` (at most [`ARMS`]), and any other arm
/// to `blocks[0]`, the miss stub, through table `blocks`.
pub fn entry_body(entry_ty: u32, blocks: u32, funcs: &[u32]) -> Vec<u8> {
    use crate::wasm::FuncBody;
    debug_assert!(!funcs.is_empty() && funcs.len() <= ARMS as usize);
    let n = funcs.len() as u32;
    let mut f = FuncBody::new(2);
    // The default's block outermost, then one per arm, arm 0 innermost:
    // `br j` leaves arm j's block and runs what follows its `end`.
    for _ in 0..=n {
        f.block_void();
    }
    f.local_get(1);
    let targets: Vec<u32> = (0..n).collect();
    f.br_table(&targets, n);
    for &func in funcs {
        f.end();
        f.local_get(0);
        f.return_call(func);
    }
    f.end();
    f.local_get(0);
    f.i32_const(0);
    f.i32_const(0);
    f.return_call_indirect(entry_ty, blocks);
    f.finish()
}

/// `(ctx) -> ()`: tail-call the block for `ctx::LAST_TARGET`, as tier-1's
/// dispatch does, dense-capable: a code-section entry (size, locals, code)
/// for a module whose dispatch tables are tables `first..` and whose
/// `blocks` is table `blocks` (the function table's runtime module, whose
/// bridge carries on in tier-1 through such a function).
pub fn dispatch_body(block_ty: u32, entry_ty: u32, first: u32, blocks: u32) -> Vec<u8> {
    dispatch_body_in(block_ty, entry_ty, first, blocks, None)
}

/// [`dispatch_body`] reading the directory at global `alt_dir` (the
/// process's other one, `gex_abi::imports::ALT_DIR`) when given.
pub fn dispatch_body_in(block_ty: u32, entry_ty: u32, first: u32, blocks: u32, alt_dir: Option<u32>) -> Vec<u8> {
    use crate::wasm::{FuncBody, ValType};
    use gex_abi::{ctx, DISPATCH_PAGE};
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
    match alt_dir {
        Some(g) => {
            f.global_get(g);
            f.addr_add();
            f.load64(0x28, 2, 0);
        }
        None => f.load64(0x28, 2, gex_abi::dispatch_dir()),
    }
    f.i32_const(0);
    f.local_get(page);
    f.i64_const(gex_abi::dispatch_max_pages() as i64);
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
            emit_arm_at(&mut f, k as usize, 0, index, entry, block_ty, entry_ty, first + k, blocks);
            continue;
        }
        f.local_get(0);
        f.local_get(index);
        f.return_call_indirect(block_ty, first + k);
    }
    f.finish()
}

/// The slot map words (`index << 8 | arm`) for bridging `entries`
/// (dispatch slots, `table << 24 | slot`), each slot keeping the word it was
/// first given (`given`): a slot bridged again gets the same one, so
/// re-bridging numbers nothing.
///
/// Every bridge is the same function, which finds its target in the
/// context (`ctx::LAST_TARGET`), not in its index, so bridges share
/// `blocks` entries: [`ARMS`] slots to an entry, one arm each (`group`, the
/// entry being filled and its next arm, kept across calls). Given an entry
/// per slot, a process with units for a desktop's libraries numbered about
/// 1.8 million entries, and each of its Workers' `blocks` held them all, in
/// the renderer's range every isolate shares. An arm of its own still sets
/// each slot apart where dispatch is watched (a call-out's return), so a
/// watch on one bridged return is not hit by every other bridge.
/// `alloc(1)` hands out a fresh index, called once per [`ARMS`] new slots.
pub fn bridge_words(
    given: &mut std::collections::HashMap<u32, u32>,
    group: &mut (u32, u32),
    entries: &[u32],
    mut alloc: impl FnMut(u32) -> u32,
) -> Vec<u32> {
    entries
        .iter()
        .map(|&e| {
            *given.entry(e).or_insert_with(|| {
                if group.0 == 0 || group.1 >= ARMS {
                    *group = (alloc(1), 0);
                }
                let w = group.0 << 8 | group.1;
                group.1 += 1;
                w
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
        let s = Section { base: 9, entries: vec![(4 << 24) | 5; 300] };
        assert_eq!(Section::decode(&s.encode()), Some(s.clone()));
        assert_eq!(s.span(), 2);
        assert_eq!(s.word(0), 9 << 8);
        assert_eq!(s.word(255), (9 << 8) | 255);
        assert_eq!(s.word(256), 10 << 8);
        assert_eq!(s.word(299), (10 << 8) | 43);
    }

    #[test]
    fn bridging_a_slot_again_keeps_its_word() {
        use std::collections::HashMap;
        let mut given = HashMap::new();
        let mut group = (0, 0);
        let mut next = 100u32;
        let mut calls = 0;
        let mut alloc = |n: u32| {
            calls += 1;
            let b = next;
            next += n;
            b
        };
        let first = bridge_words(&mut given, &mut group, &[7, 9], &mut alloc);
        assert_eq!(first, vec![100 << 8, 100 << 8 | 1]);
        // The same slots again (a re-bridge): the same words, nothing numbered.
        for _ in 0..1000 {
            assert_eq!(bridge_words(&mut given, &mut group, &[9, 7], &mut alloc), vec![100 << 8 | 1, 100 << 8]);
        }
        // One new slot beside an old one: only the new one takes an arm.
        assert_eq!(bridge_words(&mut given, &mut group, &[7, 12], &mut alloc), vec![100 << 8, 100 << 8 | 2]);
        drop(alloc);
        assert_eq!(calls, 1);
        assert_eq!(next, 101, "one entry for the slots bridged");
    }

    #[test]
    fn bridges_share_an_entry_until_its_arms_run_out() {
        use std::collections::HashMap;
        let mut given = HashMap::new();
        let mut group = (0, 0);
        let mut next = 5u32;
        let slots: Vec<u32> = (0..ARMS * 2 + 3).collect();
        let words = bridge_words(&mut given, &mut group, &slots, |n| {
            let b = next;
            next += n;
            b
        });
        // Three entries for 2 * ARMS + 3 slots, each slot an arm of its own.
        assert_eq!(next, 8);
        let distinct: std::collections::BTreeSet<u32> = words.iter().copied().collect();
        assert_eq!(distinct.len(), slots.len());
        assert_eq!(words[0], 5 << 8);
        assert_eq!(words[ARMS as usize - 1], 5 << 8 | (ARMS - 1));
        assert_eq!(words[ARMS as usize], 6 << 8);
        assert_eq!(*words.last().unwrap(), 7 << 8 | 2);
    }
}
