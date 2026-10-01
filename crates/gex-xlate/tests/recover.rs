//! Function recovery, jump tables and summaries on a small hand-written
//! program, one per guest architecture:
//!
//! ```text
//! f(i, v):  save a callee-saved register, keep v in it
//!           switch (i) { 0,1,2: return 10+i+v; 3: return g()+v; default: die() }
//! g():      return 5
//! die():    exit(1)          -- never returns
//! ```
//!
//! The jump table is bounded by `cmp; b.hi` / `cmp; ja` and lives in
//! read-only memory, so it must come back complete; the call to `die` must
//! have no continuation; the summaries must prove the saved register
//! preserved and `die` noreturn; and every function must survive the
//! interpreter checks of [`gex_xlate::validate`].

use gex_ir::func::*;
use gex_ir::GuestReg;
use gex_ld::elf::{Image, Segment, Symbol};
use gex_xlate::func::{recover, RecoverOptions};

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn segment(vaddr: u64, bytes: Vec<u8>, executable: bool) -> Segment {
    let len = bytes.len() as u64;
    Segment {
        vaddr,
        bytes,
        mem_size: len,
        readable: true,
        writable: false,
        executable,
    }
}

fn sym(name: &str, value: u64, size: u64) -> Symbol {
    Symbol {
        name: name.into(),
        value,
        size,
        ifunc: false,
    }
}

fn image(segments: Vec<Segment>, code: (u64, u64), symbols: Vec<Symbol>) -> Image {
    Image {
        soname: None,
        needed: Vec::new(),
        runpath: Vec::new(),
        bias: 0,
        entry: code.0,
        code: vec![code],
        segments,
        phdr_vaddr: code.0,
        phentsize: 56,
        phnum: 1,
        brk: 0x30000,
        text_range: code,
        tls: None,
        symbols,
        named_relocs: Vec::new(),
        relocs: Vec::new(),
        init_array: Vec::new(),
        dynsyms: Vec::new(),
        irelative: Vec::new(),
        pie: false,
        interp: None,
        class32: false,
    }
}

/// The program, where `f`, `g` and `die` are, the four case addresses, and
/// the callee-saved register `f` keeps `v` in.
#[cfg(not(feature = "x86"))]
fn program() -> (Image, [u64; 3], [u64; 4], GuestReg) {
    // aarch64-linux-musl-as, linked at 0x10000:
    //   f:    stp x29,x30,[sp,#-32]!; str x19,[sp,#16]; mov x19,x1
    //         cmp w0,#3; b.hi default; adr x1,table; ldrb w2,[x1,w0,uxtw]
    //         adr x3,base; add x3,x3,w2,sxtb #2; br x3
    //   base: c0: mov x0,#10; b join  c1: mov x0,#11; b join
    //         c2: mov x0,#12; b join  c3: bl g; b join
    //   default: bl die
    //   join: add x0,x0,x19; ldr x19,[sp,#16]; ldp x29,x30,[sp],#32; ret
    //   g:    mov x0,#5; ret
    //   die:  mov x0,#1; mov x8,#93; svc #0
    //   table: .byte 0,2,4,6
    let bytes = hex(
        "fd7bbea9f30b00f9f30301aa1f0c0071c8010054e102001022486038630000106388228b60001fd6\
         400180d208000014600180d206000014800180d2040000140700009402000014070000940000138b\
         f30b40f9fd7bc2a8c0035fd6a00080d2c0035fd6200080d2a80b80d2010000d400020406",
    );
    let img = image(
        vec![segment(0x10000, bytes, true)],
        (0x10000, 0x10070),
        vec![sym("f", 0x10000, 0x5c), sym("g", 0x1005c, 8), sym("die", 0x10064, 12)],
    );
    (
        img,
        [0x10000, 0x1005c, 0x10064],
        [0x10028, 0x10030, 0x10038, 0x10040],
        GuestReg::X(19),
    )
}

#[cfg(feature = "x86")]
fn program() -> (Image, [u64; 3], [u64; 4], GuestReg) {
    // GNU as, .text at 0x10000 and .rodata at 0x20000:
    //   f:    push %rbx; mov %rsi,%rbx; cmp $3,%edi; ja default
    //         mov %edi,%eax; lea table(%rip),%rdx; movslq (%rdx,%rax,4),%rax
    //         add %rdx,%rax; jmp *%rax
    //   c0: mov $10,%eax; jmp join  c1: mov $11,%eax; jmp join
    //   c2: mov $12,%eax; jmp join  c3: call g; jmp join
    //   default: call die
    //   join: add %rbx,%rax; pop %rbx; ret
    //   g:    mov $5,%eax; ret
    //   die:  mov $1,%edi; mov $60,%eax; syscall
    //   table: .long c0-table, c1-table, c2-table, c3-table
    let text = hex(
        "534889f383ff03772e89f8488d15eeff0000486304824801d0ffe0b80a000000eb1ab80b000000eb\
         13b80c000000eb0ce80c000000eb05e80b0000004801d85bc3b805000000c3bf01000000b83c0000\
         000f05",
    );
    let ro = hex("1b00ffff2200ffff2900ffff3000ffff");
    let img = image(
        vec![segment(0x10000, text, true), segment(0x20000, ro, false)],
        (0x10000, 0x10053),
        vec![sym("f", 0x10000, 0x41), sym("g", 0x10041, 6), sym("die", 0x10047, 12)],
    );
    (
        img,
        [0x10000, 0x10041, 0x10047],
        [0x1001b, 0x10022, 0x10029, 0x10030],
        GuestReg::X(3),
    )
}

#[test]
fn recovers_functions_a_complete_table_and_a_noreturn_call() {
    // An AArch64/x86-64 fixture (64-bit little-endian ELF or A64 code).
    if gex_abi::PPC {
        return;
    }
    let (img, [f, g, die], cases, _) = program();
    let prog = recover(&img, None, &RecoverOptions::default());
    let found: Vec<u64> = prog.functions.keys().copied().collect();
    assert_eq!(found, vec![f, g, die]);
    assert!(prog.noreturn.contains(&die) && !prog.noreturn.contains(&g));

    let func = &prog.functions[&f];
    func.verify().unwrap();
    let switches: Vec<&FTerm> = func
        .blocks
        .iter()
        .map(|b| &b.term)
        .filter(|t| matches!(t, FTerm::Switch { .. }))
        .collect();
    assert_eq!(switches.len(), 1, "{}", gex_ir::text::print(func));
    let FTerm::Switch { cases: got, table, .. } = switches[0] else { unreachable!() };
    assert!(table.complete, "{table:?}");
    assert_eq!(table.count, 4);
    let got: Vec<u64> = got.iter().map(|c| c.0).collect();
    assert_eq!(got, cases.to_vec());
    // Every case is a block of f.
    for c in cases {
        assert!(func.block_at(c).is_some(), "case {c:#x} has no block");
    }
    // The call to die has no continuation; the call to g has one.
    let calls: Vec<(u64, bool)> = func
        .blocks
        .iter()
        .filter_map(|b| match b.term {
            FTerm::Call {
                callee: Callee::Direct(t),
                ref ret,
                ..
            } => Some((t, ret.is_some())),
            _ => None,
        })
        .collect();
    assert!(calls.contains(&(die, false)) && calls.contains(&(g, true)), "{calls:?}");
    assert_eq!(prog.stats.unresolved, 0);
    assert_eq!(prog.stats.symbols_found, 3);
}

#[test]
fn summaries_prove_what_the_program_does() {
    // An AArch64/x86-64 fixture (64-bit little-endian ELF or A64 code).
    if gex_abi::PPC {
        return;
    }
    let (img, [f, g, die], _, saved) = program();
    let prog = recover(&img, None, &RecoverOptions::default());
    let sums = gex_xlate::summary::analyze(&prog);
    assert_eq!(sums.stats.unconverged, 0);
    let sp = gex_xlate::arch::SP_REG;
    let ret_delta = if gex_abi::X86 { 8 } else { 0 };

    let sf = sums.get(f);
    assert!(sf.preserved.contains(saved), "{}", gex_xlate::summary::describe(&sf));
    assert!(!sf.preserved.contains(GuestReg::X(0)));
    assert_eq!(sf.sp_delta, Some(ret_delta));
    assert!(sf.may_return && !sf.may_leave_pending && !sf.unknown_callee);
    // Stack traffic stays in its own frame (x86-64: bar the return address
    // `ret` reads).
    assert_eq!(sf.caller_frame_writes, Slots::None);
    let own = if gex_abi::X86 { Slots::Range(0, 8) } else { Slots::None };
    assert_eq!(sf.stack_inputs, own);
    // Nothing of the caller's escapes; the result derives from v.
    assert!(sf.escaping_inputs.is_empty(), "{}", gex_xlate::summary::describe(&sf));

    let sg = sums.get(g);
    assert_eq!(sg.sp_delta, Some(ret_delta));
    assert!(sg.preserved.contains(saved) && !sg.preserved.contains(GuestReg::X(0)));
    assert!(sg.output_sources.minus(RegSet::of(&[sp])).is_empty());

    let sd = sums.get(die);
    assert!(!sd.may_return);
}

#[test]
fn every_function_runs_the_same_in_both_forms_and_keeps_its_summary() {
    // An AArch64/x86-64 fixture (64-bit little-endian ELF or A64 code).
    if gex_abi::PPC {
        return;
    }
    let (img, _, _, _) = program();
    let prog = recover(&img, None, &RecoverOptions::default());
    let sums = gex_xlate::summary::analyze(&prog);
    for f in prog.functions.values() {
        let s = sums.to_ssa(f);
        s.verify().unwrap_or_else(|e| panic!("{e}\n{}", gex_ir::text::print(&s)));
        for g in [f, &s] {
            let t = gex_ir::text::print(g);
            assert_eq!(&gex_ir::text::parse(&t).unwrap(), g);
        }
    }
    let rep = gex_xlate::validate::check_program(&prog.functions, &sums, 64, 1000);
    assert!(rep.failures.is_empty(), "{:#?}", rep.failures);
    assert!(rep.returns_checked > 0, "{rep:?}");
}
