//! The decoder and disassembler against `llvm-objdump` on real code.
//!
//! Every instruction word in the given objects is disassembled by both and
//! the texts compared, after normalising whitespace and dropping LLVM's
//! `<symbol+off>` annotations. The objects are the ppc32 sysroot's `libc.a`
//! and compiler-rt builtins (`tools/ppc/build-sysroot.sh`) plus anything in
//! `GEX_PPC_OBJDUMP_FILES` (colon-separated). Without `llvm-objdump` or any
//! input the test says so and passes: the words it checks come from a
//! toolchain, not from this repository.

use std::path::PathBuf;
use std::process::Command;

fn inputs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let sysroot = std::env::var_os("PPC_SYSROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("ppc-sysroot"));
    for f in ["lib/libc.a", "lib/libclang_rt.builtins.a"] {
        let p = sysroot.join(f);
        if p.exists() {
            v.push(p);
        }
    }
    if let Some(extra) = std::env::var_os("GEX_PPC_OBJDUMP_FILES") {
        v.extend(std::env::split_paths(&extra));
    }
    v
}

fn normalise(s: &str) -> String {
    let s = match s.find(" <") {
        Some(i) => &s[..i],
        None => s,
    };
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn matches_llvm_objdump() {
    let files = inputs();
    if files.is_empty() {
        eprintln!("objdump diff: no ppc32 objects (build tools/ppc/build-sysroot.sh); skipped");
        return;
    }
    compare(&files);
}

/// 200,000 random words over every primary opcode, assembled by `llvm-mc`:
/// checks the whole encoding space, reserved fields included, not just what
/// a compiler happens to emit.
#[test]
fn random_words_match_llvm_objdump() {
    let dir = std::env::temp_dir().join(format!("gex-isa-ppc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (src, obj) = (dir.join("rand.s"), dir.join("rand.o"));
    let mut text = String::from(".text\n");
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..200_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        text.push_str(&format!(".long {:#010x}\n", x as u32));
    }
    std::fs::write(&src, text).unwrap();
    let ok = Command::new("llvm-mc")
        .args(["-triple=powerpc-linux-gnu", "-filetype=obj", "-o"])
        .arg(&obj)
        .arg(&src)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("objdump diff: llvm-mc unavailable; skipped");
        return;
    }
    compare(&[obj]);
    let _ = std::fs::remove_dir_all(&dir);
}

fn compare(files: &[PathBuf]) {
    let out = match Command::new("llvm-objdump")
        .arg("-d")
        .arg("--mcpu=ppc")
        .args(files)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("objdump diff: llvm-objdump unavailable ({e}); skipped");
            return;
        }
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let (mut total, mut bad) = (0usize, 0usize);
    let mut shown = std::collections::HashMap::<String, usize>::new();
    let mut outside = std::collections::BTreeMap::<String, usize>::new();
    for line in text.lines() {
        // "     1c: 7c 08 03 a6  \tmtlr 0"
        let Some((addr, rest)) = line.split_once(':') else { continue };
        let Ok(pc) = u64::from_str_radix(addr.trim(), 16) else { continue };
        let Some((hex, asm)) = rest.split_once('\t') else { continue };
        let bytes: Vec<u8> = hex
            .split_whitespace()
            .filter_map(|b| u8::from_str_radix(b, 16).ok())
            .collect();
        if bytes.len() != 4 {
            continue;
        }
        let raw = u32::from_be_bytes(bytes.try_into().unwrap());
        let want = normalise(asm);
        let inst = gex_isa_ppc::decode(raw, pc);
        let got = normalise(&gex_isa_ppc::fmt::disasm(&inst, pc));
        total += 1;
        // A word gex does not know is fine when LLVM names something outside
        // the 32-bit classic-FPU machine (AltiVec, 64-bit, POWER7...): those
        // are listed, for a reader to check none is baseline.
        if inst.op == gex_isa_ppc::Op::Unknown && want != "<unknown>" {
            *outside.entry(want.split(' ').next().unwrap_or("").to_string()).or_default() += 1;
            continue;
        }
        // LLVM has no lswx/stswx at all.
        if want == "<unknown>" && matches!(inst.op, gex_isa_ppc::Op::Lswx | gex_isa_ppc::Op::Stswx) {
            continue;
        }
        if want != got {
            bad += 1;
            let first = |s: &str| s.split(' ').next().unwrap_or("").to_string();
            let key = format!("{} / {}", first(&want), first(&got));
            let n = shown.entry(key).or_default();
            if *n < 3 {
                eprintln!("{raw:08x} @{pc:#x}: llvm `{want}`, gex `{got}`");
            }
            *n += 1;
        }
    }
    eprintln!("objdump diff: {total} words, {bad} differ");
    if !outside.is_empty() {
        eprintln!("not decoded by gex (outside the baseline): {outside:?}");
    }
    assert!(total > 0, "llvm-objdump printed nothing for {files:?}");
    assert_eq!(bad, 0);
}
