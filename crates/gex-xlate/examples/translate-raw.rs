//! Translate the runtime translator on a RAW binary mapped linearly at a base,
//! for debugging a translator panic on non-ELF code (a Wine PE, say).
//!
//!   cargo run --release -p gex-xlate --features x86 --example translate-raw -- \
//!       <file> <base> <exec_lo> <exec_hi> <addr> [max-blocks] [bits]
//!
//! Byte X of the file is placed at address base+X. `bits` is 64 (default) or 32.
use std::io::Write;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 6 {
        eprintln!("usage: translate-raw <file> <base> <exec_lo> <exec_hi> <addr> [max-blocks] [bits]");
        std::process::exit(2);
    }
    let hex = |s: &str| u64::from_str_radix(s.trim_start_matches("0x"), 16).expect("hex");
    let file = std::fs::read(&a[1]).expect("read file");
    let base = hex(&a[2]);
    let exec_lo = hex(&a[3]);
    let exec_hi = hex(&a[4]);
    let addr = hex(&a[5]);
    let max_blocks: usize = a.get(6).map_or(2048, |s| s.parse().expect("num"));
    let bits: u32 = a.get(7).map_or(64, |s| s.parse().expect("bits"));
    let shared = a.get(8).is_some_and(|s| s == "1" || s == "shared" || s == "true");

    let top = base + file.len() as u64 + 0x10000;
    let mut memory = vec![0u8; top as usize];
    memory[base as usize..base as usize + file.len()].copy_from_slice(&file);

    let exec = vec![(exec_lo, exec_hi)];
    gex_xlate::set_thread_mode(Some(bits == 32));

    eprintln!("translating from {addr:#x} ({bits}-bit), exec {exec_lo:#x}..{exec_hi:#x}, base {base:#x}");
    let _ = std::io::stderr().flush();
    let began = std::time::Instant::now();
    let (unit, stats) = gex_xlate::translate_runtime(
        &memory,
        addr,
        memory.len(),
        &|x| exec.iter().any(|(s, e)| x >= *s && x + 4 <= *e),
        &|_| false,
        gex_xlate::PageTable::default(),
        Default::default(),
        max_blocks,
        shared, // shared-memory (arg 8): match the browser shim when set
        &Default::default(),
    );
    println!(
        "{} blocks, {} instructions, {} KiB wasm, {:.2}s",
        stats.blocks,
        stats.instructions,
        unit.wasm.len() / 1024,
        began.elapsed().as_secs_f64()
    );
}
