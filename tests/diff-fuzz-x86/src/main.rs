//! Instruction-level differential fuzzing of the x86-64 lifter.
//!
//! The oracle is the host CPU: this only runs on an x86-64 machine, where the
//! generated program can simply be executed natively.
//!
//! 1. pick random instructions from a table of templates -- integer ALU,
//!    shifts, multiplies, bit ops, conditional moves, SSE lane ops, a few x87
//!    forms -- with random registers, widths and immediates;
//! 2. wrap them in a harness that seeds every GPR, the flags, the xmm file and
//!    a scratch buffer from a random blob, runs the candidates, then dumps the
//!    register file, the flags, the xmm registers and the buffer to stdout;
//! 3. assemble with the system `as`/`ld`;
//! 4. run natively and under `gexc`, and compare the bytes.
//!
//! Flags that an instruction leaves undefined are masked out of the compare,
//! so a difference is a real lifter bug with a one-instruction reproducer.
//!
//!   gex-difffuzz-x86 [--iters N] [--seed S] [--seq-len N] [--gexc PATH]
//!                    [--workdir DIR] [--verbose] [--only CLASS] [--bits 32]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `--bits 32`: generate and run 32-bit (i386) code instead. The host runs
/// an i386 static binary directly, so the oracle is still the CPU; gex runs
/// it in compatibility mode. Eight registers, no 64-bit widths, `int 0x80`.
static BITS32: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
fn bits32() -> bool {
    BITS32.load(std::sync::atomic::Ordering::Relaxed)
}

/// Register holding the blob address; never a candidate operand: r15 in
/// 64-bit code, ebp in 32-bit (the string operations need edi and esi).
fn base() -> &'static str {
    if bits32() { "ebp" } else { "r15" }
}
/// GPRs the candidates may use (by hardware number): everything but rsp
/// and the base register.
const GPRS: [u8; 14] = [0, 1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14];
const GPRS32: [u8; 6] = [0, 1, 2, 3, 6, 7];
fn gprs() -> &'static [u8] {
    if bits32() { &GPRS32 } else { &GPRS }
}
/// Blob layout: 16 GPR slots, flags, 16 xmm, scratch.
const OUT_GPR: usize = 0;
const OUT_FLAGS: usize = 16 * 8;
/// 16-byte aligned: aligned SSE memory operands fault natively otherwise.
const OUT_XMM: usize = 16 * 8 + 16;
const OUT_SCRATCH: usize = OUT_XMM + 16 * 16;
const SCRATCH_LEN: usize = 512;
const OUT_LEN: usize = OUT_SCRATCH + SCRATCH_LEN;

const CF: u64 = 1;
const PF: u64 = 4;
const AF: u64 = 0x10;
const ZF: u64 = 0x40;
const SF: u64 = 0x80;
const OF: u64 = 0x800;
const ALL: u64 = CF | PF | AF | ZF | SF | OF;
const NONE: u64 = 0;

struct Config {
    iters: usize,
    seed: u64,
    seq_len: usize,
    gexc: PathBuf,
    workdir: PathBuf,
    verbose: bool,
    only: Option<String>,
    bits32: bool,
}

fn main() {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gex-difffuzz-x86: {e}");
            eprintln!("usage: gex-difffuzz-x86 [--iters N] [--seed S] [--seq-len N] [--gexc PATH] [--workdir DIR] [--verbose] [--only CLASS] [--bits 32]");
            std::process::exit(2);
        }
    };
    if !cfg.gexc.exists() {
        eprintln!("gex-difffuzz-x86: missing {}", cfg.gexc.display());
        std::process::exit(2);
    }
    std::fs::create_dir_all(&cfg.workdir).expect("workdir");
    BITS32.store(cfg.bits32, std::sync::atomic::Ordering::Relaxed);
    let mut rng = Rng::new(cfg.seed);
    let mut failures = 0;
    let mut tested = 0;
    let mut by_class: std::collections::BTreeMap<&'static str, (usize, usize)> = Default::default();
    for i in 0..cfg.iters {
        let mut insts: Vec<Candidate> = Vec::new();
        while insts.len() < cfg.seq_len {
            let c = gen(&mut rng);
            if let Some(only) = &cfg.only {
                if c.class != only {
                    continue;
                }
            }
            insts.push(c);
        }
        let blob: Vec<u8> = (0..OUT_LEN).map(|_| rng.next() as u8).collect();
        let asm = harness(&insts, &blob, &mut rng);
        let dir = cfg.workdir.join(format!("case{i}"));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("t.S");
        std::fs::write(&src, &asm).unwrap();
        let bin = dir.join("t");
        if !assemble(&src, &bin) {
            eprintln!("case {i}: assembler rejected:\n{}", insts.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join("\n"));
            continue;
        }
        let native = run(&bin, &[]);
        let gex = run(&cfg.gexc, &["run".into(), bin.to_string_lossy().into_owned()]);
        tested += 1;
        for c in &insts {
            by_class.entry(c.class).or_insert((0, 0)).0 += 1;
        }
        let mask = insts.last().map(|c| c.flag_mask).unwrap_or(ALL);
        match compare(&native, &gex, mask) {
            None => {
                if cfg.verbose {
                    eprintln!("case {i}: ok  {}", insts.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join(" ; "));
                }
                let _ = std::fs::remove_dir_all(&dir);
            }
            Some(why) => {
                failures += 1;
                for c in &insts {
                    by_class.entry(c.class).or_insert((0, 0)).1 += 1;
                }
                eprintln!("case {i}: MISMATCH {why}");
                for c in &insts {
                    eprintln!("    {}", c.text);
                }
                eprintln!("    kept in {}", dir.display());
            }
        }
    }
    eprintln!("\n{tested} cases, {failures} mismatches");
    let mut v: Vec<_> = by_class.into_iter().collect();
    v.sort();
    for (class, (n, bad)) in v {
        eprintln!("  {class:<12} {n:>6} run  {bad:>4} in failing cases");
    }
    if failures > 0 {
        std::process::exit(1);
    }
}

fn parse_args() -> Result<Config, String> {
    let mut cfg = Config {
        iters: 200,
        seed: 1,
        seq_len: 3,
        gexc: PathBuf::from("target/x86/release/gexc"),
        workdir: std::env::temp_dir().join("gex-difffuzz-x86"),
        verbose: false,
        only: None,
        bits32: false,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let next = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i).cloned().ok_or_else(|| format!("{} needs a value", args[*i - 1]))
        };
        match args[i].as_str() {
            "--iters" => cfg.iters = next(&mut i)?.parse().map_err(|e| format!("{e}"))?,
            "--seed" => cfg.seed = next(&mut i)?.parse().map_err(|e| format!("{e}"))?,
            "--seq-len" => cfg.seq_len = next(&mut i)?.parse().map_err(|e| format!("{e}"))?,
            "--gexc" => cfg.gexc = PathBuf::from(next(&mut i)?),
            "--workdir" => cfg.workdir = PathBuf::from(next(&mut i)?),
            "--only" => cfg.only = Some(next(&mut i)?),
            "--verbose" => cfg.verbose = true,
            "--bits" => cfg.bits32 = next(&mut i)? == "32",
            other => return Err(format!("unknown argument {other}")),
        }
        i += 1;
    }
    Ok(cfg)
}

// ---------------------------------------------------------------------------

struct Candidate {
    text: String,
    class: &'static str,
    /// Flags this instruction defines (others are undefined and not compared).
    flag_mask: u64,
}

fn reg_name(n: u8, bits: u32) -> String {
    const Q: [&str; 16] = ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"];
    const D: [&str; 16] = ["eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8d", "r9d", "r10d", "r11d", "r12d", "r13d", "r14d", "r15d"];
    const W: [&str; 16] = ["ax", "cx", "dx", "bx", "sp", "bp", "si", "di", "r8w", "r9w", "r10w", "r11w", "r12w", "r13w", "r14w", "r15w"];
    const B: [&str; 16] = ["al", "cl", "dl", "bl", "spl", "bpl", "sil", "dil", "r8b", "r9b", "r10b", "r11b", "r12b", "r13b", "r14b", "r15b"];
    match bits {
        64 => Q[n as usize].into(),
        32 => D[n as usize].into(),
        16 => W[n as usize].into(),
        // 32-bit code has no spl/bpl/sil/dil: those encodings are ah/ch/dh/bh.
        8 if bits32() && (4..8).contains(&n) => ["ah", "ch", "dh", "bh"][n as usize - 4].into(),
        _ => B[n as usize].into(),
    }
}

fn ptr(bits: u32) -> &'static str {
    match bits {
        64 => "QWORD PTR",
        32 => "DWORD PTR",
        16 => "WORD PTR",
        8 => "BYTE PTR",
        _ => "XMMWORD PTR",
    }
}

/// A scratch-memory operand at a random aligned offset.
fn mem(rng: &mut Rng, bits: u32) -> String {
    let bytes = (bits / 8) as usize;
    let off = OUT_SCRATCH + (rng.below((SCRATCH_LEN - 32) as u64) as usize / bytes) * bytes;
    format!("{} [{}+{off}]", ptr(bits), base())
}

fn pick_gpr(rng: &mut Rng) -> u8 {
    let g = gprs();
    g[rng.below(g.len() as u64) as usize]
}

fn pick_bits(rng: &mut Rng) -> u32 {
    if bits32() {
        [8, 16, 32][rng.below(3) as usize]
    } else {
        [8, 16, 32, 64][rng.below(4) as usize]
    }
}

/// 16/32/64-bit widths (no byte form), less 64 in 32-bit mode.
fn pick_bits_wide(rng: &mut Rng) -> u32 {
    if bits32() {
        [16, 32][rng.below(2) as usize]
    } else {
        [16, 32, 64][rng.below(3) as usize]
    }
}

/// 32/64-bit widths, only 32 in 32-bit mode.
fn pick_bits_dq(rng: &mut Rng) -> u32 {
    if bits32() {
        32
    } else {
        [32, 64][rng.below(2) as usize]
    }
}

/// The address width: what `lea` and register-indirect operands use.
fn abits() -> u32 {
    if bits32() { 32 } else { 64 }
}

fn gen(rng: &mut Rng) -> Candidate {
    let bb = base();
    let (si, di) = if bits32() { ("esi", "edi") } else { ("rsi", "rdi") };
    let class_pick = rng.below(100);
    let c = |text: String, class: &'static str, flag_mask: u64| Candidate { text, class, flag_mask };
    match class_pick {
        0..=24 => {
            // ALU: add/sub/adc/sbb/and/or/xor/cmp/test with r/r, r/imm, r/m, m/r
            let ops = ["add", "sub", "adc", "sbb", "and", "or", "xor", "cmp", "test"];
            let op = ops[rng.below(ops.len() as u64) as usize];
            let bits = pick_bits(rng);
            let d = pick_gpr(rng);
            let form = rng.below(4);
            let text = match form {
                0 => format!("{op} {}, {}", reg_name(d, bits), reg_name(pick_gpr(rng), bits)),
                1 => format!("{op} {}, {}", reg_name(d, bits), imm(rng, bits)),
                2 => format!("{op} {}, {}", reg_name(d, bits), mem(rng, bits)),
                _ => format!("{op} {}, {}", mem(rng, bits), reg_name(d, bits)),
            };
            let mask = match op {
                "and" | "or" | "xor" | "test" => ALL & !AF,
                _ => ALL,
            };
            c(text, "alu", mask)
        }
        25..=29 => {
            let ops = ["inc", "dec", "neg", "not"];
            let op = ops[rng.below(4) as usize];
            let bits = pick_bits(rng);
            let text = if rng.below(3) == 0 {
                format!("{op} {}", mem(rng, bits))
            } else {
                format!("{op} {}", reg_name(pick_gpr(rng), bits))
            };
            let mask = match op {
                "inc" | "dec" => ALL & !CF,
                "neg" => ALL,
                _ => NONE,
            };
            c(text, "unary", mask)
        }
        30..=39 => {
            // Shifts and rotates by immediate or cl.
            let ops = ["shl", "shr", "sar", "rol", "ror", "rcl", "rcr"];
            let op = ops[rng.below(ops.len() as u64) as usize];
            let bits = pick_bits(rng);
            let d = pick_gpr(rng);
            let by_cl = rng.below(3) == 0 && matches!(op, "shl" | "shr" | "sar" | "rol" | "ror");
            let count = if by_cl { "cl".to_string() } else { format!("{}", rng.below(if bits == 64 { 66 } else { 34 })) };
            let text = format!("{op} {}, {count}", reg_name(d, bits));
            // Shifts by a variable/large count leave OF (and AF) undefined;
            // rotates define only CF and OF (OF only for count 1).
            let mask = match op {
                "shl" | "shr" | "sar" => (CF | ZF | SF | PF) | if count == "1" { OF } else { 0 },
                "rol" | "ror" | "rcl" | "rcr" => CF | if count == "1" { OF } else { 0 },
                _ => NONE,
            };
            // A zero count leaves everything unchanged, including flags from
            // earlier instructions: compare nothing then.
            let mask = if count == "0" { NONE } else { mask };
            c(text, "shift", mask)
        }
        40..=44 => {
            let bits = pick_bits(rng);
            let d = pick_gpr(rng);
            let s = pick_gpr(rng);
            let text = match rng.below(5) {
                0 => format!("mul {}", reg_name(s, bits)),
                1 => format!("imul {}", reg_name(s, bits)),
                2 if bits >= 16 => format!("imul {}, {}", reg_name(d, bits), reg_name(s, bits)),
                3 if bits >= 16 => format!("imul {}, {}, {}", reg_name(d, bits), reg_name(s, bits), imm(rng, bits.min(32))),
                _ => format!("mul {}", mem(rng, bits)),
            };
            c(text, "mul", CF | OF)
        }
        45..=48 => {
            // Division with a divisor forced non-zero and a small dividend high
            // half, so no #DE.
            let bits = pick_bits(rng);
            // Not rax or rdx: the setup below clobbers them.
            let s = loop {
                let s = pick_gpr(rng);
                if s != 0 && s != 2 {
                    break s;
                }
            };
            let signed = rng.below(2) == 0;
            let op = if signed { "idiv" } else { "div" };
            let sname = reg_name(s, bits);
            let setup = match bits {
                8 => format!("or {sname}, 1\n    movzx eax, al\n    "),
                16 => format!("or {sname}, 1\n    xor edx, edx\n    "),
                32 => format!("or {sname}, 1\n    xor edx, edx\n    "),
                _ => format!("or {sname}, 1\n    xor edx, edx\n    "),
            };
            // A signed division of a positive dividend by any non-zero
            // divisor cannot overflow at these widths.
            let text = format!("{setup}{op} {sname}");
            c(text, "div", NONE)
        }
        49..=55 => {
            // Moves and extensions.
            let bits = pick_bits(rng);
            let d = pick_gpr(rng);
            let s = pick_gpr(rng);
            let text = match rng.below(8) {
                0 => format!("mov {}, {}", reg_name(d, bits), reg_name(s, bits)),
                1 => format!("mov {}, {}", reg_name(d, bits), imm(rng, bits)),
                2 => format!("mov {}, {}", reg_name(d, bits), mem(rng, bits)),
                3 => format!("mov {}, {}", mem(rng, bits), reg_name(s, bits)),
                4 if bits >= 16 => format!("movzx {}, {}", reg_name(d, bits), reg_name(s, if bits > 16 && rng.below(2) == 0 { 16 } else { 8 })),
                5 if bits >= 16 => format!("movsx {}, {}", reg_name(d, bits), reg_name(s, if bits > 16 && rng.below(2) == 0 { 16 } else { 8 })),
                6 if !bits32() => format!("movsxd {}, {}", reg_name(d, 64), reg_name(s, 32)),
                6 => format!("movzx {}, {}", reg_name(d, 32), reg_name(s, 8)),
                _ => format!("lea {}, [{}+{}*{}+{}]", reg_name(d, if bits < 32 { 32 } else { bits }), reg_name(s, abits()), reg_name(d, abits()), [1, 2, 4, 8][rng.below(4) as usize], (rng.below(0x200) as i64) - 0x100),
            };
            c(text, "mov", NONE)
        }
        56..=60 => {
            // cmov / setcc after a cmp, so the flags are known.
            let conds = ["o", "no", "b", "ae", "e", "ne", "be", "a", "s", "ns", "p", "np", "l", "ge", "le", "g"];
            let cc = conds[rng.below(16) as usize];
            let bits = pick_bits_wide(rng);
            let d = pick_gpr(rng);
            let a = pick_gpr(rng);
            let b = pick_gpr(rng);
            let text = if rng.below(2) == 0 {
                format!("cmp {}, {}\n    cmov{cc} {}, {}", reg_name(a, bits), reg_name(b, bits), reg_name(d, bits), reg_name(pick_gpr(rng), bits))
            } else {
                format!("cmp {}, {}\n    set{cc} {}", reg_name(a, bits), reg_name(b, bits), reg_name(d, 8))
            };
            c(text, "cond", ALL)
        }
        61..=66 => {
            // Bit operations.
            let bits = pick_bits_wide(rng);
            let d = pick_gpr(rng);
            let s = pick_gpr(rng);
            let (text, mask) = match rng.below(9) {
                0 => (format!("bt {}, {}", reg_name(d, bits), reg_name(s, bits)), CF),
                1 => (format!("bts {}, {}", reg_name(d, bits), rng.below(bits as u64)), CF),
                2 => (format!("btr {}, {}", reg_name(d, bits), reg_name(s, bits)), CF),
                3 => (format!("btc {}, {}", reg_name(d, bits), rng.below(bits as u64)), CF),
                4 => (format!("or {}, 1\n    bsf {}, {}", reg_name(s, bits), reg_name(d, bits), reg_name(s, bits)), ZF),
                5 => (format!("or {}, 1\n    bsr {}, {}", reg_name(s, bits), reg_name(d, bits), reg_name(s, bits)), ZF),
                6 => (format!("popcnt {}, {}", reg_name(d, bits), reg_name(s, bits)), ALL),
                7 => (format!("bswap {}", reg_name(d, if bits == 16 { 32 } else { bits })), NONE),
                _ => (format!("bt {}, {}", mem(rng, bits), rng.below(bits as u64)), CF),
            };
            c(text, "bits", mask)
        }
        67..=70 => {
            // Double shifts, xchg, xadd, cmpxchg, cbw family.
            let bits = pick_bits_wide(rng);
            let d = pick_gpr(rng);
            let s = pick_gpr(rng);
            let (text, mask) = match rng.below(8) {
                0 => (format!("shld {}, {}, {}", reg_name(d, bits), reg_name(s, bits), 1 + rng.below(bits as u64 - 1)), CF | ZF | SF | PF),
                1 => (format!("shrd {}, {}, {}", reg_name(d, bits), reg_name(s, bits), 1 + rng.below(bits as u64 - 1)), CF | ZF | SF | PF),
                2 => (format!("xchg {}, {}", reg_name(d, bits), reg_name(s, bits)), NONE),
                3 => (format!("xchg {}, {}", mem(rng, bits), reg_name(s, bits)), NONE),
                4 => (format!("xadd {}, {}", mem(rng, bits), reg_name(s, bits)), ALL),
                5 => (format!("cmpxchg {}, {}", mem(rng, bits), reg_name(s, bits)), ALL),
                6 if bits32() => (["cbw", "cwde", "cwd", "cdq"][rng.below(4) as usize].to_string(), NONE),
                6 => (["cbw", "cwde", "cdqe", "cwd", "cdq", "cqo"][rng.below(6) as usize].to_string(), NONE),
                // A count past the operand width is undefined on hardware:
                // keep it in range for the 16-bit form.
                _ => (format!("and cl, {}\n    shld {}, {}, cl", bits - 1, reg_name(d, bits), reg_name(s, bits)), NONE),
            };
            c(text, "misc", mask)
        }
        71..=74 => {
            // String ops over the scratch buffer, small counts.
            let bits = pick_bits(rng);
            let bytes = bits / 8;
            let n = rng.below(20);
            let src = OUT_SCRATCH + rng.below(64) as usize * 4;
            let dst = OUT_SCRATCH + 256 + rng.below(40) as usize * 4;
            let sfx = match bits {
                8 => "b",
                16 => "w",
                32 => "d",
                _ => "q",
            };
            let text = match rng.below(5) {
                0 => format!("lea {si}, [{bb}+{src}]\n    lea {di}, [{bb}+{dst}]\n    mov ecx, {n}\n    cld\n    rep movs{sfx}"),
                1 => format!("lea {di}, [{bb}+{dst}]\n    mov ecx, {n}\n    cld\n    rep stos{sfx}"),
                2 => format!("lea {si}, [{bb}+{src}]\n    lea {di}, [{bb}+{dst}]\n    mov ecx, {n}\n    cld\n    repe cmps{sfx}"),
                3 => format!("lea {di}, [{bb}+{dst}]\n    mov ecx, {n}\n    cld\n    repne scas{sfx}"),
                _ => format!("lea {si}, [{bb}+{src}]\n    lea {di}, [{bb}+{dst}]\n    mov ecx, {n}\n    std\n    rep movs{sfx}\n    cld"),
            };
            let _ = bytes;
            c(text, "string", NONE)
        }
        75..=92 => gen_sse(rng),
        _ => gen_x87(rng),
    }
}

fn gen_sse(rng: &mut Rng) -> Candidate {
    let bb = base();
    let x = |rng: &mut Rng| format!("xmm{}", rng.below(if bits32() { 8 } else { 16 }));
    let d = x(rng);
    let s = x(rng);
    let m128 = |rng: &mut Rng| {
        let off = OUT_SCRATCH + rng.below(30) as usize * 16;
        format!("XMMWORD PTR [{bb}+{off}]")
    };
    let m64 = |rng: &mut Rng| {
        let off = OUT_SCRATCH + rng.below(60) as usize * 8;
        format!("QWORD PTR [{bb}+{off}]")
    };
    let m32 = |rng: &mut Rng| {
        let off = OUT_SCRATCH + rng.below(120) as usize * 4;
        format!("DWORD PTR [{bb}+{off}]")
    };
    let g = pick_gpr(rng);
    let text = match rng.below(40) {
        0 => format!("paddb {d}, {s}"),
        1 => format!("paddw {d}, {}", m128(rng)),
        2 => format!("paddd {d}, {s}"),
        3 => format!("paddq {d}, {s}"),
        4 => format!("psubb {d}, {s}"),
        5 => format!("psubw {d}, {s}"),
        6 => format!("paddsb {d}, {s}"),
        7 => format!("paddusw {d}, {s}"),
        8 => format!("psubsw {d}, {s}"),
        9 => format!("pcmpeqb {d}, {s}"),
        10 => format!("pcmpgtw {d}, {s}"),
        11 => format!("pand {d}, {s}"),
        12 => format!("pandn {d}, {s}"),
        13 => format!("por {d}, {s}"),
        14 => format!("pxor {d}, {}", m128(rng)),
        15 => format!("punpcklbw {d}, {s}"),
        16 => format!("punpckhdq {d}, {s}"),
        17 => format!("punpcklqdq {d}, {s}"),
        18 => format!("pshufd {d}, {s}, {}", rng.below(256)),
        19 => format!("pshuflw {d}, {s}, {}", rng.below(256)),
        20 => format!("psllw {d}, {}", rng.below(20)),
        21 => format!("psrld {d}, {}", rng.below(40)),
        22 => format!("psraw {d}, {}", rng.below(20)),
        23 => format!("psrldq {d}, {}", rng.below(20)),
        24 => format!("pslldq {d}, {}", rng.below(20)),
        25 => format!("psllq {d}, {s}"),
        26 => format!("packsswb {d}, {s}"),
        27 => format!("packuswb {d}, {s}"),
        28 => format!("pmullw {d}, {s}"),
        29 => format!("pmulhw {d}, {s}"),
        30 => format!("pmulhuw {d}, {s}"),
        31 => format!("pmaddwd {d}, {s}"),
        32 => format!("pmuludq {d}, {s}"),
        33 => format!("psadbw {d}, {s}"),
        34 => format!("pavgb {d}, {s}"),
        35 => format!("pmaxub {d}, {s}"),
        36 => format!("pminsw {d}, {s}"),
        37 => format!("pmovmskb {}, {s}", reg_name(g, 32)),
        38 => format!("movd {d}, {}", reg_name(g, 32)),
        _ => format!("movq {}, {s}", reg_name(g, 64)),
    };
    // Float forms, chosen by another draw so the integer ones stay frequent.
    let text = if rng.below(3) == 0 {
        match rng.below(30) {
            0 => format!("addps {d}, {s}"),
            1 => format!("subpd {d}, {s}"),
            2 => format!("mulss {d}, {s}"),
            3 => format!("divsd {d}, {}", m64(rng)),
            4 => format!("minps {d}, {s}"),
            5 => format!("maxsd {d}, {s}"),
            6 => format!("sqrtps {d}, {s}"),
            7 => format!("andps {d}, {s}"),
            8 => format!("andnpd {d}, {s}"),
            9 => format!("xorps {d}, {s}"),
            10 => format!("cmpps {d}, {s}, {}", rng.below(8)),
            11 => format!("cmpsd {d}, {s}, {}", rng.below(8)),
            12 => format!("cvtsi2sd {d}, {}", reg_name(g, 64)),
            13 => format!("cvtsi2ss {d}, {}", reg_name(g, 32)),
            14 => format!("cvttsd2si {}, {d}", reg_name(g, 64)),
            15 => format!("cvtsd2ss {d}, {s}"),
            16 => format!("cvtss2sd {d}, {}", m32(rng)),
            17 => format!("cvtps2pd {d}, {s}"),
            18 => format!("cvtpd2ps {d}, {s}"),
            19 => format!("cvtdq2ps {d}, {s}"),
            20 => format!("cvttps2dq {d}, {s}"),
            21 => format!("shufps {d}, {s}, {}", rng.below(256)),
            22 => format!("unpcklps {d}, {s}"),
            23 => format!("unpckhpd {d}, {s}"),
            24 => format!("movss {d}, {s}"),
            25 => format!("movsd {d}, {}", m64(rng)),
            26 => format!("movhlps {d}, {s}"),
            27 => format!("movlhps {d}, {s}"),
            28 => format!("movmskpd {}, {s}", reg_name(g, 32)),
            _ => format!("cvtdq2pd {d}, {s}"),
        }
    } else {
        text
    };
    let mask = if text.starts_with("comis") || text.starts_with("ucomis") { ZF | PF | CF } else { NONE };
    Candidate { text, class: "sse", flag_mask: mask }
}

fn gen_x87(rng: &mut Rng) -> Candidate {
    let bb = base();
    // Values come from the scratch buffer as doubles; results go back there.
    let a = OUT_SCRATCH + rng.below(20) as usize * 8;
    let b = OUT_SCRATCH + 160 + rng.below(20) as usize * 8;
    let out = OUT_SCRATCH + 352 + rng.below(16) as usize * 8;
    let text = match rng.below(10) {
        0 => format!("fld QWORD PTR [{bb}+{a}]\n    fld QWORD PTR [{bb}+{b}]\n    faddp st(1), st\n    fstp QWORD PTR [{bb}+{out}]"),
        1 => format!("fld QWORD PTR [{bb}+{a}]\n    fld QWORD PTR [{bb}+{b}]\n    fsubrp st(1), st\n    fstp QWORD PTR [{bb}+{out}]"),
        2 => format!("fld QWORD PTR [{bb}+{a}]\n    fld QWORD PTR [{bb}+{b}]\n    fmulp st(1), st\n    fstp QWORD PTR [{bb}+{out}]"),
        3 => format!("fld QWORD PTR [{bb}+{a}]\n    fld QWORD PTR [{bb}+{b}]\n    fdivp st(1), st\n    fstp QWORD PTR [{bb}+{out}]"),
        4 => format!("fld QWORD PTR [{bb}+{a}]\n    fchs\n    fabs\n    fstp QWORD PTR [{bb}+{out}]"),
        5 => format!("fld DWORD PTR [{bb}+{a}]\n    fstp QWORD PTR [{bb}+{out}]"),
        6 => format!("fild QWORD PTR [{bb}+{a}]\n    fstp QWORD PTR [{bb}+{out}]"),
        7 => format!("fld QWORD PTR [{bb}+{a}]\n    fld QWORD PTR [{bb}+{b}]\n    fxch st(1)\n    fstp QWORD PTR [{bb}+{out}]\n    fstp QWORD PTR [{bb}+{out}]"),
        8 => format!("fld QWORD PTR [{bb}+{a}]\n    fld QWORD PTR [{bb}+{b}]\n    fucomip st, st(1)\n    fstp st(0)\n    setb al\n    sete cl\n    setp dl"),
        _ => format!("fld1\n    fldz\n    faddp st(1), st\n    fstp QWORD PTR [{bb}+{out}]"),
    };
    Candidate { text, class: "x87", flag_mask: NONE }
}

fn imm(rng: &mut Rng, bits: u32) -> String {
    let v: i64 = match rng.below(4) {
        0 => (rng.below(256) as i64) - 128,
        1 => rng.next() as i64,
        2 => 1i64 << rng.below(bits as u64),
        _ => -1,
    };
    // Immediates are at most 32 bits (sign-extended); wider only via movabs,
    // which the generator does not use.
    let v = match bits {
        8 => v as i8 as i64,
        16 => v as i16 as i64,
        _ => v as i32 as i64,
    };
    format!("{v}")
}

// ---------------------------------------------------------------------------

/// The harness around the candidates: seed everything from the blob, run,
/// dump everything, write it out, exit.
fn harness(insts: &[Candidate], blob: &[u8], rng: &mut Rng) -> String {
    let mut s = String::new();
    let bb = base();
    let (w, wp, nx) = if bits32() { (32, "DWORD PTR", 8) } else { (64, "QWORD PTR", 16) };
    s.push_str(".intel_syntax noprefix\n.globl _start\n.text\n_start:\n");
    if bits32() {
        s.push_str(&format!("    lea {bb}, [blob]\n"));
    } else {
        s.push_str(&format!("    lea {bb}, [rip+blob]\n"));
    }
    // xmm from the blob.
    for i in 0..nx {
        s.push_str(&format!("    movdqu xmm{i}, XMMWORD PTR [{bb}+{}]\n", OUT_XMM + i * 16));
    }
    // Flags: only the six arithmetic flags, DF clear, IF set.
    let fl = (rng.next() & ALL) | 0x202;
    if bits32() {
        s.push_str(&format!("    mov eax, {fl}\n    push eax\n    popfd\n"));
    } else {
        s.push_str(&format!("    mov rax, {fl}\n    push rax\n    popfq\n"));
    }
    // GPRs from the blob (the stack pointer and the base register excluded).
    for &n in gprs().iter() {
        s.push_str(&format!("    mov {}, {wp} [{bb}+{}]\n", reg_name(n, w), OUT_GPR + n as usize * 8));
    }
    // The candidates.
    for c in insts {
        for line in c.text.lines() {
            s.push_str("    ");
            s.push_str(line.trim());
            s.push('\n');
        }
    }
    // Dump: flags first (before anything disturbs them), then registers.
    if bits32() {
        s.push_str(&format!("    pushfd\n    pop DWORD PTR [{bb}+{OUT_FLAGS}]\n"));
    } else {
        s.push_str(&format!("    pushfq\n    pop QWORD PTR [{bb}+{OUT_FLAGS}]\n"));
    }
    for &n in gprs().iter() {
        s.push_str(&format!("    mov {wp} [{bb}+{}], {}\n", OUT_GPR + n as usize * 8, reg_name(n, w)));
    }
    for i in 0..nx {
        s.push_str(&format!("    movdqu XMMWORD PTR [{bb}+{}], xmm{i}\n", OUT_XMM + i * 16));
    }
    // write(1, blob, OUT_LEN); exit(0)
    if bits32() {
        s.push_str(&format!(
            "    mov eax, 4\n    mov ebx, 1\n    lea ecx, [blob]\n    mov edx, {OUT_LEN}\n    int 0x80\n    mov eax, 1\n    xor ebx, ebx\n    int 0x80\n"
        ));
    } else {
    s.push_str(&format!(
        "    mov eax, 1\n    mov edi, 1\n    lea rsi, [rip+blob]\n    mov edx, {OUT_LEN}\n    syscall\n    mov eax, 60\n    xor edi, edi\n    syscall\n"
    ));
    }
    s.push_str(".data\n.align 16\nblob:\n");
    for chunk in blob.chunks(16) {
        s.push_str("    .byte ");
        s.push_str(&chunk.iter().map(|b| format!("{b}")).collect::<Vec<_>>().join(","));
        s.push('\n');
    }
    s
}

fn assemble(src: &Path, bin: &Path) -> bool {
    let obj = src.with_extension("o");
    let mut asm = Command::new("as");
    if bits32() {
        asm.arg("--32");
    }
    let a = asm.arg("-o").arg(&obj).arg(src).output().expect("as");
    if !a.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&a.stderr));
        return false;
    }
    let mut ld = Command::new("ld");
    if bits32() {
        ld.arg("-m").arg("elf_i386");
    }
    let l = ld.arg("-static").arg("-o").arg(bin).arg(&obj).output().expect("ld");
    if !l.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&l.stderr));
        return false;
    }
    true
}

fn run(bin: &Path, args: &[String]) -> Vec<u8> {
    match Command::new(bin).args(args).output() {
        Ok(o) => {
            if !o.status.success() {
                let mut v = b"EXIT:".to_vec();
                v.extend_from_slice(format!("{:?} {}", o.status, String::from_utf8_lossy(&o.stderr)).as_bytes());
                return v;
            }
            o.stdout
        }
        Err(e) => format!("SPAWN:{e}").into_bytes(),
    }
}

fn compare(native: &[u8], gex: &[u8], flag_mask: u64) -> Option<String> {
    if native.len() != OUT_LEN {
        return Some(format!("native produced {} bytes: {}", native.len(), String::from_utf8_lossy(&native[..native.len().min(200)])));
    }
    if gex.len() != OUT_LEN {
        return Some(format!("gex produced {} bytes: {}", gex.len(), String::from_utf8_lossy(&gex[..gex.len().min(300)])));
    }
    let mut diffs = Vec::new();
    for &n in GPRS.iter() {
        let o = OUT_GPR + n as usize * 8;
        let a = u64::from_le_bytes(native[o..o + 8].try_into().unwrap());
        let b = u64::from_le_bytes(gex[o..o + 8].try_into().unwrap());
        if a != b {
            diffs.push(format!("{}: native {a:#x} gex {b:#x}", reg_name(n, 64)));
        }
    }
    let fa = u64::from_le_bytes(native[OUT_FLAGS..OUT_FLAGS + 8].try_into().unwrap()) & flag_mask;
    let fb = u64::from_le_bytes(gex[OUT_FLAGS..OUT_FLAGS + 8].try_into().unwrap()) & flag_mask;
    if fa != fb {
        diffs.push(format!("flags: native {fa:#x} gex {fb:#x} (mask {flag_mask:#x})"));
    }
    for i in 0..16 {
        let o = OUT_XMM + i * 16;
        if native[o..o + 16] != gex[o..o + 16] {
            diffs.push(format!(
                "xmm{i}: native {} gex {}",
                hex(&native[o..o + 16]),
                hex(&gex[o..o + 16])
            ));
        }
    }
    if native[OUT_SCRATCH..] != gex[OUT_SCRATCH..] {
        let first = (0..SCRATCH_LEN).find(|&k| native[OUT_SCRATCH + k] != gex[OUT_SCRATCH + k]).unwrap();
        diffs.push(format!(
            "scratch differs from byte {first}: native {} gex {}",
            hex(&native[OUT_SCRATCH + first..(OUT_SCRATCH + first + 16).min(OUT_LEN)]),
            hex(&gex[OUT_SCRATCH + first..(OUT_SCRATCH + first + 16).min(OUT_LEN)])
        ));
    }
    if diffs.is_empty() {
        None
    } else {
        Some(diffs.join("; "))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[allow(dead_code)]
fn unused(_: &mut dyn Write) {}
