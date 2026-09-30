//! List the instructions gex cannot lift, from `llvm-objdump -d` output.
//!
//!     llvm-objdump -d prog | cargo run -p gex-lift --example gaps_a64 [label]
//!
//! Every instruction llvm disassembles is lifted on its own; one that ends
//! its block in a SIGILL (undecoded, undefined to gex, or decoded but not
//! implemented) is printed as `label <tab> word <tab> llvm's disassembly`,
//! one line per static occurrence. Words llvm cannot disassemble (literal
//! pools, padding) are skipped. The corpus scan in the lifter-gap notes
//! aggregates these lines.

use std::io::BufRead;

fn main() {
    let label = std::env::args().nth(1).unwrap_or_default();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        // "  400120: 6e613893     \tshll2 v19.4s, v4.8h, #16" (the word as
        // one big-endian token).
        let Some((addr, rest)) = line.trim_start().split_once(':') else { continue };
        let Ok(pc) = u64::from_str_radix(addr.trim(), 16) else { continue };
        let rest = rest.trim_start();
        let Some((hex, text)) = rest.split_once(char::is_whitespace) else { continue };
        let text = text.trim();
        if hex.len() != 8 || text.is_empty() || text.starts_with('<') || text.starts_with('.') {
            continue;
        }
        let Ok(word) = u32::from_str_radix(hex, 16) else { continue };
        let block = std::panic::catch_unwind(|| {
            gex_lift::lift_block(
                |a| (a == pc).then_some(word),
                pc,
                gex_lift::BlockLimits { max_insts: 1 },
            )
        });
        let bad = match block {
            Ok(b) => matches!(b.term, gex_ir::Term::Illegal { .. }),
            Err(_) => true,
        };
        if bad {
            println!("{label}\t{word:08x}\t{}", text.split('\t').collect::<Vec<_>>().join(" "));
        }
    }
}
