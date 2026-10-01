//! Function recovery on i386 code: an ELF32 program and a PE32 image.
//!
//! A test binary of its own because the decoder's mode (i386 compatibility
//! or long mode) is one setting per process: every test here wants i386.

#![cfg(feature = "x86")]

use gex_ir::func::*;
use gex_ir::GuestReg;
use gex_ld::elf::{Image, Segment, Symbol};
use gex_xlate::func::{recover, recover_pe, RecoverOptions};
use gex_xlate::summary::Summaries;

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

/// GNU as --32, .text at 0x10000 and .rodata at 0x20000:
///
/// ```text
/// f:    push %ebx; mov 12(%esp),%ebx; call 1f; 1: pop %ecx   (PIC idiom)
///       mov 8(%esp),%eax; cmp $3,%eax; ja default; jmp *table(,%eax,4)
///   c0: mov $10,%eax; jmp join   c1: mov $11,%eax; jmp join
///   c2: mov $12,%eax; jmp join   c3: push %ebx; call g; jmp join
///   default: call die
///   join: add %ebx,%eax; pop %ebx; ret
/// g:    mov 4(%esp),%eax; ret $4                             (stdcall)
/// die:  mov $1,%ebx; mov $1,%eax; int $0x80                  (exit)
/// table: .long c0, c1, c2, c3
/// ```
fn elf32() -> Image {
    let text = hex(
        "538b5c240ce800000000598b44240883f8037724ff248500000200b80a000000eb1bb80b000000eb14b80c0000\
         00eb0d53e80b000000eb05e80b00000001d85bc38b442404c20400bb01000000b801000000cd80",
    );
    let ro = hex("1b000100220001002900010030000100");
    Image {
        soname: None,
        needed: Vec::new(),
        runpath: Vec::new(),
        bias: 0,
        entry: 0x10000,
        code: vec![(0x10000, 0x10054)],
        segments: vec![segment(0x10000, text, true), segment(0x20000, ro, false)],
        phdr_vaddr: 0x10000,
        phentsize: 32,
        phnum: 1,
        brk: 0x30000,
        text_range: (0x10000, 0x10054),
        tls: None,
        symbols: vec![sym("f", 0x10000, 0x41), sym("g", 0x10041, 7), sym("die", 0x10048, 12)],
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

fn switches(f: &Function) -> Vec<(Vec<u64>, bool)> {
    f.blocks
        .iter()
        .filter_map(|b| match &b.term {
            FTerm::Switch { cases, table, .. } => Some((cases.iter().map(|c| c.0).collect(), table.complete)),
            _ => None,
        })
        .collect()
}

/// SSA, text round trips and the interpreter checks, for every function.
fn check_all(functions: &std::collections::BTreeMap<u64, Function>, sums: &Summaries, runs: u64) {
    for f in functions.values() {
        let s = sums.to_ssa(f);
        s.verify().unwrap_or_else(|e| panic!("{e}\n{}", gex_ir::text::print(&s)));
        for g in [f, &s] {
            assert_eq!(&gex_ir::text::parse(&gex_ir::text::print(g)).unwrap(), g);
        }
    }
    let rep = gex_xlate::validate::check_program(functions, sums, runs, 1000);
    assert!(rep.failures.is_empty(), "{:#?}", rep.failures);
    assert!(rep.returns_checked > 0, "{rep:?}");
}

#[test]
fn an_i386_elf_program_recovers_with_its_table_stdcall_and_noreturn() {
    let img = elf32();
    let prog = recover(&img, None, &RecoverOptions::default());
    assert!(gex_xlate::arch::compat());
    let found: Vec<u64> = prog.functions.keys().copied().collect();
    // `call 1f; pop %ecx` makes no function at 0x1000a.
    assert_eq!(found, vec![0x10000, 0x10041, 0x10048]);
    assert!(prog.noreturn.contains(&0x10048));
    let f = &prog.functions[&0x10000];
    assert_eq!(
        switches(f),
        vec![(vec![0x1001b, 0x10022, 0x10029, 0x10030], true)],
        "{}",
        gex_ir::text::print(f)
    );

    let sums = gex_xlate::summary::analyze(&prog);
    let sf = sums.get(0x10000);
    let d = gex_xlate::summary::describe(&sf);
    // ebx saved with a 4-byte push and restored: preserved. `ret` pops 4.
    assert!(sf.preserved.contains(GuestReg::X(3)), "{d}");
    assert_eq!(sf.sp_delta, Some(4), "{d}");
    assert!(sf.may_return && !sf.may_leave_pending, "{d}");
    // Reads its return address and two stack arguments, writes nothing of
    // its caller's.
    assert_eq!(sf.stack_inputs, Slots::Range(0, 12), "{d}");
    assert_eq!(sf.caller_frame_writes, Slots::None, "{d}");
    // stdcall: `ret $4` pops the argument too.
    assert_eq!(sums.get(0x10041).sp_delta, Some(8));
    assert!(!sums.get(0x10048).may_return);
    check_all(&prog.functions, &sums, 64);
}

/// `tests/pe/switch32.exe` (see `tests/pe/build.sh`): MinGW -O2, no CRT.
/// `_pick` has a seven-case jump table and a `.cold` part, `_scale@8` is
/// stdcall, `_entry` calls `pick` through a pointer and `ExitProcess`
/// through the IAT.
#[test]
fn a_pe32_image_recovers_from_its_entry_relocations_and_tables() {
    let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/pe/switch32.exe")).unwrap();
    let pe = gex_ld::pe::parse(&bytes).unwrap();
    assert!(!pe.is64);
    let (img, extra) = gex_xlate::func::pe_input(&pe, pe.preferred_base).unwrap();
    assert!(img.class32);
    let prog = recover_pe(&img, &extra, &RecoverOptions::default());
    let found: Vec<u64> = prog.functions.keys().copied().collect();
    // scale (called), pick (a relocated pointer in `hook`), entry.
    for a in [0x401000, 0x401010, 0x4010b0] {
        assert!(found.contains(&a), "{a:#x} not in {found:x?}");
    }
    let pick = &prog.functions[&0x401010];
    let sw = switches(pick);
    assert_eq!(sw.len(), 1, "{}", gex_ir::text::print(pick));
    assert!(sw[0].1, "{sw:x?}");
    assert_eq!(sw[0].0.len(), 7, "{sw:x?}");
    // The `.cold` part is reached from `pick` and belongs to it.
    assert!(pick.block_at(0x401100).is_some());

    let sums = gex_xlate::summary::analyze(&prog);
    // `ret $8` and the return address.
    assert_eq!(sums.get(0x401000).sp_delta, Some(12));
    assert_eq!(sums.get(0x401010).sp_delta, Some(4));
    check_all(&prog.functions, &sums, 16);
}

/// `f`: `push %ebx; mov 8(%esp),%ebx; mov $20,%eax; call *%gs:0x10;
/// add %ebx,%eax; pop %ebx; ret` -- getpid through musl's vsyscall entry --
/// and `stub`: `int $0x80; ret`, which `%gs:0x10` holds.
#[test]
fn a_guarded_vsyscall_is_the_syscall_it_makes() {
    let text = hex("538b5c2408b81400000065ff151000000001d85bc3cd80c3");
    let len = text.len() as u64;
    let mut img = elf32();
    img.segments = vec![segment(0x10000, text, true)];
    img.code = vec![(0x10000, 0x10000 + len)];
    img.text_range = (0x10000, 0x10000 + len);
    img.symbols = vec![sym("f", 0x10000, 0x15), sym("stub", 0x10015, 3)];
    let prog = recover(&img, None, &RecoverOptions::default());
    let f = &prog.functions[&0x10000];

    // Unguarded, the call is to unknown code: nothing is proved.
    let plain = gex_xlate::summary::analyze(&prog);
    let sf = plain.get(0x10000);
    assert!(sf.unknown_callee && !sf.preserved.contains(GuestReg::X(3)));

    let guards = gex_xlate::summary::vsyscall_guards(&prog);
    assert_eq!(guards.calls.get(&0x1000a), Some(&vec![0x10015]), "{guards:?}");
    let sums = gex_xlate::summary::analyze_with_guards(&prog, &guards);
    let sf = sums.get(0x10000);
    let d = gex_xlate::summary::describe(&sf);
    // getpid (20) changes eax only: ebx is restored, the stack balances,
    // and nothing unknown is reached.
    assert!(!sf.unknown_callee && !sf.may_unwind, "{d}");
    assert!(sf.preserved.contains(GuestReg::X(3)) && sf.preserved.contains(GuestReg::X(1)), "{d}");
    assert_eq!(sf.sp_delta, Some(4), "{d}");
    // SSA carries ebx and ecx across the guarded call.
    let s = sums.to_ssa(f);
    s.verify().unwrap();
    check_all(&prog.functions, &sums, 64);

    // The same guard from a profile: S6 names the site by the tier-1 block
    // ending in the call (here f's first), and its target by offset.
    use gex_xlate::profile::{IndirectSite, Loc, ObjectProfile, Profile};
    let mut site = IndirectSite {
        call: true,
        total: 3,
        ..Default::default()
    };
    site.targets.insert(
        Loc {
            obj: "sha256:test".into(),
            off: 0x10015,
        },
        3,
    );
    let mut o = ObjectProfile::default();
    o.indirect.insert(0x10000, site);
    let mut profile = Profile::new("x86_64");
    profile.objects.insert("sha256:test".into(), o);
    let pg = gex_xlate::summary::profile_guards(&prog, &profile, "sha256:test", 0);
    assert_eq!(pg.calls, guards.calls);
    // Another object's id matches nothing.
    assert!(gex_xlate::summary::profile_guards(&prog, &profile, "sha256:other", 0).calls.is_empty());
}
