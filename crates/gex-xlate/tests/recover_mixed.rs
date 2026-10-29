//! A PE32 and a PE32+ image recovered at once on two threads, as a WoW64
//! process's translator does: each thread decodes its image in the image's
//! own mode (`set_thread_compat`), and the results match the ones each gets
//! alone. With one process-wide mode, the threads' flips raced.

#![cfg(feature = "x86")]

use gex_xlate::func::{pe_input, recover_pe, RecoverOptions};

/// The image's functions and their summarised stack deltas: what a wrong
/// decode mode changes first.
fn shape(path: &str) -> Vec<(u64, Option<i64>)> {
    let pe = gex_ld::pe::parse(&std::fs::read(path).unwrap()).unwrap();
    let (img, extra) = pe_input(&pe, pe.preferred_base).unwrap();
    let prog = recover_pe(&img, &extra, &RecoverOptions::default());
    assert_eq!(gex_xlate::arch::compat(), img.class32);
    let sums = gex_xlate::summary::analyze(&prog);
    prog.functions.keys().map(|&a| (a, sums.get(a).sp_delta.map(|d| d as i64))).collect()
}

const PE32: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/pe/switch32.exe");
const PE64: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/pe/vtail64.exe");

#[test]
fn a_pe32_and_a_pe32_plus_image_recover_on_two_threads_at_once() {
    let want32 = shape(PE32);
    let want64 = shape(PE64);
    assert!(want32.iter().any(|&(_, d)| d == Some(12)), "{want32:x?}");
    let go = std::sync::Barrier::new(2);
    std::thread::scope(|s| {
        for t in 0..2 {
            let (go, w32, w64) = (&go, &want32, &want64);
            s.spawn(move || {
                go.wait();
                for i in 0..40 {
                    // Alternating, out of step with the other thread.
                    if (i + t) % 2 == 0 {
                        assert_eq!(&shape(PE32), w32, "thread {t} round {i}");
                    } else {
                        assert_eq!(&shape(PE64), w64, "thread {t} round {i}");
                    }
                }
            });
        }
    });
}
