//! Bulk decoder validation against binutils.
//!
//! Reads `objdump -d` output on stdin, re-decodes each raw encoding with
//! `gex-isa`, and reports where we disagree. This gives coverage over millions
//! of *real* instructions from real binaries, which no hand-written test suite
//! matches.
//!
//!   aarch64-linux-musl-objdump -d /path/to/bin | cargo run --example objdump-diff
//!
//! Exit code is non-zero if any instruction fails to decode. Operand-text
//! mismatches are reported but not fatal: binutils has alias spellings we do
//! not need to reproduce exactly, whereas a mnemonic mismatch is a real bug.

use gex_isa::{decode, fmt::disasm, Inst};
use std::collections::BTreeMap;
use std::io::Read;

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();

    let mut total = 0usize;
    let mut unknown: BTreeMap<String, (u32, usize)> = BTreeMap::new();
    let mut mnemonic_diff: BTreeMap<String, (String, String, u32, usize)> = BTreeMap::new();
    let mut operand_diff = 0usize;
    let mut operand_samples: BTreeMap<String, (String, String, usize, u64, u32)> = BTreeMap::new();
    let mut agree = 0usize;

    for line in input.lines() {
        // Format: "  4004a0:\t910003fd \tmov\tx29, sp"
        let Some((addr_part, rest)) = line.split_once(":\t") else {
            continue;
        };
        let Ok(pc) = u64::from_str_radix(addr_part.trim(), 16) else {
            continue;
        };
        let mut fields = rest.splitn(2, '\t');
        let hex = fields.next().unwrap_or("").trim();
        if hex.len() != 8 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(raw) = u32::from_str_radix(hex, 16) else {
            continue;
        };
        let expect_full = fields.next().unwrap_or("").trim();
        if expect_full.is_empty() {
            continue;
        }
        // Strip binutils' trailing "// comment" and symbol annotations.
        let expect_full = expect_full
            .split("//")
            .next()
            .unwrap()
            .split(" <")
            .next()
            .unwrap()
            .trim();
        let expect_mnem = expect_full.split_whitespace().next().unwrap_or("");
        if expect_mnem.starts_with('.') {
            continue; // data words disassembled as directives
        }

        total += 1;
        let inst = decode(raw, pc);
        match inst {
            Inst::Unknown { .. } | Inst::Undefined { .. } => {
                let e = unknown.entry(expect_mnem.to_string()).or_insert((raw, 0));
                e.1 += 1;
                continue;
            }
            _ => {}
        }
        let got_full = disasm(&inst, pc);
        let got_mnem = got_full.split_whitespace().next().unwrap_or("");
        let aliased = got_mnem != expect_mnem && alias_equivalent(expect_mnem, got_mnem);
        if got_mnem != expect_mnem && !aliased {
            let e = mnemonic_diff
                .entry(format!("{expect_mnem} -> {got_mnem}"))
                .or_insert((expect_full.to_string(), got_full.clone(), raw, 0));
            e.3 += 1;
        } else if normalize_cmp(&got_full, aliased) != normalize_cmp(expect_full, aliased) {
            operand_diff += 1;
            let e = operand_samples
                .entry(expect_mnem.to_string())
                .or_insert((expect_full.to_string(), got_full.clone(), 0, pc, raw));
            e.2 += 1;
        } else {
            agree += 1;
        }
    }

    println!("instructions:        {total}");
    println!("exact agreement:     {agree}");
    println!("operand-text diffs:  {operand_diff}");
    let undecoded: usize = unknown.values().map(|v| v.1).sum();
    println!("undecoded:           {undecoded}");
    let mismatched: usize = mnemonic_diff.values().map(|v| v.3).sum();
    println!("mnemonic mismatches: {mismatched}");

    if !unknown.is_empty() {
        println!("\n-- undecoded, by binutils mnemonic --");
        let mut v: Vec<_> = unknown.iter().collect();
        v.sort_by_key(|(_, (_, n))| std::cmp::Reverse(*n));
        for (mnem, (sample, n)) in v.iter().take(40) {
            println!("  {n:>7}  {mnem:<12} e.g. 0x{sample:08x}");
        }
    }
    if !mnemonic_diff.is_empty() {
        println!("\n-- mnemonic mismatches --");
        let mut v: Vec<_> = mnemonic_diff.iter().collect();
        v.sort_by_key(|(_, (_, _, _, n))| std::cmp::Reverse(*n));
        for (k, (exp, got, raw, n)) in v.iter().take(40) {
            println!("  {n:>7}  {k}\n           0x{raw:08x}  binutils: {exp}\n                       gex: {got}");
        }
    }

    if !operand_samples.is_empty() {
        println!("\n-- operand-text diffs, one sample per mnemonic --");
        let mut v: Vec<_> = operand_samples.iter().collect();
        v.sort_by_key(|(_, (_, _, n, _, _))| std::cmp::Reverse(*n));
        for (m, (exp, got, n, pc, raw)) in v.iter().take(30) {
            println!("  {n:>6}  {m:<8} @0x{pc:x} 0x{raw:08x}  binutils: {exp:<32} gex: {got}");
        }
    }

    if mismatched > 0 {
        std::process::exit(1);
    }
}

/// Mnemonic pairs that name the same operation.
///
/// The unscaled-offset forms (`ldur`/`stur`) are a distinct *encoding* but not
/// a distinct *semantic*: gex deliberately folds them into the plain
/// offset addressing mode, so binutils' spelling difference is expected.
fn alias_equivalent(expect: &str, got: &str) -> bool {
    let unscaled = |m: &str| -> Option<&'static str> {
        Some(match m {
            "ldur" => "ldr",
            "ldurb" => "ldrb",
            "ldurh" => "ldrh",
            "ldursb" => "ldrsb",
            "ldursh" => "ldrsh",
            "ldursw" => "ldrsw",
            "stur" => "str",
            "sturb" => "strb",
            "sturh" => "strh",
            "prfum" => "prfm",
            // Unprivileged forms behave identically at EL0.
            "ldtr" => "ldr",
            "ldtrb" => "ldrb",
            "ldtrh" => "ldrh",
            "ldtrsb" => "ldrsb",
            "ldtrsh" => "ldrsh",
            "ldtrsw" => "ldrsw",
            "sttr" => "str",
            "sttrb" => "strb",
            "sttrh" => "strh",
            _ => return None,
        })
    };
    unscaled(expect) == Some(got)
}

/// Normalise for comparison, optionally dropping the mnemonic when the two
/// sides use equivalent-but-differently-spelled mnemonics.
fn normalize_cmp(s: &str, drop_mnemonic: bool) -> String {
    let n = normalize(s);
    if drop_mnemonic {
        n.split_once(' ').map(|(_, r)| r.to_string()).unwrap_or_default()
    } else {
        n
    }
}

/// Canonicalise assembly text so only *semantic* operand differences survive.
///
/// Three families of spelling difference are folded away:
///   * hex vs decimal (`#0x10` and `#16`),
///   * gex printing branch targets as `0x1234` where binutils prints `1234`,
///   * gex folding `add x0, x0, #0x24, lsl #12` into the final immediate.
fn normalize(s: &str) -> String {
    let s = s.to_ascii_lowercase().replace(['\t', ','], " ");
    let raw: Vec<&str> = s.split_whitespace().collect();
    if raw.is_empty() {
        return String::new();
    }

    // binutils prints branch and pc-relative targets as bare hex; gex prefixes
    // them with `0x`. Read the final token as hex for those mnemonics so both
    // spellings land on the same number. This must happen before the generic
    // decimalisation below, which would otherwise read `4cc` as a non-number
    // and `0x4cc` as decimal 1228.
    let m = raw[0];
    let pc_rel = matches!(m, "b" | "bl" | "adr" | "adrp")
        || m.starts_with("b.")
        || m.starts_with("cb")
        || m.starts_with("tb");

    let mut toks: Vec<String> = Vec::with_capacity(raw.len());
    for (i, t) in raw.iter().enumerate() {
        let is_target = pc_rel && i == raw.len() - 1 && i > 0;
        if is_target {
            let body = t.trim_start_matches("0x");
            match i128::from_str_radix(body, 16) {
                Ok(v) => toks.push(v.to_string()),
                Err(_) => toks.push((*t).to_string()),
            }
        } else {
            toks.push(canon_number(t));
        }
    }

    // Fold a trailing `lsl #12` shift into the preceding immediate, which is
    // how gex represents `add/sub/cmp (immediate, shifted)`.
    if toks.len() >= 3 && toks[toks.len() - 2] == "lsl" {
        if let (Ok(base), Ok(sh)) = (
            toks[toks.len() - 3].parse::<i128>(),
            toks[toks.len() - 1].parse::<u32>(),
        ) {
            if sh == 12 {
                let folded = base << 12;
                let n = toks.len();
                toks.truncate(n - 3);
                toks.push(folded.to_string());
            }
        }
    }
    toks.join(" ")
}

/// Rewrite one token to a canonical form: numbers become decimal, register
/// and mnemonic tokens pass through unchanged.
fn canon_number(tok: &str) -> String {
    let t = tok.trim_matches(|c| c == '[' || c == ']' || c == '!' || c == '#');
    let brackets: String = tok.chars().filter(|c| "[]!".contains(*c)).collect();
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    let parsed = if let Some(hex) = body.strip_prefix("0x") {
        i128::from_str_radix(hex, 16).ok()
    } else if !body.is_empty() && body.chars().all(|c| c.is_ascii_digit()) {
        body.parse::<i128>().ok()
    } else {
        None
    };
    match parsed {
        Some(v) => format!("{}{}", if neg { -v } else { v }, brackets),
        None => tok.to_string(),
    }
}
