//! The hosted layout (`gex_abi::set_hosted_layout`): what the translators emit with the runtime's structures in a
//! block a host placed, instead of the low region gex owns otherwise.
//!
//! Its own test binary, because the layout is process-wide and fixed by the first translation.

#![cfg(not(feature = "x86"))]

use std::collections::HashMap;

/// Somewhere no fixed-layout structure is: the block, 64 KiB-aligned.
const BASE: u64 = 0x0abc_0000;
/// The host's data floor (emscripten's `GLOBAL_BASE`).
const FLOOR: u64 = 0x400;
/// Above the fixed layout's directory reach (2 GiB), which hosted code may be.
const TEXT: u64 = 0x9000_0000;

/// `1: ldxr x2,[x0]; add x2,x2,#1; stxr w3,x2,[x0]; cbnz w3,1b; br x1`: the exclusive monitor (its scratch word is
/// one of the fixed words) and an indirect branch (the directory).
const CODE: [u32; 5] = [0xc85f7c02, 0x91000442, 0xc8037c02, 0x35ffffa3, 0xd61f0020];

/// Every constant and memory offset in `wasm`'s code.
fn constants(wasm: &[u8]) -> (Vec<u64>, usize) {
    let mut out = Vec::new();
    let mut table_sizes = 0;
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        let wasmparser::Payload::CodeSectionEntry(body) = payload.expect("parse") else { continue };
        let mut ops = body.get_operators_reader().expect("operators");
        while !ops.eof() {
            let op = ops.read().expect("operator");
            match op {
                wasmparser::Operator::I32Const { value } => out.push(value as u32 as u64),
                // Dispatch's bound on the page index is the hosted directory's length, 1 << 20, which happens to be
                // the fixed layout's directory address: a count, not an address.
                wasmparser::Operator::I64Const { value } if value as u64 == gex_abi::hosted::MAX_PAGES => {}
                wasmparser::Operator::I64Const { value } => out.push(value as u64),
                wasmparser::Operator::TableSize { .. } => table_sizes += 1,
                _ => {
                    // Every load, store and atomic: their memarg's offset, read from the operator's own rendering
                    // rather than a match on each of the hundred-odd memory operators.
                    let s = format!("{op:?}");
                    if let Some(i) = s.find("offset: ") {
                        let n: String = s[i + 8..].chars().take_while(|c| c.is_ascii_digit()).collect();
                        out.push(n.parse().expect("offset"));
                    }
                }
            }
        }
    }
    (out, table_sizes)
}

#[test]
fn hosted_layout() {
    gex_abi::set_hosted_layout(BASE, FLOOR);
    assert_eq!(gex_abi::dispatch_dir(), BASE + gex_abi::hosted::DIR);
    assert_eq!(gex_abi::dispatch_max_pages(), 1 << 20);
    assert_eq!(gex_abi::exclusive_scratch(), BASE + gex_abi::hosted::EXCLUSIVE_SCRATCH);
    assert_eq!(gex_abi::guest_floor(), FLOOR);
    assert_eq!(gex_abi::layout_key(), BASE);

    let mut mem = vec![0u8; 0x1_0000];
    for (i, w) in CODE.iter().enumerate() {
        mem[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    // Through a window starting at `TEXT`, as an embedding with code above 2 GiB has to hand it over: a Rust
    // slice of the real memory from zero can't be longer than isize::MAX.
    let end = TEXT + CODE.len() as u64 * 4;
    let seed = gex_xlate::seed_module(true);
    let (unit, stats) = gex_xlate::translate_runtime_in(
        gex_xlate::GuestWindow { bytes: &mem, base: TEXT },
        TEXT,
        &|a| a >= TEXT && a + gex_abi::INST_ALIGN <= end,
        &|_| false,
        gex_xlate::PageTable::new(),
        Default::default(),
        4096,
        true,
        &HashMap::new(),
    );
    assert!(stats.blocks >= 2, "{} blocks", stats.blocks);
    assert_eq!(unit.dir_entries.len(), 1);
    assert_eq!(unit.dir_entries[0].0, TEXT / gex_abi::DISPATCH_PAGE);

    for (what, m) in [("seed", &seed), ("unit", &unit)] {
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&m.wasm)
            .unwrap_or_else(|e| panic!("{what}: {e}"));
    }

    // Nothing of the fixed layout's runtime region (its words, probe cache, patched bits, directory) is named.
    let fixed = gex_abi::FCACHE..gex_abi::DISPATCH_DIR_END;
    let (seed_k, seed_guards) = constants(&seed.wasm);
    let (unit_k, _) = constants(&unit.wasm);
    for (what, k) in [("seed", &seed_k), ("unit", &unit_k)] {
        if let Some(c) = k.iter().find(|c| fixed.contains(c)) {
            panic!("{what} names {c:#x}, in the fixed layout's runtime region");
        }
    }
    // And the block is: the directory (dispatch's lookup) and the exclusive monitor's scratch word.
    let dir = gex_abi::dispatch_dir();
    assert!(seed_k.contains(&dir), "the seed's dispatch doesn't read the hosted directory");
    let scratch = gex_abi::exclusive_scratch();
    assert!(unit_k.contains(&scratch), "the unit doesn't name the hosted exclusive scratch word");
    // The seed's `dispatch` checks each of the eight tables' size before calling into it.
    assert_eq!(seed_guards, gex_abi::DISPATCH_TABLES, "table guards in the seed's dispatch");

    // The layout is fixed now.
    assert!(std::panic::catch_unwind(|| gex_abi::set_hosted_layout(BASE + 0x1_0000, FLOOR)).is_err());
}
