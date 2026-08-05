//! Instruction-level differential fuzzing against QEMU (PLAN §12.1).
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
//! Memory and control-flow instructions are deliberately out of scope here.
//! A random branch or store would leave the harness, so those are covered by
//! the compiled-program tests in `tests/integration` instead.

use gex_isa::{decode, fmt::disasm, Inst, RegSel};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Register reserved by the harness to address its data block. Generated
/// instructions are rejected if they write it.
const HARNESS_REG: u8 = 28;

/// Registers seeded and compared: `x0`–`x27`.
const N_TESTED: usize = 28;

/// Offset of the output area within the harness data block.
const OUT_OFFSET: usize = 256;

struct Config {
    iters: usize,
    seed: u64,
    seq_len: usize,
    toolchain: PathBuf,
    qemu: PathBuf,
    gexc: PathBuf,
    workdir: PathBuf,
    verbose: bool,
}

fn main() {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gex-difffuzz: {e}");
            eprintln!(
                "\nusage: gex-difffuzz [--iters N] [--seed S] [--seq-len N] [--verbose]
                    [--toolchain DIR] [--qemu PATH] [--gexc PATH] [--workdir DIR]"
            );
            std::process::exit(2);
        }
    };

    for tool in [&cfg.qemu, &cfg.gexc] {
        if !tool.exists() {
            eprintln!("gex-difffuzz: missing {}", tool.display());
            std::process::exit(2);
        }
    }
    let as_bin = cfg.toolchain.join("aarch64-linux-musl-as");
    if !as_bin.exists() {
        eprintln!("gex-difffuzz: missing assembler {}", as_bin.display());
        std::process::exit(2);
    }
    std::fs::create_dir_all(&cfg.workdir).expect("creating the work directory");

    let mut rng = Rng::new(cfg.seed);
    let mut tested = 0usize;
    let mut failures = 0usize;
    let mut seen_ops: std::collections::BTreeMap<&'static str, usize> = Default::default();

    for i in 0..cfg.iters {
        let mut insts = Vec::with_capacity(cfg.seq_len);
        while insts.len() < cfg.seq_len {
            let raw = rng.next_u32();
            if let Some(inst) = accept(raw) {
                let name = mnemonic(&inst);
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
    if matches!(writes, RegSel::Sp) || matches!(writes, RegSel::Gp(HARNESS_REG)) {
        return None;
    }
    Some(inst)
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
fn run_case(cfg: &Config, i: usize, insts: &[u32], seeds: &[u64; 32]) -> Result<Outcome, String> {
    let src = cfg.workdir.join(format!("case{i}.s"));
    let obj = cfg.workdir.join(format!("case{i}.o"));
    let exe = cfg.workdir.join(format!("case{i}.elf"));

    std::fs::write(&src, harness_source(insts, seeds)).map_err(|e| e.to_string())?;

    let as_bin = cfg.toolchain.join("aarch64-linux-musl-as");
    let ld_bin = cfg.toolchain.join("aarch64-linux-musl-ld");
    run_tool(&as_bin, &["-o", &obj.to_string_lossy(), &src.to_string_lossy()])?;
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

    let qemu = capture(&cfg.qemu, &[&exe.to_string_lossy()])?;
    let gex = capture(&cfg.gexc, &["run", &exe.to_string_lossy()])?;

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
fn harness_source(insts: &[u32], seeds: &[u64; 32]) -> String {
    let mut s = String::new();
    s.push_str(".text\n.global _start\n_start:\n");
    s.push_str(&format!("\tadrp x{HARNESS_REG}, gexbuf\n"));
    s.push_str(&format!(
        "\tadd x{HARNESS_REG}, x{HARNESS_REG}, :lo12:gexbuf\n"
    ));
    // Flags first: the register loads that follow do not disturb them.
    s.push_str(&format!("\tldr x0, [x{HARNESS_REG}, #224]\n"));
    s.push_str("\tmsr nzcv, x0\n");
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
    // write(1, buf + OUT_OFFSET, (N_TESTED + 1) * 8)
    s.push_str(&format!("\tadd x1, x{HARNESS_REG}, #{OUT_OFFSET}\n"));
    s.push_str("\tmov x0, #1\n");
    s.push_str(&format!("\tmov x2, #{}\n", (N_TESTED + 1) * 8));
    s.push_str("\tmov x8, #64\n\tsvc #0\n");
    s.push_str("\tmov x0, #0\n\tmov x8, #94\n\tsvc #0\n");

    s.push_str("\n.data\n.balign 16\ngexbuf:\n");
    for v in seeds.iter().take(N_TESTED) {
        s.push_str(&format!("\t.quad 0x{v:016x}\n"));
    }
    // NZCV seed, masked to the four flag bits.
    s.push_str(&format!(
        "\t.quad 0x{:016x}\n",
        seeds[N_TESTED] & 0xf000_0000
    ));
    s.push_str("\t.space 24\n");
    s.push_str(&format!("\t.space {}\n", (N_TESTED + 1) * 8));
    s
}

fn report(insts: &[u32], seeds: &[u64; 32], qemu: &[u8], gex: &[u8]) {
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
        return;
    }
    for r in 0..N_TESTED {
        let q = read_u64(qemu, r * 8);
        let g = read_u64(gex, r * 8);
        if q != g {
            let _ = writeln!(
                out,
                "  x{r:<2} seed=0x{:016x}  qemu=0x{q:016x}  gex=0x{g:016x}",
                seeds[r]
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
    fn seed_block(&mut self) -> [u64; 32] {
        let mut out = [0u64; 32];
        for slot in out.iter_mut() {
            *slot = match self.next_u64() % 8 {
                0 => 0,
                1 => u64::MAX,
                2 => 1 << 63,
                3 => (1 << 63) - 1,
                4 => 0xffff_ffff,
                5 => self.next_u64() & 0xff,
                6 => self.next_u64() & 0xffff_ffff,
                _ => self.next_u64(),
            };
        }
        out
    }
}

fn parse_args() -> Result<Config, String> {
    let mut cfg = Config {
        iters: 200,
        seed: 0x2545_f491_4f6c_dd1d,
        seq_len: 1,
        toolchain: PathBuf::from("/home/claude/toolchains/aarch64-linux-musl-cross/bin"),
        qemu: PathBuf::from("/home/claude/toolchains/qemu-aarch64"),
        gexc: PathBuf::from("target/release/gexc"),
        workdir: PathBuf::from("target/difffuzz"),
        verbose: false,
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
            "--verbose" => cfg.verbose = true,
            other => return Err(format!("unknown option `{other}`")),
        }
        i += 1;
    }
    Ok(cfg)
}
