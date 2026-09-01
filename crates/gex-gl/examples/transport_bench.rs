//! Allocation-amortized command-stream throughput for the GL transport.
//!
//! This measures the gex wire codec, not GPU execution or presentation. Run
//! with `cargo run --release -p gex-gl --example transport_bench -- 1000000`.

use gex_gl::proto::{op, Args, Batch, Commands};
use std::time::Instant;

fn rate(label: &str, commands: usize, bytes: usize, elapsed: std::time::Duration) {
    let seconds = elapsed.as_secs_f64();
    println!(
        "{label}: {:.1} Mcommands/s, {:.1} MiB/s ({commands} commands, {bytes} bytes, {:.3}s)",
        commands as f64 / seconds / 1_000_000.0,
        bytes as f64 / seconds / (1024.0 * 1024.0),
        seconds,
    );
}

fn main() {
    let count = std::env::args().nth(1).and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    let start = Instant::now();
    let mut batch = Batch::new();
    for index in 0..count {
        batch.cmd(op::DRAW_ARRAYS, &[4, index as u32, 3]);
    }
    let encode = start.elapsed();
    rate("encode", count, batch.bytes().len(), encode);

    let start = Instant::now();
    let mut seen = 0usize;
    let mut checksum = 0u64;
    let mut commands = Commands::new(batch.bytes());
    while let Some(command) = commands.next() {
        let (opcode, body) = command.expect("benchmark generated a valid stream");
        assert_eq!(opcode, op::DRAW_ARRAYS);
        let mut args = Args::new(body);
        checksum = checksum.wrapping_add(args.u32().unwrap() as u64);
        checksum = checksum.wrapping_add(args.u32().unwrap() as u64);
        checksum = checksum.wrapping_add(args.u32().unwrap() as u64);
        seen += 1;
    }
    assert_eq!(seen, count);
    std::hint::black_box(checksum);
    rate("decode", count, batch.bytes().len(), start.elapsed());
}
