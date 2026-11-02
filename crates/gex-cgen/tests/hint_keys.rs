//! A process's "build it with" line (the browser shim's `gex_cgen_missing`
//! and its trampolines line) names `image::flavour_switches`; a unit built
//! with those switches must have the very key the process looks up. One
//! test, alone in its binary: the flavour is process-wide.

use gex_cgen::image::{flavour_switches, image_key, set_flavour_switch, set_max_insns, DEFAULT_MAX_INSNS};

/// A small fixed image (gex-key's).
fn fixture() -> gex_ld::elf::Image {
    let seg = |vaddr: u64, bytes: Vec<u8>, executable: bool| gex_ld::elf::Segment {
        vaddr,
        mem_size: bytes.len() as u64,
        bytes,
        readable: true,
        writable: !executable,
        executable,
    };
    gex_ld::elf::Image {
        soname: None,
        needed: Vec::new(),
        runpath: Vec::new(),
        bias: 0x40_0000,
        entry: 0x40_1000,
        code: vec![(0x40_1000, 0x40_1010)],
        segments: vec![
            seg(0x40_1000, vec![0x55, 0x89, 0xe5, 0x31, 0xc0, 0x5d, 0xc3, 0x90], true),
            seg(0x40_2000, b"data".to_vec(), false),
        ],
        phdr_vaddr: 0x40_0000,
        phentsize: 0,
        phnum: 0,
        brk: 0x40_3000,
        text_range: (0x40_1000, 0x40_1010),
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

/// Back to gexc's defaults: no fold, a 32-bit memory, the default limit.
fn reset() {
    gex_abi::set_kuser_fold(false);
    gex_abi::set_kuser_fold_entry(false);
    gex_abi::set_mem_flavor(gex_abi::MemFlavor::M32);
    set_max_insns(DEFAULT_MAX_INSNS);
}

/// Apply switches as gexc's `cgen-image` does.
fn apply(switches: &[String]) {
    let mut it = switches.iter();
    while let Some(s) = it.next() {
        if s == "--max-insns" {
            set_max_insns(it.next().expect("--max-insns N").parse().unwrap());
        } else {
            assert!(set_flavour_switch(s), "{s}: not a switch gexc takes");
        }
    }
}

#[test]
fn a_hints_switches_build_the_key_the_process_looks_up() {
    let img = fixture();
    reset();
    let plain = image_key(&img);
    // Each flavour a process can be: (fold, entry fold, memory64, limit).
    let flavours = [
        (false, false, false, DEFAULT_MAX_INSNS),
        (true, false, false, DEFAULT_MAX_INSNS),
        (true, true, false, DEFAULT_MAX_INSNS),
        (false, false, true, DEFAULT_MAX_INSNS),
        (true, true, true, DEFAULT_MAX_INSNS),
        (false, false, false, 500),
        (true, true, false, 500),
    ];
    for (fold, entry, m64, max) in flavours {
        reset();
        gex_abi::set_kuser_fold(fold);
        gex_abi::set_kuser_fold_entry(entry);
        gex_abi::set_mem_flavor(if m64 { gex_abi::MemFlavor::M64 } else { gex_abi::MemFlavor::M32 });
        set_max_insns(max);
        let looked_up = image_key(&img);
        let hint = flavour_switches();
        reset();
        apply(&hint);
        let built = image_key(&img);
        let what = format!("fold {fold}, entry {entry}, mem64 {m64}, max {max}: hint {hint:?}");
        assert_eq!(built, looked_up, "{what}");
        // And a flavour that moves the key is one the hint names.
        assert_eq!(looked_up != plain, !hint.is_empty(), "{what}");
    }
    reset();
}
