//! Instruction-level differential fuzzing of gex's PowerPC lifter against
//! `qemu-ppc` (docs/ppc.md §11), in the manner of `tests/diff-fuzz`.
//!
//! Random words the decoder accepts are filtered to one of five classes that
//! can run in isolation, and each becomes a *case*: the harness loads r0–r30,
//! CR, XER, LR, CTR and f0–f31 from a record, runs the candidate, and stores
//! them back into the record. Many cases share one binary (a process per case
//! would spend the run spawning), which ends by writing every record to
//! stdout. The binary runs under `qemu-ppc` and under `gexc run`, and the
//! records are compared field by field.
//!
//! - `int`: integer arithmetic, logical, rotate, shift, compare, XER and
//!   LR/CTR moves, including the record and overflow forms;
//! - `cr`: the CR logical instructions, `mcrf`, `mfcr`, `mtcrf`, `mcrxr`;
//! - `fp`: FP arithmetic, conversions, moves and compares (single-precision
//!   operations get single-representable operands, as the ISA requires);
//! - `mem`: loads and stores of every width and form, `lmw`/`stmw`-shaped,
//!   `lwarx`/`stwcx.`, `dcbz`, against a scratch area in the record;
//! - `branch`: `b`, `bc`, `bclr`, `bcctr` with and without link, into a
//!   ladder of `addi r30, r30, 2^k` whose sum says where control went.
//!
//! A NaN result compares equal to any NaN: payloads are PLAN §7's documented
//! gap. The FPSCR is not compared (docs/ppc.md §8.1).

use gex_isa_ppc::{decode, fmt::disasm, Inst, Op};
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

// Record layout, bytes. Inputs then outputs, then the scratch area memory
// accesses use (32-byte aligned, so `dcbz` stays inside it).
const IN_R: usize = 0;
const IN_CR: usize = 128;
const IN_XER: usize = 132;
const IN_LR: usize = 136;
const IN_CTR: usize = 140;
const IN_F: usize = 144;
const OUT_R: usize = 400;
const OUT_CR: usize = 524;
const OUT_XER: usize = 528;
const OUT_LR: usize = 532;
const OUT_CTR: usize = 536;
const OUT_F: usize = 544;
const SCRATCH: usize = 800;
const SCRATCH_LEN: usize = 64;
const REC: usize = 864;
/// r29 points this far into the scratch area, so displacements of both
/// signs stay inside it.
const MEM_BIAS: i32 = 32;
/// The ladder of the branch class: slot `k` adds `1 << k` to r30.
const LADDER: u32 = 8;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Class {
    Int,
    Cr,
    Fp,
    Mem,
    Branch,
}

impl Class {
    fn name(self) -> &'static str {
        match self {
            Class::Int => "int",
            Class::Cr => "cr",
            Class::Fp => "fp",
            Class::Mem => "mem",
            Class::Branch => "branch",
        }
    }
}

struct Config {
    iters: usize,
    per_bin: usize,
    seed: u64,
    classes: Vec<Class>,
    qemu: PathBuf,
    gexc: PathBuf,
    clang: PathBuf,
    workdir: PathBuf,
    verbose: bool,
}

/// One test: the candidate words, its class, and the input record.
struct Case {
    class: Class,
    words: Vec<u32>,
    /// For `bclr`/`bcctr`: the ladder slot LR or CTR is aimed at.
    reg_target: Option<u32>,
    record: Vec<u8>,
}

fn main() {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gex-difffuzz-ppc: {e}");
            eprintln!(
                "\nusage: gex-difffuzz-ppc [--iters N] [--per-bin N] [--seed S] [--class int,cr,fp,mem,branch]
                        [--gexc PATH] [--qemu PATH] [--clang PATH] [--workdir DIR] [--verbose]"
            );
            std::process::exit(2);
        }
    };
    std::fs::create_dir_all(&cfg.workdir).expect("creating the work directory");
    let mut rng = Rng::new(cfg.seed);
    let (mut tested, mut failures) = (0usize, 0usize);
    let mut seen: std::collections::BTreeMap<String, usize> = Default::default();
    let mut bin = 0usize;
    while tested + failures < cfg.iters && failures < 20 {
        let n = cfg.per_bin.min(cfg.iters - tested - failures);
        let cases: Vec<Case> = (0..n)
            .map(|_| {
                let class = cfg.classes[(rng.next_u64() % cfg.classes.len() as u64) as usize];
                make_case(&mut rng, class)
            })
            .collect();
        for c in &cases {
            let op = decode(c.words[0], 0).op;
            *seen.entry(format!("{}:{op:?}", c.class.name())).or_default() += 1;
        }
        match run_bin(&cfg, bin, &cases) {
            Ok((q, g)) => {
                for (i, c) in cases.iter().enumerate() {
                    let (qr, gr) = (record(&q, i), record(&g, i));
                    match (qr, gr) {
                        (Some(qr), Some(gr)) if same(qr, gr) => tested += 1,
                        _ => {
                            failures += 1;
                            report(c, qr, gr);
                        }
                    }
                }
            }
            Err(e) => eprintln!("gex-difffuzz-ppc: batch {bin} could not be run: {e}"),
        }
        bin += 1;
    }
    if cfg.verbose {
        eprintln!("\ninstructions exercised:");
        let mut v: Vec<_> = seen.iter().collect();
        v.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        for (name, n) in v {
            eprintln!("  {n:>6}  {name}");
        }
    }
    println!("{tested} cases matched, {failures} differed");
    if failures > 0 {
        std::process::exit(1);
    }
}

fn record(out: &[u8], i: usize) -> Option<&[u8]> {
    out.get(i * REC..(i + 1) * REC)
}

fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes(b[off..off + 4].try_into().unwrap())
}

fn be64(b: &[u8], off: usize) -> u64 {
    u64::from_be_bytes(b[off..off + 8].try_into().unwrap())
}

fn is_nan(v: u64) -> bool {
    f64::from_bits(v).is_nan()
}

/// Two output records agree: every register, the scratch memory, and each
/// FPR bit for bit or both NaN.
fn same(q: &[u8], g: &[u8]) -> bool {
    if q[OUT_R..OUT_F] != g[OUT_R..OUT_F] || q[SCRATCH..REC] != g[SCRATCH..REC] {
        return false;
    }
    (0..32).all(|i| {
        let (a, b) = (be64(q, OUT_F + i * 8), be64(g, OUT_F + i * 8));
        a == b || (is_nan(a) && is_nan(b))
    })
}

// -- case generation ------------------------------------------------------------

fn make_case(rng: &mut Rng, class: Class) -> Case {
    let (words, reg_target) = loop {
        match class {
            Class::Branch => break branch_candidate(rng),
            _ => {
                let raw = rng.next_u32();
                if let Some(w) = accept(raw, class, rng) {
                    break (vec![w], None);
                }
            }
        }
    };
    let single = matches!(
        decode(words[0], 0).op,
        Op::Fadds | Op::Fsubs | Op::Fmuls | Op::Fdivs | Op::Fsqrts | Op::Fmadds | Op::Fmsubs
            | Op::Fnmadds | Op::Fnmsubs | Op::Stfs | Op::Stfsu | Op::Stfsx | Op::Stfsux
    );
    let mut rec = vec![0u8; REC];
    for r in 0..31 {
        rec[IN_R + r * 4..IN_R + r * 4 + 4].copy_from_slice(&rng.seed32().to_be_bytes());
    }
    let cr = rng.next_u32();
    // XER: SO, OV, CA and a byte count; the rest of the register reads 0.
    let xer = rng.next_u32() & 0xe000_007f;
    let lr = rng.next_u32() & !3;
    // Small counts, so `bdnz` and friends reach zero.
    let ctr = if rng.next_u64() % 2 == 0 { (rng.next_u64() % 3) as u32 } else { rng.next_u32() };
    rec[IN_CR..IN_CR + 4].copy_from_slice(&cr.to_be_bytes());
    rec[IN_XER..IN_XER + 4].copy_from_slice(&xer.to_be_bytes());
    rec[IN_LR..IN_LR + 4].copy_from_slice(&lr.to_be_bytes());
    rec[IN_CTR..IN_CTR + 4].copy_from_slice(&ctr.to_be_bytes());
    for f in 0..32 {
        let v = rng.seed_f64(single);
        rec[IN_F + f * 8..IN_F + f * 8 + 8].copy_from_slice(&v.to_be_bytes());
    }
    for b in rec[SCRATCH..REC].iter_mut() {
        *b = rng.next_u64() as u8;
    }
    Case {
        class,
        words,
        reg_target,
        record: rec,
    }
}

/// Place `v` in bits `first..=last` (Power ISA numbering) of `raw`.
fn set_bits(raw: u32, first: u32, last: u32, v: u32) -> u32 {
    let width = last - first + 1;
    let shift = 31 - last;
    let mask = ((1u32 << width) - 1) << shift;
    (raw & !mask) | ((v << shift) & mask)
}

/// Whether a random word is usable in `class`, rewritten where the class
/// needs it (memory operands aimed at the scratch area).
fn accept(raw: u32, class: Class, rng: &mut Rng) -> Option<u32> {
    use Op::*;
    let i = decode(raw, 0);
    if i.op == Unknown {
        return None;
    }
    // r31 holds the record pointer; nothing may write it, and reading it
    // would see an address, which is the same under both but not a seed.
    if i.rt() == 31 || i.ra() == 31 || i.rb() == 31 {
        return None;
    }
    let ok = match class {
        Class::Int => matches!(
            i.op,
            Addi | Addis | Addic | AddicRc | Subfic | Mulli | Add | Addc | Adde | Addme | Addze
                | Subf | Subfc | Subfe | Subfme | Subfze | Neg | Mullw | Mulhw | Mulhwu | Divw
                | Divwu | Cmp | Cmpl | Cmpi | Cmpli | AndiRc | AndisRc | Ori | Oris | Xori
                | Xoris | And | Andc | Or | Orc | Xor | Nand | Nor | Eqv | Extsb | Extsh
                | Cntlzw | Rlwinm | Rlwnm | Rlwimi | Slw | Srw | Sraw | Srawi
        ) || (matches!(i.op, Mfspr | Mtspr) && matches!(i.spr(), 1 | 8 | 9)),
        Class::Cr => matches!(
            i.op,
            Mcrf | Crand | Crandc | Creqv | Crnand | Crnor | Cror | Crorc | Crxor | Mfcr | Mtcrf
                | Mcrxr
        ),
        Class::Fp => {
            matches!(
                i.op,
                Fadd | Fsub | Fmul | Fdiv | Fsqrt | Fsel | Fmadd | Fmsub | Fnmadd | Fnmsub
                    | Fadds | Fsubs | Fmuls | Fdivs | Fsqrts | Fmadds | Fmsubs | Fnmadds
                    | Fnmsubs | Fmr | Fneg | Fabs | Fnabs | Frsp | Fctiw | Fctiwz | Fcmpu
                    | Fcmpo
            ) && !i.rc()
        }
        Class::Mem => return mem_candidate(raw, &i, rng),
        Class::Branch => false,
    };
    ok.then_some(raw)
}

/// A load or store, aimed at the scratch area: rA = r29 (the scratch area
/// plus `MEM_BIAS`), rB = r28 (a small offset the harness sets), and a
/// displacement that keeps the access inside the area.
fn mem_candidate(raw: u32, i: &Inst, rng: &mut Rng) -> Option<u32> {
    use Op::*;
    let (size, indexed, update) = match i.op {
        Lbz | Stb => (1, false, false),
        Lbzu | Stbu => (1, false, true),
        Lhz | Lha | Sth => (2, false, false),
        Lhzu | Lhau | Sthu => (2, false, true),
        Lwz | Stw | Lfs | Stfs => (4, false, false),
        Lwzu | Stwu | Lfsu | Stfsu => (4, false, true),
        Lfd | Stfd => (8, false, false),
        Lfdu | Stfdu => (8, false, true),
        Lbzx | Stbx => (1, true, false),
        Lbzux | Stbux => (1, true, true),
        Lhzx | Lhax | Sthx | Lhbrx | Sthbrx => (2, true, false),
        Lhzux | Lhaux | Sthux => (2, true, true),
        Lwzx | Stwx | Lwbrx | Stwbrx | Lfsx | Stfsx | Stfiwx | Lwarx | Stwcx => (4, true, false),
        Lwzux | Stwux | Lfsux | Stfsux => (4, true, true),
        Lfdx | Stfdx => (8, true, false),
        Lfdux | Stfdux => (8, true, true),
        Dcbz => (1, true, false),
        Stmw => (4, false, false),
        _ => return None,
    };
    // The base and index registers are the harness's.
    if matches!(i.rt(), 28 | 29) && !is_store(i.op) {
        return None;
    }
    if update && i.rt() == 29 && !is_store(i.op) {
        return None;
    }
    let mut w = set_bits(raw, 11, 15, 29);
    if i.op == Stmw {
        // Stores rT..r31: the words must fit the area.
        let n = 32 - i.rt() as i32;
        if n * 4 > SCRATCH_LEN as i32 {
            return None;
        }
        let lo = -MEM_BIAS;
        let hi = SCRATCH_LEN as i32 - MEM_BIAS - n * 4;
        let d = lo + (rng.next_u64() % ((hi - lo) as u64 / 4 + 1)) as i32 * 4;
        return Some(set_bits(w, 16, 31, d as u16 as u32));
    }
    if indexed {
        w = set_bits(w, 16, 20, 28);
    } else {
        let align = if matches!(i.op, Lwz | Lwzu | Stw | Stwu | Lfd | Lfdu | Stfd | Stfdu) { size } else { 1 };
        let span = SCRATCH_LEN as i32 - size;
        let d = (rng.next_u64() % (span as u64 / align as u64 + 1)) as i32 * align - MEM_BIAS;
        w = set_bits(w, 16, 31, d as u16 as u32);
    }
    Some(w)
}

fn is_store(op: Op) -> bool {
    use Op::*;
    matches!(
        op,
        Stb | Stbu | Stbx | Stbux | Sth | Sthu | Sthx | Sthux | Stw | Stwu | Stwx | Stwux
            | Sthbrx | Stwbrx | Stfs | Stfsu | Stfsx | Stfsux | Stfd | Stfdu | Stfdx | Stfdux
            | Stfiwx | Stwcx | Stmw | Dcbz
    )
}

/// A branch into the ladder: `b`/`bc` with a displacement of 1..=LADDER
/// words, or `bclr`/`bcctr` with LR/CTR aimed at a slot.
fn branch_candidate(rng: &mut Rng) -> (Vec<u32>, Option<u32>) {
    let slot = 1 + (rng.next_u64() % LADDER as u64) as u32;
    let lk = (rng.next_u64() % 2) as u32;
    let bi = (rng.next_u64() % 32) as u32;
    match rng.next_u64() % 4 {
        0 => ((18 << 26) | (slot * 4) | lk, None).map_vec(),
        1 => {
            let bo = pick_bo(rng);
            ((16 << 26) | (bo << 21) | (bi << 16) | (slot * 4) | lk, None).map_vec()
        }
        2 => {
            let bo = pick_bo(rng);
            ((19 << 26) | (bo << 21) | (bi << 16) | (16 << 1) | lk, Some(slot)).map_vec()
        }
        _ => {
            // bcctr may not decrement CTR: BO[2] set.
            let bo = pick_bo(rng) | 0b00100;
            ((19 << 26) | (bo << 21) | (bi << 16) | (528 << 1) | lk, Some(slot)).map_vec()
        }
    }
}

/// A BO from the valid encodings: `1z1zz` always, and the decrement/test
/// combinations, with the hint bits random.
fn pick_bo(rng: &mut Rng) -> u32 {
    let forms = [0b00000, 0b00010, 0b00100, 0b01000, 0b01010, 0b01100, 0b10000, 0b10010, 0b10100];
    let mut bo = forms[(rng.next_u64() % forms.len() as u64) as usize];
    // The "y"/"at" hint bits, where the form has them.
    if bo & 0b10100 == 0b10100 {
        return 20;
    }
    if bo & 0b10000 != 0 {
        bo |= (rng.next_u64() % 2) as u32 & 1;
        bo |= (((rng.next_u64() % 2) as u32) << 3) & 0b01000;
    } else if bo & 0b00100 != 0 {
        bo |= (rng.next_u64() % 4) as u32 & 0b11;
        if bo & 0b11 == 0b01 {
            bo &= !0b11;
        }
    } else {
        bo |= (rng.next_u64() % 2) as u32;
    }
    bo
}

trait MapVec {
    fn map_vec(self) -> (Vec<u32>, Option<u32>);
}
impl MapVec for (u32, Option<u32>) {
    fn map_vec(self) -> (Vec<u32>, Option<u32>) {
        (vec![self.0], self.1)
    }
}

// -- the harness ---------------------------------------------------------------

fn harness(cases: &[Case]) -> String {
    let mut s = String::from(".text\n.globl _start\n_start:\n");
    s.push_str("\tlis 31, gexdata@ha\n\taddi 31, 31, gexdata@l\n");
    for (k, c) in cases.iter().enumerate() {
        s.push_str(&format!("case{k}:\n"));
        for f in 0..32 {
            s.push_str(&format!("\tlfd {f}, {}(31)\n", IN_F + f * 8));
        }
        s.push_str(&format!("\tlwz 0, {IN_CR}(31)\n\tmtcrf 255, 0\n"));
        s.push_str(&format!("\tlwz 0, {IN_XER}(31)\n\tmtxer 0\n"));
        s.push_str(&format!("\tlwz 0, {IN_LR}(31)\n\tmtlr 0\n"));
        s.push_str(&format!("\tlwz 0, {IN_CTR}(31)\n\tmtctr 0\n"));
        for r in 0..31 {
            s.push_str(&format!("\tlwz {r}, {}(31)\n", IN_R + r * 4));
        }
        match c.class {
            Class::Mem => {
                s.push_str(&format!("\taddi 29, 31, {}\n", SCRATCH as i32 + MEM_BIAS));
                // The index register: a small offset, word-aligned for the
                // reservation instructions.
                let off = (c.record[IN_R + 28 * 4 + 3] as i32 % 40) - MEM_BIAS;
                let op = decode(c.words[0], 0).op;
                let off = if matches!(op, Op::Lwarx | Op::Stwcx | Op::Lfdx | Op::Stfdx | Op::Lfdux | Op::Stfdux) {
                    off & !7
                } else {
                    off
                };
                s.push_str(&format!("\tli 28, {off}\n"));
            }
            Class::Branch => {
                if let Some(slot) = c.reg_target {
                    let reg = if (c.words[0] >> 1) & 0x3ff == 528 { "ctr" } else { "lr" };
                    s.push_str(&format!(
                        "\tlis 30, ladder{k}_{slot}@ha\n\taddi 30, 30, ladder{k}_{slot}@l\n\tmt{reg} 30\n"
                    ));
                }
                s.push_str("\tli 30, 0\n");
            }
            _ => {}
        }
        for w in &c.words {
            s.push_str(&format!("\t.long {w:#010x}\n"));
        }
        if c.class == Class::Branch {
            for slot in 1..=LADDER {
                s.push_str(&format!("ladder{k}_{slot}:\n\taddi 30, 30, {}\n", 1u32 << (slot - 1)));
            }
        }
        for r in 0..31 {
            s.push_str(&format!("\tstw {r}, {}(31)\n", OUT_R + r * 4));
        }
        s.push_str(&format!("\tmfcr 0\n\tstw 0, {OUT_CR}(31)\n"));
        s.push_str(&format!("\tmfxer 0\n\tstw 0, {OUT_XER}(31)\n"));
        s.push_str(&format!("\tmflr 0\n\tstw 0, {OUT_LR}(31)\n"));
        s.push_str(&format!("\tmfctr 0\n\tstw 0, {OUT_CTR}(31)\n"));
        for f in 0..32 {
            s.push_str(&format!("\tstfd {f}, {}(31)\n", OUT_F + f * 8));
        }
        s.push_str(&format!("\taddi 31, 31, {REC}\n"));
    }
    let len = cases.len() * REC;
    s.push_str("\tlis 4, gexdata@ha\n\taddi 4, 4, gexdata@l\n");
    s.push_str(&format!("\tlis 5, {}\n\tori 5, 5, {}\n", len >> 16, len & 0xffff));
    // write() until everything is out: a pipe may take less than asked.
    s.push_str("gexwrite:\n\tli 3, 1\n\tli 0, 4\n\tsc\n\tbso gexdone\n");
    s.push_str("\tadd 4, 4, 3\n\tsubf 5, 3, 5\n\tcmpwi 5, 0\n\tbgt gexwrite\n");
    s.push_str("gexdone:\n\tli 3, 0\n\tli 0, 234\n\tsc\n");
    s.push_str("\n.data\n.balign 32\ngexdata:\n");
    for c in cases {
        for chunk in c.record.chunks(16) {
            let bytes: Vec<String> = chunk.iter().map(|b| format!("{b:#04x}")).collect();
            s.push_str(&format!("\t.byte {}\n", bytes.join(",")));
        }
    }
    s
}

fn run_bin(cfg: &Config, n: usize, cases: &[Case]) -> Result<(Vec<u8>, Vec<u8>), String> {
    let src = cfg.workdir.join(format!("batch{n}.s"));
    let exe = cfg.workdir.join(format!("batch{n}.elf"));
    std::fs::write(&src, harness(cases)).map_err(|e| e.to_string())?;
    let out = Command::new(&cfg.clang)
        .args([
            "--target=powerpc-linux-musl",
            "-nostdlib",
            "-static",
            "-fuse-ld=lld",
            "-Wl,--image-base=0x1000000",
            "-o",
        ])
        .arg(&exe)
        .arg(&src)
        .output()
        .map_err(|e| format!("{}: {e}", cfg.clang.display()))?;
    if !out.status.success() {
        return Err(format!("assembling: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let run = |bin: &PathBuf, args: &[&std::ffi::OsStr]| -> Result<Vec<u8>, String> {
        Command::new(bin)
            .args(args)
            .output()
            .map(|o| o.stdout)
            .map_err(|e| format!("{}: {e}", bin.display()))
    };
    let q = run(&cfg.qemu, &[exe.as_os_str()])?;
    let g = run(&cfg.gexc, &["run".as_ref(), exe.as_os_str()])?;
    if q.len() == cases.len() * REC && g.len() == q.len() {
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&exe);
    }
    Ok((q, g))
}

// -- reporting ------------------------------------------------------------------

fn report(c: &Case, q: Option<&[u8]>, g: Option<&[u8]>) {
    let mut out = std::io::stderr();
    let _ = writeln!(out, "\n=== MISMATCH ({}) ===", c.class.name());
    for w in &c.words {
        let _ = writeln!(out, "  {w:08x}  {}", disasm(&decode(*w, 0x1000), 0x1000));
    }
    let (Some(q), Some(g)) = (q, g) else {
        let _ = writeln!(out, "  output missing: qemu {} gex {}", q.is_some(), g.is_some());
        return;
    };
    let rec = &c.record;
    for r in 0..31 {
        let (a, b) = (be32(q, OUT_R + r * 4), be32(g, OUT_R + r * 4));
        if a != b {
            let _ = writeln!(out, "  r{r:<2} seed={:08x} qemu={a:08x} gex={b:08x}", be32(rec, IN_R + r * 4));
        }
    }
    for (name, i, o) in [
        ("cr", IN_CR, OUT_CR),
        ("xer", IN_XER, OUT_XER),
        ("lr", IN_LR, OUT_LR),
        ("ctr", IN_CTR, OUT_CTR),
    ] {
        let (a, b) = (be32(q, o), be32(g, o));
        if a != b {
            let _ = writeln!(out, "  {name:<4} seed={:08x} qemu={a:08x} gex={b:08x}", be32(rec, i));
        }
    }
    for f in 0..32 {
        let (a, b) = (be64(q, OUT_F + f * 8), be64(g, OUT_F + f * 8));
        if a != b && !(is_nan(a) && is_nan(b)) {
            let _ = writeln!(
                out,
                "  f{f:<2} seed={:016x} qemu={a:016x} ({:e}) gex={b:016x} ({:e})",
                be64(rec, IN_F + f * 8),
                f64::from_bits(a),
                f64::from_bits(b)
            );
        }
    }
    if q[SCRATCH..REC] != g[SCRATCH..REC] {
        let _ = writeln!(out, "  scratch seed {:02x?}", &rec[SCRATCH..REC]);
        let _ = writeln!(out, "  scratch qemu {:02x?}", &q[SCRATCH..REC]);
        let _ = writeln!(out, "  scratch gex  {:02x?}", &g[SCRATCH..REC]);
    }
}

// -- randomness ------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 16) as u32
    }
    /// Integer seeds biased toward the edges where flag and carry bugs live.
    fn seed32(&mut self) -> u32 {
        match self.next_u64() % 8 {
            0 => 0,
            1 => u32::MAX,
            2 => 0x8000_0000,
            3 => 0x7fff_ffff,
            4 => (self.next_u64() % 64) as u32,
            5 => (self.next_u64() & 0xffff) as u32,
            _ => self.next_u32(),
        }
    }
    /// FP seeds: specials, small integers, extremes, denormals and random
    /// values; single-representable when `single`.
    fn seed_f64(&mut self, single: bool) -> u64 {
        let v: f64 = match self.next_u64() % 12 {
            0 => 0.0,
            1 => -0.0,
            2 => 1.0,
            3 => -1.5,
            4 => f64::INFINITY,
            5 => f64::NEG_INFINITY,
            6 => f64::NAN,
            7 => (self.next_u64() % 2000) as f64 - 1000.0 + 0.5,
            8 => {
                if single {
                    f32::from_bits(1 + (self.next_u32() & 0x7f_ffff)) as f64
                } else {
                    f64::from_bits(1 + (self.next_u64() & 0x000f_ffff_ffff_ffff))
                }
            }
            9 => {
                if single {
                    f32::MAX as f64
                } else {
                    f64::MAX
                }
            }
            _ => {
                if single {
                    f32::from_bits(self.next_u32() & 0x7fff_ffff | (self.next_u32() & 0x8000_0000)) as f64
                } else {
                    f64::from_bits(self.next_u64())
                }
            }
        };
        if single {
            (v as f32 as f64).to_bits()
        } else {
            v.to_bits()
        }
    }
}

fn parse_args() -> Result<Config, String> {
    let mut cfg = Config {
        iters: 2000,
        per_bin: 200,
        seed: 0x2545_f491_4f6c_dd1d,
        classes: vec![Class::Int, Class::Cr, Class::Fp, Class::Mem, Class::Branch],
        qemu: PathBuf::from("qemu-ppc"),
        gexc: PathBuf::from("target/ppc/release/gexc"),
        clang: PathBuf::from("clang"),
        workdir: PathBuf::from("target/difffuzz-ppc"),
        verbose: false,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let need = |i: usize| -> Result<String, String> {
            args.get(i + 1).cloned().ok_or_else(|| format!("{} needs a value", args[i]))
        };
        match args[i].as_str() {
            "--iters" => cfg.iters = need(i)?.parse().map_err(|_| "bad --iters")?,
            "--per-bin" => cfg.per_bin = need(i)?.parse().map_err(|_| "bad --per-bin")?,
            "--seed" => cfg.seed = need(i)?.parse().map_err(|_| "bad --seed")?,
            "--class" => {
                cfg.classes = need(i)?
                    .split(',')
                    .map(|c| match c {
                        "int" => Ok(Class::Int),
                        "cr" => Ok(Class::Cr),
                        "fp" => Ok(Class::Fp),
                        "mem" => Ok(Class::Mem),
                        "branch" => Ok(Class::Branch),
                        other => Err(format!("unknown class `{other}`")),
                    })
                    .collect::<Result<_, _>>()?
            }
            "--qemu" => cfg.qemu = need(i)?.into(),
            "--gexc" => cfg.gexc = need(i)?.into(),
            "--clang" => cfg.clang = need(i)?.into(),
            "--workdir" => cfg.workdir = need(i)?.into(),
            "--verbose" => {
                cfg.verbose = true;
                i += 1;
                continue;
            }
            other => return Err(format!("unknown option `{other}`")),
        }
        i += 2;
    }
    Ok(cfg)
}
