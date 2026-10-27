//! Instruction-level differential fuzzing against QEMU.
//!
//! The method is the one `risu` uses, adapted so the oracle is a process
//! rather than hardware:
//!
//! 1. pick random 32-bit words and keep the ones the gex decoder recognises
//!    as a data-processing instruction that is safe to run in isolation;
//! 2. wrap them in a fixed harness that seeds every register and the
//!    condition flags from a known blob, runs the candidate, then writes the
//!    whole register file plus NZCV to stdout;
//! 3. assemble and link with the cross toolchain;
//! 4. run the result under QEMU and under gex, and compare the bytes.
//!
//! Any difference is a lifter bug with a one-instruction reproducer, which is
//! the whole point: a bug found this way arrives already minimised.
//!
//! `--simd` draws the candidates from the Advanced SIMD encoding space
//! instead, and the harness seeds and compares `v0`–`v31` as well. Only the
//! integer and bitwise members are drawn: gex's floating-point vector
//! lowerings knowingly differ from hardware on NaN payloads, fused
//! multiply-add rounding and the reciprocal estimates (README, fidelity
//! notes), so fuzzing them would only re-report those. `--simd-fp` includes
//! them anyway, for exploration. An instruction that decodes but that the
//! lifter turns into a SIGILL is counted and listed as unimplemented, not
//! reported as a difference; the corpus scan is what ranks those gaps.
//! `--inst WORD` (repeatable) tests only the given encodings, each case
//! against fresh seeds: the regression check for a specific fix.
//!
//! Memory and control-flow instructions are deliberately out of scope here.
//! A random branch or store would leave the harness, so those are covered by
//! the compiled-program tests in `tests/integration` instead.

use gex_isa::{decode, fmt::disasm, Dp2Op, Dp3Op, Inst, RegSel, VArr, VOp2, VShiftImmOp};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Register reserved by the harness to address its data block. Generated
/// instructions are rejected if they write it.
const HARNESS_REG: u8 = 28;

/// Registers seeded and compared: `x0`–`x27`.
const N_TESTED: usize = 28;

/// Offset of the vector register seeds within the harness data block.
const V_SEED_OFFSET: usize = 256;

/// Offset of the output area within the harness data block. The output
/// mirrors the seed area: `x0`–`x27`, NZCV, padding to 256, then `v0`–`v31`.
const OUT_OFFSET: usize = V_SEED_OFFSET + 32 * 16;

/// Bytes the harness writes: the general registers and flags, then every
/// vector register.
const OUT_BYTES: usize = 256 + 32 * 16;

struct Config {
    iters: usize,
    seed: u64,
    seq_len: usize,
    toolchain: PathBuf,
    qemu: PathBuf,
    gexc: PathBuf,
    workdir: PathBuf,
    verbose: bool,
    /// Tool name prefix inside `toolchain`, e.g. `aarch64-linux-gnu-`.
    prefix: String,
    /// Draw candidates from the Advanced SIMD space.
    simd: bool,
    /// Include the floating-point SIMD members.
    simd_fp: bool,
    /// `--inst`: test only these words, against fresh seeds each case.
    only: Vec<u32>,
}

fn main() {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gex-difffuzz: {e}");
            eprintln!(
                "\nusage: gex-difffuzz [--iters N] [--seed S] [--seq-len N] [--verbose]
                    [--simd] [--simd-fp] [--inst HEXWORD]...
                    [--toolchain DIR (GEX_TOOLCHAIN)] [--prefix TRIPLE-] [--qemu PATH (GEX_QEMU)]
                    [--gexc PATH] [--workdir DIR]"
            );
            std::process::exit(2);
        }
    };

    for tool in [&cfg.qemu, &cfg.gexc] {
        if tool.as_os_str().is_empty() || !tool.exists() {
            eprintln!("gex-difffuzz: missing {} (--qemu/GEX_QEMU, --gexc)", tool.display());
            std::process::exit(2);
        }
    }
    let as_bin = cfg.toolchain.join(format!("{}as", cfg.prefix));
    if !as_bin.exists() {
        eprintln!("gex-difffuzz: missing assembler {}", as_bin.display());
        std::process::exit(2);
    }
    std::fs::create_dir_all(&cfg.workdir).expect("creating the work directory");

    let mut rng = Rng::new(cfg.seed);
    let mut tested = 0usize;
    let mut failures = 0usize;
    let mut seen_ops: std::collections::BTreeMap<String, usize> = Default::default();
    // Decoded, accepted, but lifted to a SIGILL: mnemonic -> (count, example).
    let mut unimplemented: std::collections::BTreeMap<String, (usize, u32)> = Default::default();

    for i in 0..cfg.iters {
        let mut insts = Vec::with_capacity(cfg.seq_len);
        while insts.len() < cfg.seq_len {
            let raw = if !cfg.only.is_empty() {
                cfg.only[(rng.next_u64() % cfg.only.len() as u64) as usize]
            } else if cfg.simd {
                simd_word(&mut rng)
            } else {
                rng.next_u32()
            };
            let inst = if !cfg.only.is_empty() {
                Some(decode(raw, 0x400000))
            } else if cfg.simd {
                accept_simd(raw, cfg.simd_fp)
            } else {
                accept(raw)
            };
            if let Some(inst) = inst {
                let name = if cfg.simd || !cfg.only.is_empty() {
                    disasm(&inst, 0x400000)
                        .split(' ')
                        .next()
                        .unwrap_or("?")
                        .to_string()
                } else {
                    mnemonic(&inst).to_string()
                };
                if !lifts(raw) {
                    if !cfg.only.is_empty() {
                        eprintln!("gex-difffuzz: 0x{raw:08x} does not lift");
                        std::process::exit(1);
                    }
                    let e = unimplemented.entry(name).or_insert((0, raw));
                    e.0 += 1;
                    continue;
                }
                *seen_ops.entry(name).or_default() += 1;
                insts.push(raw);
            }
        }
        let seeds = rng.seed_block();

        match run_case(&cfg, i, &insts, &seeds) {
            Ok(Outcome::Match) => tested += 1,
            Ok(Outcome::Differ { qemu, gex }) => {
                failures += 1;
                report(&insts, &seeds, &qemu, &gex);
                if failures >= 20 {
                    eprintln!("gex-difffuzz: stopping after 20 failures");
                    break;
                }
            }
            Err(e) => {
                eprintln!("gex-difffuzz: case {i} could not be run: {e}");
            }
        }
    }

    if cfg.verbose {
        eprintln!("\ninstruction families exercised:");
        let mut v: Vec<_> = seen_ops.iter().collect();
        v.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        for (name, n) in v {
            eprintln!("  {n:>6}  {name}");
        }
    }
    if !unimplemented.is_empty() {
        eprintln!("\ndecoded but not lifted (skipped):");
        let mut v: Vec<_> = unimplemented.iter().collect();
        v.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
        for (name, (n, raw)) in v {
            eprintln!(
                "  {n:>6}  {name:<10} e.g. 0x{raw:08x} {}",
                disasm(&decode(*raw, 0x400000), 0x400000)
            );
        }
    }
    println!("{tested} cases matched, {failures} differed");
    if failures > 0 {
        std::process::exit(1);
    }
}

enum Outcome {
    Match,
    Differ { qemu: Vec<u8>, gex: Vec<u8> },
}

/// Decide whether a random word is a usable test instruction.
///
/// The candidate must decode, must be pure data processing, and must not
/// write the harness register or the stack pointer.
fn accept(raw: u32) -> Option<Inst> {
    let inst = decode(raw, 0x400000);
    let writes = destination(&inst)?;
    if matches!(writes, RegSel::Sp | RegSel::Gp(HARNESS_REG)) {
        return None;
    }
    // Reading SP is not a lifter question: the stack lives at a different
    // address under QEMU than under gex, so any instruction that observes it
    // would report a difference that says nothing about the semantics.
    if reads_sp(&inst) {
        return None;
    }
    // CRC32 is deliberately not lifted: gex does not advertise the extension
    // in AT_HWCAP, so no well-behaved guest emits it, and expanding it inline
    // would cost hundreds of IR nodes per instruction. Testing it here would
    // only re-report a known, intentional gap.
    if let Inst::DataProc2 { op, .. } = inst {
        if matches!(
            op,
            Dp2Op::Crc32b
                | Dp2Op::Crc32h
                | Dp2Op::Crc32w
                | Dp2Op::Crc32x
                | Dp2Op::Crc32cb
                | Dp2Op::Crc32ch
                | Dp2Op::Crc32cw
                | Dp2Op::Crc32cx
        ) {
            return None;
        }
    }
    // `smulh`/`umulh` with `Ra` other than 31: the field is should-be-one,
    // so the architecture allows either UNDEFINED (QEMU's choice) or the
    // ordinary result (gex's). Neither is wrong, so neither is tested.
    if let Inst::DataProc3 {
        op: Dp3Op::Smulh | Dp3Op::Umulh,
        ..
    } = inst
    {
        if bits(raw, 14, 10) != 0b11111 {
            return None;
        }
    }
    Some(inst)
}

fn bits(raw: u32, hi: u32, lo: u32) -> u32 {
    (raw >> lo) & ((1 << (hi - lo + 1)) - 1)
}

/// Whether gex lifts `raw` on its own, rather than ending the block in a
/// SIGILL.
fn lifts(raw: u32) -> bool {
    let block = gex_lift::lift_block(
        |pc| (pc == 0x400000).then_some(raw),
        0x400000,
        gex_lift::BlockLimits { max_insts: 1 },
    );
    !matches!(block.term, gex_ir::Term::Illegal { .. })
}

/// A random word from the Advanced SIMD data-processing space: bit 31 clear
/// and bits 28:24 one of the vector (`01110`, `01111`) or scalar (`11110`,
/// `11111`) groups, with Q, U and everything below bit 24 random.
fn simd_word(rng: &mut Rng) -> u32 {
    const GROUPS: [u32; 4] = [0b01110, 0b01111, 0b11110, 0b11111];
    let r = rng.next_u32();
    let group = GROUPS[(rng.next_u64() % 4) as usize];
    (r & 0x60ff_ffff) | (group << 24)
}

/// `accept` for `--simd`: an Advanced SIMD data-processing instruction that
/// reads and writes only registers the harness seeds and dumps.
fn accept_simd(raw: u32, fp: bool) -> Option<Inst> {
    let inst = decode(raw, 0x400000);
    let int = |arr: VArr| fp || !arr.float;
    let ok = match inst {
        Inst::VThreeSame { arr, .. }
        | Inst::VByElement { arr, .. }
        | Inst::VAcross { arr, .. }
        | Inst::VPermute { arr, .. } => int(arr),
        Inst::VTwoMisc { op, arr, .. } => {
            // The integer <-> float conversions carry an integer
            // arrangement on one side.
            int(arr) && (fp || !matches!(op, VOp2::Scvtf | VOp2::Ucvtf))
        }
        Inst::VShiftImm { op, arr, .. } => {
            int(arr)
                && (fp
                    || !matches!(
                        op,
                        VShiftImmOp::Scvtf
                            | VShiftImmOp::Ucvtf
                            | VShiftImmOp::Fcvtzs
                            | VShiftImmOp::Fcvtzu
                    ))
        }
        Inst::VWiden { .. }
        | Inst::VNarrow { .. }
        | Inst::VExt { .. }
        | Inst::VTableLookup { .. }
        | Inst::VDupElement { .. }
        | Inst::VInsElement { .. }
        | Inst::VLogical { .. }
        | Inst::VLogicalImm { .. }
        | Inst::VSelect { .. }
        | Inst::VMoveImm { .. }
        | Inst::VCnt { .. } => true,
        Inst::VDupGeneral { rn, .. } => !matches!(rn, RegSel::Sp),
        Inst::VMoveLane { to_vector, rd, .. } => {
            to_vector || !matches!(rd, RegSel::Sp | RegSel::Gp(HARNESS_REG))
        }
        Inst::VFpCvtPrecision { .. } => fp,
        _ => false,
    };
    ok.then_some(inst)
}

/// True when any source operand is the stack pointer.
fn reads_sp(inst: &Inst) -> bool {
    let sp = |r: RegSel| matches!(r, RegSel::Sp);
    match *inst {
        Inst::AddSubImm { rn, .. } | Inst::LogicalImm { rn, .. } => sp(rn),
        Inst::Bitfield { rn, .. } | Inst::DataProc1 { rn, .. } => sp(rn),
        Inst::Extract { rn, rm, .. }
        | Inst::AddSubShifted { rn, rm, .. }
        | Inst::AddSubExtended { rn, rm, .. }
        | Inst::AddSubCarry { rn, rm, .. }
        | Inst::LogicalShifted { rn, rm, .. }
        | Inst::CondSelect { rn, rm, .. }
        | Inst::DataProc2 { rn, rm, .. } => sp(rn) || sp(rm),
        Inst::DataProc3 { rn, rm, ra, .. } => sp(rn) || sp(rm) || sp(ra),
        Inst::CondCompare { rn, rm, .. } => sp(rn) || rm.map(sp).unwrap_or(false),
        // `bfm` also reads its destination register.
        _ => false,
    }
}

/// The register an instruction writes, or `None` when the instruction is not
/// one this fuzzer knows how to run in isolation.
fn destination(inst: &Inst) -> Option<RegSel> {
    match *inst {
        Inst::AddSubImm { rd, .. }
        | Inst::LogicalImm { rd, .. }
        | Inst::MoveWide { rd, .. }
        | Inst::Bitfield { rd, .. }
        | Inst::Extract { rd, .. }
        | Inst::AddSubShifted { rd, .. }
        | Inst::AddSubExtended { rd, .. }
        | Inst::AddSubCarry { rd, .. }
        | Inst::LogicalShifted { rd, .. }
        | Inst::CondSelect { rd, .. }
        | Inst::DataProc1 { rd, .. }
        | Inst::DataProc2 { rd, .. }
        | Inst::DataProc3 { rd, .. } => Some(rd),
        // Conditional compare writes only the flags.
        Inst::CondCompare { .. } => Some(RegSel::Zr),
        // `adr`/`adrp` are position-dependent; the harness would have to
        // agree on the link address, and they exercise no lifter logic worth
        // fuzzing.
        _ => None,
    }
}

fn mnemonic(inst: &Inst) -> &'static str {
    match inst {
        Inst::AddSubImm { .. } => "addsub-imm",
        Inst::LogicalImm { .. } => "logical-imm",
        Inst::MoveWide { .. } => "movewide",
        Inst::Bitfield { .. } => "bitfield",
        Inst::Extract { .. } => "extract",
        Inst::AddSubShifted { .. } => "addsub-shifted",
        Inst::AddSubExtended { .. } => "addsub-extended",
        Inst::AddSubCarry { .. } => "addsub-carry",
        Inst::LogicalShifted { .. } => "logical-shifted",
        Inst::CondSelect { .. } => "condsel",
        Inst::CondCompare { .. } => "condcmp",
        Inst::DataProc1 { .. } => "dataproc1",
        Inst::DataProc2 { .. } => "dataproc2",
        Inst::DataProc3 { .. } => "dataproc3",
        _ => "other",
    }
}

/// Build, assemble, link and run one case under both engines.
fn run_case(cfg: &Config, i: usize, insts: &[u32], seeds: &Seeds) -> Result<Outcome, String> {
    let src = cfg.workdir.join(format!("case{i}.s"));
    let obj = cfg.workdir.join(format!("case{i}.o"));
    let exe = cfg.workdir.join(format!("case{i}.elf"));

    std::fs::write(&src, harness_source(insts, seeds)).map_err(|e| e.to_string())?;

    let as_bin = cfg.toolchain.join(format!("{}as", cfg.prefix));
    let ld_bin = cfg.toolchain.join(format!("{}ld", cfg.prefix));
    run_tool(
        &as_bin,
        &["-o", &obj.to_string_lossy(), &src.to_string_lossy()],
    )?;
    run_tool(
        &ld_bin,
        &[
            "-static",
            "--no-pie",
            "-Ttext=0x400000",
            "-o",
            &exe.to_string_lossy(),
            &obj.to_string_lossy(),
        ],
    )?;

    // Both engines are run twice on disagreement. Spawning thousands of
    // processes in a tight loop occasionally yields an empty result from a
    // child that never got off the ground; retrying keeps that from being
    // reported as a lifter bug.
    let mut qemu = capture(&cfg.qemu, &[&exe.to_string_lossy()])?;
    let mut gex = capture(&cfg.gexc, &["run", &exe.to_string_lossy()])?;
    if qemu != gex {
        qemu = capture(&cfg.qemu, &[&exe.to_string_lossy()])?;
        gex = capture(&cfg.gexc, &["run", &exe.to_string_lossy()])?;
    }

    // Clean up the successful cases so a long run does not fill the disk.
    if qemu == gex {
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&obj);
        let _ = std::fs::remove_file(&exe);
        return Ok(Outcome::Match);
    }
    Ok(Outcome::Differ { qemu, gex })
}

fn run_tool(bin: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    if !out.status.success() {
        return Err(format!(
            "{} failed: {}",
            bin.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn capture(bin: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    Ok(out.stdout)
}

/// Emit the assembly harness around a candidate sequence.
fn harness_source(insts: &[u32], seeds: &Seeds) -> String {
    let mut s = String::new();
    s.push_str(".text\n.global _start\n_start:\n");
    s.push_str(&format!("\tadrp x{HARNESS_REG}, gexbuf\n"));
    s.push_str(&format!(
        "\tadd x{HARNESS_REG}, x{HARNESS_REG}, :lo12:gexbuf\n"
    ));
    // Flags first: the register loads that follow do not disturb them.
    s.push_str(&format!("\tldr x0, [x{HARNESS_REG}, #224]\n"));
    s.push_str("\tmsr nzcv, x0\n");
    for r in 0..32 {
        s.push_str(&format!(
            "\tldr q{r}, [x{HARNESS_REG}, #{}]\n",
            V_SEED_OFFSET + r * 16
        ));
    }
    for r in 0..N_TESTED {
        s.push_str(&format!("\tldr x{r}, [x{HARNESS_REG}, #{}]\n", r * 8));
    }

    s.push_str("\t// candidate sequence\n");
    for raw in insts {
        s.push_str(&format!("\t.inst 0x{raw:08x}\n"));
    }

    s.push_str("\t// dump\n");
    // x0 must be stored before anything is allowed to clobber it.
    s.push_str(&format!("\tstr x0, [x{HARNESS_REG}, #{OUT_OFFSET}]\n"));
    s.push_str("\tmrs x0, nzcv\n");
    s.push_str(&format!(
        "\tstr x0, [x{HARNESS_REG}, #{}]\n",
        OUT_OFFSET + N_TESTED * 8
    ));
    for r in 1..N_TESTED {
        s.push_str(&format!(
            "\tstr x{r}, [x{HARNESS_REG}, #{}]\n",
            OUT_OFFSET + r * 8
        ));
    }
    for r in 0..32 {
        s.push_str(&format!(
            "\tstr q{r}, [x{HARNESS_REG}, #{}]\n",
            OUT_OFFSET + 256 + r * 16
        ));
    }
    // write(1, buf + OUT_OFFSET, OUT_BYTES)
    s.push_str(&format!("\tadd x1, x{HARNESS_REG}, #{OUT_OFFSET}\n"));
    s.push_str("\tmov x0, #1\n");
    s.push_str(&format!("\tmov x2, #{OUT_BYTES}\n"));
    s.push_str("\tmov x8, #64\n\tsvc #0\n");
    s.push_str("\tmov x0, #0\n\tmov x8, #94\n\tsvc #0\n");

    s.push_str("\n.data\n.balign 16\ngexbuf:\n");
    for v in seeds.x.iter().take(N_TESTED) {
        s.push_str(&format!("\t.quad 0x{v:016x}\n"));
    }
    // NZCV seed, masked to the four flag bits.
    s.push_str(&format!(
        "\t.quad 0x{:016x}\n",
        seeds.x[N_TESTED] & 0xf000_0000
    ));
    s.push_str("\t.space 24\n");
    for v in seeds.v.iter() {
        s.push_str(&format!(
            "\t.quad 0x{:016x}, 0x{:016x}\n",
            *v as u64,
            (*v >> 64) as u64
        ));
    }
    s.push_str(&format!("\t.space {OUT_BYTES}\n"));
    s
}

/// Initial register state: `x` holds `x0`–`x27`, then NZCV in slot 28.
struct Seeds {
    x: [u64; 32],
    v: [u128; 32],
}

fn report(insts: &[u32], seeds: &Seeds, qemu: &[u8], gex: &[u8]) {
    let mut out = std::io::stderr();
    let _ = writeln!(out, "\n=== MISMATCH ===");
    for raw in insts {
        let _ = writeln!(
            out,
            "  0x{raw:08x}  {}",
            disasm(&decode(*raw, 0x400000), 0x400000)
        );
    }
    if qemu.len() != gex.len() {
        let _ = writeln!(
            out,
            "  output lengths differ: qemu {} bytes, gex {} bytes",
            qemu.len(),
            gex.len()
        );
        if gex.is_empty() {
            let _ = writeln!(out, "  (gex produced nothing — likely a trap or SIGILL)");
        }
        if qemu.is_empty() {
            let _ = writeln!(
                out,
                "  (QEMU produced nothing — the encoding is likely unallocated)"
            );
        }
        return;
    }
    for r in 0..N_TESTED {
        let q = read_u64(qemu, r * 8);
        let g = read_u64(gex, r * 8);
        if q != g {
            let _ = writeln!(
                out,
                "  x{r:<2} seed=0x{:016x}  qemu=0x{q:016x}  gex=0x{g:016x}",
                seeds.x[r]
            );
        }
    }
    for r in 0..32 {
        let off = 256 + r * 16;
        let q = read_u128(qemu, off);
        let g = read_u128(gex, off);
        if q != g {
            let _ = writeln!(
                out,
                "  v{r:<2} seed=0x{:032x}\n       qemu=0x{q:032x}\n        gex=0x{g:032x}",
                seeds.v[r]
            );
        }
    }
    let q = read_u64(qemu, N_TESTED * 8);
    let g = read_u64(gex, N_TESTED * 8);
    if q != g {
        let _ = writeln!(
            out,
            "  nzcv qemu={} gex={}",
            flag_str(q as u32),
            flag_str(g as u32)
        );
    }
}

fn read_u128(b: &[u8], off: usize) -> u128 {
    read_u64(b, off) as u128 | (read_u64(b, off + 8) as u128) << 64
}

fn read_u64(b: &[u8], off: usize) -> u64 {
    if off + 8 > b.len() {
        return 0;
    }
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

fn flag_str(v: u32) -> String {
    let names = ['N', 'Z', 'C', 'V'];
    let mut s = String::new();
    for (i, n) in names.iter().enumerate() {
        if v & (1 << (31 - i)) != 0 {
            s.push(*n);
        } else {
            s.push('-');
        }
    }
    s
}

/// A small xorshift generator so runs are reproducible from `--seed`.
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
    /// Seed values biased toward the interesting cases: zero, all-ones, the
    /// signed extremes and small magnitudes, where flag bugs live.
    fn seed_block(&mut self) -> Seeds {
        let mut x = [0u64; 32];
        for slot in x.iter_mut() {
            *slot = self.seed_value();
        }
        let mut v = [0u128; 32];
        for slot in v.iter_mut() {
            *slot = self.seed_half() as u128 | (self.seed_half() as u128) << 64;
        }
        Seeds { x, v }
    }

    fn seed_value(&mut self) -> u64 {
        match self.next_u64() % 8 {
            0 => 0,
            1 => u64::MAX,
            2 => 1 << 63,
            3 => (1 << 63) - 1,
            4 => 0xffff_ffff,
            5 => self.next_u64() & 0xff,
            6 => self.next_u64() & 0xffff_ffff,
            _ => self.next_u64(),
        }
    }

    /// Half a vector register. Lane boundaries are where the vector bugs
    /// live, so half the time every byte is drawn on its own from the
    /// extremes of a byte, halfword, word or doubleword lane.
    fn seed_half(&mut self) -> u64 {
        if self.next_u64() % 2 == 0 {
            return self.seed_value();
        }
        let mut out = 0u64;
        for i in 0..8 {
            let b: u64 = match self.next_u64() % 6 {
                0 => 0,
                1 => 0xff,
                2 => 0x80,
                3 => 0x7f,
                4 => 1,
                _ => self.next_u64() & 0xff,
            };
            out |= b << (i * 8);
        }
        out
    }
}

fn parse_args() -> Result<Config, String> {
    let mut cfg = Config {
        iters: 200,
        seed: 0x2545_f491_4f6c_dd1d,
        seq_len: 1,
        toolchain: std::env::var_os("GEX_TOOLCHAIN").map(PathBuf::from).unwrap_or_default(),
        qemu: std::env::var_os("GEX_QEMU").map(PathBuf::from).unwrap_or_default(),
        gexc: PathBuf::from("target/release/gexc"),
        workdir: PathBuf::from("target/difffuzz"),
        verbose: false,
        prefix: "aarch64-linux-musl-".into(),
        simd: false,
        simd_fp: false,
        only: Vec::new(),
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let need = |i: usize| -> Result<String, String> {
            args.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{} needs a value", args[i]))
        };
        match args[i].as_str() {
            "--iters" => {
                cfg.iters = need(i)?.parse().map_err(|_| "bad --iters")?;
                i += 1;
            }
            "--seed" => {
                cfg.seed = need(i)?.parse().map_err(|_| "bad --seed")?;
                i += 1;
            }
            "--seq-len" => {
                cfg.seq_len = need(i)?.parse().map_err(|_| "bad --seq-len")?;
                i += 1;
            }
            "--toolchain" => {
                cfg.toolchain = need(i)?.into();
                i += 1;
            }
            "--qemu" => {
                cfg.qemu = need(i)?.into();
                i += 1;
            }
            "--gexc" => {
                cfg.gexc = need(i)?.into();
                i += 1;
            }
            "--workdir" => {
                cfg.workdir = need(i)?.into();
                i += 1;
            }
            "--prefix" => {
                cfg.prefix = need(i)?;
                i += 1;
            }
            "--inst" => {
                let v = need(i)?;
                let word = u32::from_str_radix(v.trim_start_matches("0x"), 16)
                    .map_err(|_| format!("bad --inst `{v}`"))?;
                cfg.only.push(word);
                i += 1;
            }
            "--verbose" => cfg.verbose = true,
            "--simd" => cfg.simd = true,
            "--simd-fp" => {
                cfg.simd = true;
                cfg.simd_fp = true;
            }
            other => return Err(format!("unknown option `{other}`")),
        }
        i += 1;
    }
    Ok(cfg)
}
