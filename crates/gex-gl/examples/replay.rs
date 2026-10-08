//! Replay a recorded guest GL stream (`crates/gex-gl/src/trace.rs`) on the
//! native host (gex-gl's `Gl`, the device natively run games use), timing it:
//!
//!   cargo run --release -p gex-gl --example replay -- TRACE [FRAMES]
//!
//! Prints, over the last FRAMES frames (a batch with PRESENT ends a frame):
//! wall ms per frame inside `Gl::run`, batches per frame, and the commands
//! by host time.
use gex_gl::host::Gl;
use gex_gl::proto::{op, Commands};
use gex_gl::trace;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let frames: usize = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(300);
    let bytes = std::fs::read(path).expect("trace");
    let t = trace::parse(&bytes).expect("a trace");
    let mut gl = Gl::new(t.info.width.max(1) as i32, t.info.height.max(1) as i32).expect("host GL");
    let presents: Vec<bool> = t
        .batches
        .iter()
        .map(|b| {
            let mut c = Commands::new(&b.bytes);
            let mut present = false;
            while let Some(Ok((o, _))) = c.next() {
                present |= o == op::PRESENT;
            }
            present
        })
        .collect();
    let total = presents.iter().filter(|p| **p).count();
    let start_frame = total.saturating_sub(frames);
    let (mut frame, mut ms, mut nb, mut measuring) = (0usize, 0f64, 0usize, false);
    let mut per = Vec::new();
    let mut fms = 0f64;
    let t_all = Instant::now();
    for (i, b) in t.batches.iter().enumerate() {
        if !measuring && frame >= start_frame {
            measuring = true;
            gl.time_ops(true);
        }
        let t0 = Instant::now();
        let (_, status, _) = gl.run(&b.bytes);
        let e = t0.elapsed().as_secs_f64() * 1e3;
        if status != 0 {
            eprintln!("batch {i}: status {status}");
        }
        if measuring {
            ms += e;
            fms += e;
            nb += 1;
        }
        if presents[i] {
            if measuring {
                per.push(fms);
            }
            fms = 0.0;
            frame += 1;
        }
    }
    let n = per.len().max(1);
    per.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "{total} frames ({:.1} s to replay all); last {n}: {:.3} ms a frame in Gl::run (median {:.3}, p90 {:.3}), {:.2} batches a frame",
        t_all.elapsed().as_secs_f64(),
        ms / n as f64,
        per[n / 2],
        per[n * 9 / 10],
        nb as f64 / n as f64
    );
    let ops = gl.take_op_times();
    let sum: u64 = ops.iter().map(|o| o.2).sum();
    println!("commands: {:.3} ms a frame in all", sum as f64 / 1e6 / n as f64);
    for (o, c, ns) in ops.iter().take(30) {
        println!("  op {o:5} {:8.1}/frame {:8.3} ms/frame", *c as f64 / n as f64, *ns as f64 / 1e6 / n as f64);
    }
}
