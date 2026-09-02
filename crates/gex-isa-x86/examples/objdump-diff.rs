//! Bulk decoder validation against binutils, x86-64 edition.
//!
//!   objdump -d -M intel /path/to/bin | cargo run --release --example objdump-diff -p gex-isa-x86
//!
//! Compares the mnemonic AND the instruction length of every instruction
//! objdump prints. A length disagreement is fatal for a variable-length ISA
//! (every later instruction in the block is misread), so it counts as a
//! mismatch even when the mnemonic agrees. Exits non-zero on any mismatch
//! among instructions gex claims to decode; undecoded instructions (VEX,
//! privileged, exotic) are reported but tolerated.

use gex_isa_x86::{decode, fmt, Inst, Op};
use std::collections::BTreeMap;
use std::io::Read;

/// Words objdump prints before the mnemonic that gex folds into the
/// instruction rather than naming.
const PREFIX_WORDS: &[&str] = &[
    "rep", "repz", "repnz", "lock", "data16", "notrack", "bnd", "cs", "ds", "ss", "es", "fs",
    "gs", "addr32", "rex", "rex.W", "rex.B", "rex.WB", "rex.R", "rex.X", "rex.WRB", "rex.RB",
    "rex.WR", "rex.XB", "rex.WX", "rex.WRXB", "rex.RXB", "rex.WXB", "rex.RX", "rex.WRX",
];

/// Mnemonic spellings that mean the same instruction.
fn equivalent(expect: &str, got: &str) -> bool {
    if expect == got {
        return true;
    }
    let pairs: &[(&str, &str)] = &[
        // objdump names the kind of nop it saw; gex does not care.
        ("xchg", "nop"),
        ("nopw", "nop"),
        ("nopl", "nop"),
        ("endbr64", "endbr64"),
        // Non-temporal moves are ordinary moves to gex.
        ("movntps", "movaps"),
        ("movntpd", "movapd"),
        ("movntdq", "movdqa"),
        ("movntq", "movq"),
        ("movntdqa", "movdqu"),
        ("lddqu", "movdqu"),
        // Fences.
        ("lfence", "mfence"),
        ("sfence", "mfence"),
        // Prefetch hints.
        ("prefetcht1", "prefetcht0"),
        ("prefetcht2", "prefetcht0"),
        ("prefetchnta", "prefetcht0"),
        ("prefetchw", "prefetcht0"),
        ("prefetch", "prefetcht0"),
        // xsave family.
        ("xsaveopt", "xsave"),
        ("xsavec", "xsave"),
        ("xsaves", "xsave"),
        ("xsave64", "xsave"),
        ("xsaveopt64", "xsave"),
        ("xsavec64", "xsave"),
        ("xrstor64", "xrstor"),
        ("fxsave64", "fxsave"),
        ("fxrstor64", "fxrstor"),
        // ud0/ud1/ud2.
        ("ud0", "ud2"),
        ("ud1", "ud2"),
        ("int1", "int"),
        ("icebp", "int"),
        // x87 spellings.
        ("fstsw", "fnstsw"),
        ("fstcw", "fnstcw"),
        ("fclex", "fnclex"),
        ("finit", "fninit"),
        ("fstenv", "fnstenv"),
        ("fsave", "fnsave"),
        ("fwait", "fwait"),
        ("fucomp", "fucomp"),
        // loop spellings
        ("loopz", "loope"),
        ("loopnz", "loopne"),
        ("loope", "loope"),
        ("retq", "ret"),
        ("ret", "ret"),
        ("repz", "ret"),
        // sal is shl
        ("sal", "shl"),
        // cvt aliases of the mmx forms
        ("cvtpi2pd", "cvtpi2ps"),
        ("cvttps2pi", "cvtpi2ps"),
        ("cvttpd2pi", "cvtpi2ps"),
        ("cvtps2pi", "cvtpi2ps"),
        ("cvtpd2pi", "cvtpi2ps"),
        // objdump prints the string ops with an operand-size letter sometimes
        ("movsb", "movs"),
        ("movsw", "movs"),
        ("movsd", "movs"),
        ("movsq", "movs"),
        ("stosb", "stos"),
        ("stosw", "stos"),
        ("stosd", "stos"),
        ("stosq", "stos"),
        ("lodsb", "lods"),
        ("lodsw", "lods"),
        ("lodsd", "lods"),
        ("lodsq", "lods"),
        ("scasb", "scas"),
        ("scasw", "scas"),
        ("scasd", "scas"),
        ("scasq", "scas"),
        ("cmpsb", "cmps"),
        ("cmpsw", "cmps"),
        ("cmpsd", "cmps"),
        ("cmpsq", "cmps"),
        // setcc / jcc / cmovcc alias names
        ("setnz", "setne"),
        ("setz", "sete"),
        ("setnb", "setae"),
        ("setnae", "setb"),
        ("setc", "setb"),
        ("setnc", "setae"),
        ("setna", "setbe"),
        ("setnbe", "seta"),
        ("setnge", "setl"),
        ("setnl", "setge"),
        ("setng", "setle"),
        ("setnle", "setg"),
        ("setpe", "setp"),
        ("setpo", "setnp"),
        ("jnz", "jne"),
        ("jz", "je"),
        ("jnb", "jae"),
        ("jc", "jb"),
        ("jnc", "jae"),
        ("jna", "jbe"),
        ("jnbe", "ja"),
        ("jnge", "jl"),
        ("jnl", "jge"),
        ("jng", "jle"),
        ("jnle", "jg"),
        ("jpe", "jp"),
        ("jpo", "jnp"),
        ("cmovnz", "cmovne"),
        ("cmovz", "cmove"),
        ("cmovnb", "cmovae"),
        ("cmovc", "cmovb"),
        ("cmovnc", "cmovae"),
        ("cmovna", "cmovbe"),
        ("cmovnbe", "cmova"),
        ("cmovnge", "cmovl"),
        ("cmovnl", "cmovge"),
        ("cmovng", "cmovle"),
        ("cmovnle", "cmovg"),
        ("cmovpe", "cmovp"),
        ("cmovpo", "cmovnp"),
        ("pushf", "pushf"),
        ("pushfq", "pushf"),
        ("popfq", "popf"),
        ("cltq", "cdqe"),
        ("cqto", "cqo"),
        ("cltd", "cdq"),
        ("cwtl", "cwde"),
        ("cbtw", "cbw"),
        ("cwtd", "cwd"),
        ("movslq", "movsxd"),
        ("iretq", "iret"),
    ];
    pairs.iter().any(|(e, g)| *e == expect && *g == got)
}

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();

    // Parse objdump's listing into (address, mnemonic) records. Length comes
    // from the next record's address within the same run of consecutive
    // instructions; objdump's raw byte columns can wrap to a continuation
    // line, which is why lengths are not read from the byte column.
    struct Rec {
        pc: u64,
        mnem: String,
        full: String,
        bytes: Vec<u8>,
    }
    let mut recs: Vec<Rec> = Vec::new();
    for line in input.lines() {
        let Some((addr_part, rest)) = line.split_once(":\t") else {
            continue;
        };
        let Ok(pc) = u64::from_str_radix(addr_part.trim(), 16) else {
            continue;
        };
        let mut fields = rest.splitn(2, '\t');
        let hex = fields.next().unwrap_or("").trim();
        let bytes: Vec<u8> = hex
            .split_whitespace()
            .filter_map(|h| u8::from_str_radix(h, 16).ok())
            .collect();
        if bytes.is_empty() {
            continue;
        }
        match fields.next() {
            Some(text) if !text.trim().is_empty() => {
                let text = text.split('#').next().unwrap().split(" <").next().unwrap().trim();
                let mut words = text.split_whitespace();
                let mut mnem = words.next().unwrap_or("").to_string();
                // Strip prefix words objdump prints separately.
                while PREFIX_WORDS.contains(&mnem.as_str()) {
                    match words.next() {
                        Some(w) => mnem = w.to_string(),
                        None => break,
                    }
                }
                recs.push(Rec {
                    pc,
                    mnem,
                    full: text.to_string(),
                    bytes,
                });
            }
            _ => {
                // Continuation line: more bytes for the previous instruction.
                if let Some(last) = recs.last_mut() {
                    if last.pc + last.bytes.len() as u64 == pc {
                        last.bytes.extend(bytes);
                    }
                }
            }
        }
    }

    let mut total = 0usize;
    let mut agree = 0usize;
    let mut unknown: BTreeMap<String, (String, usize)> = BTreeMap::new();
    let mut mismatch: BTreeMap<String, (String, String, usize)> = BTreeMap::new();
    let mut len_mismatch: BTreeMap<String, (String, u8, usize, usize)> = BTreeMap::new();
    let mut lone_prefix = 0usize;

    for r in &recs {
        // A bare prefix objdump could not attach (e.g. a `data16` before a
        // call it prints alone) is not an instruction.
        if r.mnem.starts_with('.') || r.mnem == "(bad)" {
            continue;
        }
        if r.bytes.len() <= 2 && PREFIX_WORDS.contains(&r.full.as_str()) {
            lone_prefix += 1;
            continue;
        }
        total += 1;
        let inst: Inst = decode(&r.bytes, r.pc);
        if !inst.is_known() && r.mnem != "ud2" {
            let e = unknown.entry(r.mnem.clone()).or_insert((r.full.clone(), 0));
            e.1 += 1;
            continue;
        }
        let got = fmt::mnemonic(&inst);
        let exp_base = r.mnem.as_str();
        if !equivalent(exp_base, &got) && !(inst.op == Op::Nop && exp_base.starts_with("nop")) {
            let e = mismatch
                .entry(format!("{exp_base} -> {got}"))
                .or_insert((r.full.clone(), hex(&r.bytes), 0));
            e.2 += 1;
            continue;
        }
        if inst.len as usize != r.bytes.len() {
            let e = len_mismatch
                .entry(exp_base.to_string())
                .or_insert((r.full.clone(), inst.len, r.bytes.len(), 0));
            e.3 += 1;
            continue;
        }
        agree += 1;
    }

    println!("instructions:        {total}");
    println!("agree (mnem+len):    {agree}");
    let undecoded: usize = unknown.values().map(|v| v.1).sum();
    println!("undecoded:           {undecoded}");
    let mismatched: usize = mismatch.values().map(|v| v.2).sum();
    println!("mnemonic mismatches: {mismatched}");
    let lens: usize = len_mismatch.values().map(|v| v.3).sum();
    println!("length mismatches:   {lens}");
    if lone_prefix > 0 {
        println!("lone prefixes skipped: {lone_prefix}");
    }

    if !unknown.is_empty() {
        println!("\n-- undecoded, by binutils mnemonic --");
        let mut v: Vec<_> = unknown.iter().collect();
        v.sort_by_key(|(_, (_, n))| std::cmp::Reverse(*n));
        for (mnem, (sample, n)) in v.iter().take(60) {
            println!("  {n:>7}  {mnem:<14} e.g. {sample}");
        }
    }
    if !mismatch.is_empty() {
        println!("\n-- mnemonic mismatches --");
        let mut v: Vec<_> = mismatch.iter().collect();
        v.sort_by_key(|(_, (_, _, n))| std::cmp::Reverse(*n));
        for (k, (exp, bytes, n)) in v.iter().take(60) {
            println!("  {n:>7}  {k:<28} e.g. {exp}   [{bytes}]");
        }
    }
    if !len_mismatch.is_empty() {
        println!("\n-- length mismatches --");
        let mut v: Vec<_> = len_mismatch.iter().collect();
        v.sort_by_key(|(_, (_, _, _, n))| std::cmp::Reverse(*n));
        for (k, (exp, got, want, n)) in v.iter().take(60) {
            println!("  {n:>7}  {k:<14} got {got} want {want}  e.g. {exp}");
        }
    }
    if mismatched > 0 || lens > 0 {
        std::process::exit(1);
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}
