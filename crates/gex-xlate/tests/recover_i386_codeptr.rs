//! i386 code pointers: a function whose address only a `push imm32` names
//! (a callback passed as an argument, as GameMaker's runner registers its
//! builtins) is recovered. A test binary of its own because the decoder's mode
//! is one setting per process (see recover_i386.rs).

#![cfg(feature = "x86")]

use gex_ld::elf::{Image, Segment};
use gex_xlate::func::{recover, RecoverOptions};

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

#[test]
fn a_function_only_a_push_imm32_names_is_recovered() {
    // 10000: push $0x10011; call 10010; add $4,%esp; ret; nop; nop
    // 10010: ret                  (register: takes the callback, never calls it)
    // 10011: xor %eax,%eax; ret   (the callback: no call, jump or relocation names it)
    let text = hex("6811000100e80600000083c404c39090c331c0c3");
    assert_eq!(text.len(), 0x14);
    let len = text.len() as u64;
    let img = Image {
        soname: None,
        needed: Vec::new(),
        runpath: Vec::new(),
        bias: 0,
        entry: 0x10000,
        code: vec![(0x10000, 0x10000 + len)],
        segments: vec![Segment { vaddr: 0x10000, bytes: text, mem_size: len, readable: true, writable: false, executable: true }],
        phdr_vaddr: 0x10000,
        phentsize: 32,
        phnum: 1,
        brk: 0x20000,
        text_range: (0x10000, 0x10000 + len),
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
    };
    let prog = recover(&img, None, &RecoverOptions::default());
    assert!(gex_xlate::arch::compat());
    let found: Vec<u64> = prog.functions.keys().copied().collect();
    assert_eq!(found, vec![0x10000, 0x10010, 0x10011]);
}
