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

/// The return check of an AOT unit (and of the function JIT) expects SP
/// back past what the `ret` pops: 4 for `ret`, 8 for g's stdcall `ret $4`.
/// With only 4, every stdcall return failed its check and left.
#[cfg(feature = "cgen")]
#[test]
fn the_return_check_counts_what_ret_n_pops() {
    let img = elf32();
    let prog = recover(&img, None, &RecoverOptions::default());
    let sums = gex_xlate::summary::analyze(&prog).map;
    let byte = |a: u64| {
        img.segments.iter().find_map(|s| s.byte(a))
    };
    let funcs = gex_cgen::select(
        prog.functions.into_values().collect(),
        &std::collections::HashMap::new(),
        &std::collections::HashSet::new(),
    )
    .unwrap();
    // g's `ret $4` is at 0x10045.
    let pops = gex_cgen::func::ret_pops(&funcs, true, &byte);
    assert_eq!(pops, [(0x10045u64, 8u64)].into_iter().collect());
    let c = gex_cgen::standalone::standalone(&funcs, &[(0x10000, "f".into()), (0x10041, "g".into())], &sums, true, &byte)
        .unwrap();
    let body = |entry: u64| {
        let def = format!("{}(int ctx, int32_t depth, uint32_t need, uint64_t ret) {{", gex_cgen::func::body_name(entry));
        let start = c.find(&def).unwrap_or_else(|| panic!("no {def} in\n{c}"));
        let rest = &c[start..];
        rest[..rest.find("\n}\n").unwrap()].to_string()
    };
    let (f, g) = (body(0x10000), body(0x10041));
    assert!(f.contains("== sp0 + 4ull"), "{f}");
    assert!(g.contains("== sp0 + 8ull") && !g.contains("== sp0 + 4ull"), "{g}");
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

/// Every block of every function, by the address its terminator is at.
fn illegal_blocks(prog: &gex_xlate::func::Program) -> Vec<u64> {
    prog.functions
        .values()
        .flat_map(|f| f.blocks.iter())
        .filter(|b| matches!(b.term, FTerm::Illegal { .. }))
        .map(|b| b.start)
        .collect()
}

/// A PE section's trailing zero bytes are not stored (`PeImage::segments`),
/// so an instruction that ends `.text` in a zero byte used to be cut short
/// and lift as illegal: libb.dll's `ret $0xc` (`c2 0c 00`), and an import
/// thunk `jmp *slot` last in `.text` once the image is rebased where the
/// slot's address has a zero high byte (Noita's lua51.dll at 0xb70000:
/// `ff 25 1c f0 bb 00`). liba.dll's thunk is followed by two `nop`s of
/// padding; zeroed, it is that shape at base 0x10000 (slot 0x15030).
#[test]
fn an_instruction_ending_text_in_a_zero_byte_lifts_whole() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../gex-ld/tests/pe/bin/x86");
    let libb = gex_ld::pe::parse(&std::fs::read(format!("{dir}/libb.dll")).unwrap()).unwrap();
    let (img, extra) = gex_xlate::func::pe_input(&libb, libb.preferred_base).unwrap();
    let prog = recover_pe(&img, &extra, &RecoverOptions::default());
    assert!(illegal_blocks(&prog).is_empty(), "{:x?}", illegal_blocks(&prog));
    let ret = libb.preferred_base + 0x1039;
    let f = prog.functions.values().find(|f| f.blocks.iter().any(|b| b.term_pc == ret)).expect("the ret $0xc");
    let b = f.blocks.iter().find(|b| b.term_pc == ret).unwrap();
    assert!(matches!(b.term, FTerm::Return { .. }), "{}", gex_ir::text::print(f));

    let mut bytes = std::fs::read(format!("{dir}/liba.dll")).unwrap();
    // .text's raw data starts at file offset 0x400; the thunk is at +0x34.
    assert_eq!(bytes[0x434..0x43c], [0xff, 0x25, 0x30, 0x50, 0x6c, 0x65, 0x90, 0x90]);
    bytes[0x43a..0x43c].fill(0);
    let liba = gex_ld::pe::parse(&bytes).unwrap();
    let base = 0x10000;
    let (img, extra) = gex_xlate::func::pe_input(&liba, base).unwrap();
    let prog = recover_pe(&img, &extra, &RecoverOptions::default());
    assert!(illegal_blocks(&prog).is_empty(), "{:x?}", illegal_blocks(&prog));
    let thunk = &prog.functions[&(base + 0x1034)];
    let text = gex_ir::text::print(thunk);
    assert_eq!(thunk.blocks.len(), 1, "{text}");
    assert!(matches!(thunk.blocks[0].term, FTerm::Unresolved { .. }), "{text}");
    assert!(text.contains(&format!("{:#x}", base + 0x5030)), "the slot, rebased: {text}");
}

/// Microsoft's linker puts the import address table at the start of
/// `.text`: once the loader has written the imports' addresses there, the
/// mapped code still counts as the image's (`pe_code_difference`), but any
/// other byte that differs does not.
#[test]
fn import_slots_written_by_the_loader_are_not_a_code_difference() {
    let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/pe/switch32.exe")).unwrap();
    let pe = gex_ld::pe::parse(&bytes).unwrap();
    let base = pe.preferred_base;
    let (img, _) = gex_xlate::func::pe_input(&pe, base).unwrap();
    let slots = gex_xlate::pe_import_slots(&pe);
    let (lo, _) = slots[0];
    let seg = img.segments.iter().find(|s| s.vaddr <= base + lo && base + lo < s.vaddr + s.bytes.len() as u64).unwrap();
    let at = (base + lo - seg.vaddr) as usize;
    assert_eq!(&seg.bytes[at..at + 4], &[0; 4]);
    let mut live = seg.bytes.clone();
    live[at..at + 4].copy_from_slice(&0x7b01_2345u32.to_le_bytes());
    assert_eq!(gex_xlate::pe_code_difference(seg.vaddr, &seg.bytes, &live, base, Some(&pe)), None);
    assert_eq!(gex_xlate::pe_code_difference(seg.vaddr, &seg.bytes, &live, base, None), Some(at));
    let other = if at > 0 { 0 } else { 4 };
    live[other] ^= 0xff;
    assert_eq!(gex_xlate::pe_code_difference(seg.vaddr, &seg.bytes, &live, base, Some(&pe)), Some(other));
}

/// A page's hash leaves the import slots out: the loader filling them after
/// a unit was placed does not make the page read as patched.
#[test]
fn a_page_hash_leaves_import_slots_out() {
    let mut mem = vec![0x90u8; 0x3000];
    let slots = [(0x1010u64, 0x1018u64)];
    let h = |m: &[u8]| gex_xlate::masked_page_hash(m, 1, 0x1000, &slots);
    let before = h(&mem);
    assert_eq!(gex_xlate::masked_page_hash(&mem, 1, 0x1000, &[]), gex_xlate::masked_page_hash(&mem, 1, 0x1000, &[(0x2000, 0x2004)]));
    mem[0x1010..0x1018].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(h(&mem), before);
    mem[0x1018] = 0xcc;
    assert_ne!(h(&mem), before);
}

/// A vtable kept in `.text` (Microsoft merges `.rdata` there): the relocated
/// pointer to the table seeds a weak start whose decode runs on into
/// `get_b`, which only the table names. `get_b` is a function of its own
/// all the same, not an interior entry of that garbage (uiribbon.dll's
/// 0x2f15153d, called through its vtable a few thousand times at start-up).
#[test]
fn a_virtual_tail_s_callee_after_a_vtable_in_text_is_a_function() {
    let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/pe/vtext32.exe")).unwrap();
    let pe = gex_ld::pe::parse(&bytes).unwrap();
    let (img, extra) = gex_xlate::func::pe_input(&pe, pe.preferred_base).unwrap();
    let prog = recover_pe(&img, &extra, &RecoverOptions::default());
    let found: Vec<u64> = prog.functions.keys().copied().collect();
    assert!(found.contains(&0x401010), "get_b not a function: {found:x?}");
}

/// `tests/pe/ptext32.exe` (see `tests/pe/build.sh`): a function pointer in
/// `.text` with its target right after it, called through (`call *_slot`).
/// The call's relocated operand is the slot's address, which is data: taken
/// for a start, its bytes decoded straight into the stub and made it an
/// interior block of a function at the slot, so the stub had no function of
/// its own and every call through the slot left the unit (shell32.dll's
/// lazily bound wrappers, the Win7 shell's hottest call-outs).
#[test]
fn a_pointer_slot_in_text_is_not_a_function_and_its_target_is() {
    let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/pe/ptext32.exe")).unwrap();
    let pe = gex_ld::pe::parse(&bytes).unwrap();
    let (img, extra) = gex_xlate::func::pe_input(&pe, pe.preferred_base).unwrap();
    let prog = recover_pe(&img, &extra, &RecoverOptions::default());
    let found: Vec<u64> = prog.functions.keys().copied().collect();
    let (slot, stub) = (0x401000, 0x401009);
    assert!(!found.contains(&slot), "the slot {slot:#x} is a function: {found:x?}");
    assert!(found.contains(&stub), "the stub {stub:#x} is not a function: {found:x?}");
}
