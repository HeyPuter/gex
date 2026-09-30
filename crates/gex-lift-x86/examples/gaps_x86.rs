//! List the instructions gex cannot lift, from `llvm-objdump -d -M intel`
//! output of an x86-64 program.
//!
//!     llvm-objdump -d -M intel prog | cargo run -p gex-lift-x86 --example gaps_x86 [label]
//!
//! Every instruction llvm disassembles is lifted on its own; one that ends
//! its block in a SIGILL (undecoded or not implemented) is printed as
//! `label <tab> bytes <tab> llvm's disassembly`, one line per static
//! occurrence.

use std::io::BufRead;

fn main() {
    let label = std::env::args().nth(1).unwrap_or_default();
    let stdin = std::io::stdin();
    // llvm prints a `lock` or `rep` prefix on a line of its own; it is
    // carried over to the instruction that follows.
    let mut prefix: Option<(u64, Vec<u8>, String)> = None;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Some((addr, rest)) = line.trim_start().split_once(':') else { continue };
        let Ok(pc) = u64::from_str_radix(addr.trim(), 16) else { continue };
        let mut bytes = Vec::new();
        let mut text = rest.trim_start();
        loop {
            let t = text.trim_start();
            let tok = t.split_whitespace().next().unwrap_or("");
            if tok.len() == 2 && tok.chars().all(|c| c.is_ascii_hexdigit()) && bytes.len() < 15 {
                bytes.push(u8::from_str_radix(tok, 16).unwrap());
                text = &t[2..];
            } else {
                text = t;
                break;
            }
        }
        if bytes.is_empty() || text.is_empty() || text.starts_with('<') {
            continue;
        }
        let mut pc = pc;
        let mut text = text.to_string();
        if let Some((ppc, pbytes, ptext)) = prefix.take() {
            if ppc + pbytes.len() as u64 == pc {
                pc = ppc;
                bytes = [pbytes, bytes].concat();
                text = format!("{ptext} {text}");
            }
        }
        if matches!(text.trim(), "lock" | "rep" | "repne" | "repe" | "data16" | "cs" | "ds" | "fs" | "gs") {
            prefix = Some((pc, bytes, text.trim().to_string()));
            continue;
        }
        let code = bytes.clone();
        let block = std::panic::catch_unwind(|| {
            gex_lift_x86::lift_block(
                |a, buf: &mut [u8; 16]| {
                    let off = (a - pc) as usize;
                    let n = code.len().saturating_sub(off).min(16);
                    buf[..n].copy_from_slice(&code[off..off + n]);
                    n
                },
                pc,
                gex_lift_x86::BlockLimits { max_insts: 1 },
            )
        });
        let bad = match block {
            Ok(b) => matches!(b.term, gex_ir::Term::Illegal { .. }),
            Err(_) => true,
        };
        if bad {
            let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
            println!("{label}\t{}\t{}", hex.join(""), text.split_whitespace().collect::<Vec<_>>().join(" "));
        }
    }
}
