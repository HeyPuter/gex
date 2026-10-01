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
    let rep = gex_xlate::validate::check_program(&prog.functions, &sums, 8, 1000);
    assert!(rep.failures.is_empty(), "{:#?}", rep.failures);
}
