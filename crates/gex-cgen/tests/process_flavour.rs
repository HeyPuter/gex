//! `ProcessFlavour`, which both shims decide a new process's unit flavour by
//! and `gexc cgen-flavour` answers from: for each kind of process, the
//! switches it names (`image::flavour_switches`). One test, alone in its
//! binary: the flavour is process-wide.

use gex_cgen::image::{flavour_switches, program_flags, ProcessFlavour};

/// Back to gexc's defaults: no fold, a 32-bit memory.
fn reset() {
    gex_abi::set_kuser_fold(false);
    gex_abi::set_kuser_fold_entry(false);
    gex_abi::set_mem_flavor(gex_abi::MemFlavor::M32);
}

/// An ELF image whose entry has a GEXWASM header with `flags` 0x40 before it.
fn program(flags: u32) -> gex_ld::elf::Image {
    let mut bytes = vec![0u8; 0x80];
    bytes[..8].copy_from_slice(program_flags::MAGIC);
    bytes[36..40].copy_from_slice(&flags.to_le_bytes());
    let seg = gex_ld::elf::Segment {
        vaddr: 0x40_0000,
        mem_size: bytes.len() as u64,
        bytes,
        readable: true,
        writable: false,
        executable: true,
    };
    gex_ld::elf::Image {
        soname: None,
        needed: Vec::new(),
        runpath: Vec::new(),
        bias: 0x40_0000,
        entry: 0x40_0040,
        code: vec![(0x40_0040, 0x40_0080)],
        segments: vec![seg],
        phdr_vaddr: 0x40_0000,
        phentsize: 0,
        phnum: 0,
        brk: 0x40_1000,
        text_range: (0x40_0040, 0x40_0080),
        tls: None,
        symbols: Vec::new(),
        named_relocs: Vec::new(),
        relocs: Vec::new(),
        init_array: Vec::new(),
        dynsyms: Vec::new(),
        irelative: Vec::new(),
        pie: false,
        interp: None,
        class32: true,
    }
}

#[test]
fn each_process_names_its_flavour() {
    use program_flags::{GUEST_I386 as I386, GUEST_X86_64 as X64, KUSER_WINE32 as KUSER};
    assert_eq!(program_flags::of(&program(I386 | KUSER)), I386 | KUSER);
    let mut bare = program(X64);
    bare.segments[0].bytes[0] = 0;
    assert_eq!(program_flags::of(&bare), 0, "no header, no flags");

    let p = |page, flags, mem64, split, native_ntdll| ProcessFlavour { page, flags, mem64, split, native_ntdll };
    let cases: &[(&str, ProcessFlavour, &[&str])] = &[
        ("32-bit Wine, KUSER at its address, natively", p(false, I386 | KUSER, false, false, false), &["--wine32-kuser"]),
        ("32-bit Wine, KUSER at its address, in the page", p(true, I386 | KUSER, false, false, false), &["--wine32-kuser"]),
        ("32-bit Wine without the fold", p(true, I386, false, false, false), &[]),
        ("64-bit Wine natively (no compact space)", p(false, X64, false, false, true), &[]),
        ("64-bit Wine in the page, native ntdll: the compact space", p(true, X64, false, false, true), &["--wine64-compact"]),
        ("64-bit Wine in the page, translated ntdll: the split", p(true, X64, false, false, false), &[]),
        ("64-bit Wine in the page, ?winesplit=1", p(true, X64, false, true, true), &[]),
        ("64-bit Wine in a 64-bit memory (the twin)", p(true, X64, true, false, true), &["--mem64"]),
        ("64-bit Wine natively in a 64-bit memory", p(false, X64, true, false, true), &["--mem64"]),
    ];
    for (what, pf, want) in cases {
        reset();
        pf.apply();
        assert_eq!(flavour_switches(), want.iter().map(|s| s.to_string()).collect::<Vec<_>>(), "{what}");
    }
    reset();
}
