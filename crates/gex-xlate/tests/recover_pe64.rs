//! Function recovery on a PE32+ image: S5's `uses.exe` fixture (MinGW
//! x86-64, imports from two DLLs and a resource), seeded from its entry,
//! relocations, import thunks and `.pdata`. A test binary of its own: the
//! decoder's mode is per process, and this one wants long mode.

#![cfg(feature = "x86")]

use gex_xlate::func::{pe_input, recover_pe, RecoverOptions};

#[test]
fn a_pe32_plus_image_recovers_every_pdata_function() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../gex-ld/tests/pe/bin/x64/uses.exe");
    let pe = gex_ld::pe::parse(&std::fs::read(path).unwrap()).unwrap();
    assert!(pe.is64 && !pe.pdata.is_empty());
    let (img, extra) = pe_input(&pe, pe.preferred_base).unwrap();
    assert!(!img.class32);
    let prog = recover_pe(&img, &extra, &RecoverOptions::default());
    assert!(!gex_xlate::arch::compat());
    // Every RUNTIME_FUNCTION begins a recovered function.
    assert_eq!(prog.stats.fdes_found, prog.stats.fdes, "{:?}", prog.stats);
    assert!(prog.stats.fdes >= 40);
    assert!(prog.functions.contains_key(&img.entry));
    let sums = gex_xlate::summary::analyze(&prog);
    assert_eq!(sums.stats.unconverged, 0);
    let rep = gex_xlate::validate::check_program(&prog.functions, &sums, 2, 200);
    assert!(rep.failures.is_empty(), "{:#?}", rep.failures);
}

/// `tests/pe/vtail64.exe` (see `tests/pe/build.sh`): a PE32+ vtable in
/// `.rdata` names `get_b`, which `pick` also reaches by a direct tail jump
/// and so covers. `get_b` stays an interior entry of `pick` (its jump stays
/// a jump inside `pick`; calls through the table reach `get_b` as a region
/// entry), and `get_c`, which nothing covers, is a function of its own.
#[test]
fn a_pe32_plus_relocated_code_pointer_covered_by_a_tail_jump_is_an_interior_entry() {
    let prog = vtail64(&RecoverOptions::default());
    check_vtail64(&prog);
}

/// The same image with a round limit too low for anything to settle: the
/// weak starts are still placed in the last round of the pass, and what
/// they promote is still discovered.
#[test]
fn weak_starts_are_placed_when_the_round_limit_is_reached() {
    let prog = vtail64(&RecoverOptions { max_rounds: 1, summary_passes: 0, ..RecoverOptions::default() });
    check_vtail64(&prog);
}

fn vtail64(opts: &RecoverOptions) -> gex_xlate::func::Program {
    let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/pe/vtail64.exe")).unwrap();
    let pe = gex_ld::pe::parse(&bytes).unwrap();
    assert!(pe.is64);
    let (img, extra) = pe_input(&pe, pe.preferred_base).unwrap();
    recover_pe(&img, &extra, opts)
}

fn check_vtail64(prog: &gex_xlate::func::Program) {
    let found: Vec<u64> = prog.functions.keys().copied().collect();
    let (pick, get_b, get_c) = (0x140001000, 0x140001010, 0x140001020);
    assert!(found.contains(&pick), "pick not a function: {found:x?}");
    assert!(!found.contains(&get_b), "get_b split off pick: {found:x?}");
    assert!(prog.functions[&pick].entries.contains(&get_b), "get_b not an interior entry of pick: {:x?}", prog.functions[&pick].entries);
    assert!(found.contains(&get_c), "get_c not a function: {found:x?}");
}
