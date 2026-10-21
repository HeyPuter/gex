//! How many processors the guest is told it has.
//!
//! Guest threads run in parallel: native threads on the host, Workers in the
//! page. So the guest gets the host's count, which is what sizes an engine's job
//! pool, a GC's markers, or `make -j`. Told one processor, a program runs its
//! whole job system on one worker and leaves the other cores idle.
//!
//! On the host the count comes from `std::thread::available_parallelism`, or
//! from `GEX_CPUS` when that is set. The page has no such call in wasm, so its
//! runtime passes `navigator.hardwareConcurrency` through [`set`]. Until it
//! does, the count is 1. The count is at most [`MAX`], so the affinity mask
//! fits in one word.
//!
//! Every place that reports processors asks [`count`]: `sched_getaffinity`
//! (musl's `sysconf(_SC_NPROCESSORS_ONLN)`), `/proc/stat`'s cpu lines (glibc's
//! fallback when there is no sysfs, and Boehm GC's count) and `/proc/cpuinfo`.
use std::sync::atomic::{AtomicU32, Ordering};

/// The most processors reported: one 64-bit affinity word.
pub const MAX: u32 = 64;

/// 0 means not set yet.
static SET: AtomicU32 = AtomicU32::new(0);

/// Report `n` processors from now on, clamped to `1..=MAX`. The page sets it
/// from `navigator.hardwareConcurrency`.
pub fn set(n: u32) {
    SET.store(n.clamp(1, MAX), Ordering::Relaxed);
}

/// The processor count the guest sees.
pub fn count() -> u32 {
    let n = SET.load(Ordering::Relaxed);
    if n != 0 {
        return n;
    }
    let n = host().clamp(1, MAX);
    SET.store(n, Ordering::Relaxed);
    n
}

#[cfg(not(target_family = "wasm"))]
fn host() -> u32 {
    if let Some(n) = std::env::var("GEX_CPUS").ok().and_then(|v| v.trim().parse::<u32>().ok()) {
        return n;
    }
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}

#[cfg(target_family = "wasm")]
fn host() -> u32 {
    1
}

/// The affinity mask: one bit per processor.
pub fn mask() -> u64 {
    let n = count();
    if n >= 64 { u64::MAX } else { (1u64 << n) - 1 }
}

/// `/proc/stat`: an idle aggregate line and one per processor, then the rest.
pub fn proc_stat() -> String {
    let n = count();
    let mut s = format!("cpu  0 0 0 {} 0 0 0 0 0 0\n", 100000 * n as u64);
    for i in 0..n {
        s.push_str(&format!("cpu{i} 0 0 0 100000 0 0 0 0 0 0\n"));
    }
    s.push_str("intr 0\nctxt 0\nbtime 1700000000\nprocesses 1\nprocs_running 1\nprocs_blocked 0\n");
    s
}

/// `/proc/cpuinfo`: `block` (one processor's entry, starting with its
/// `processor\t: 0` line) once per processor, renumbered. In the x86 entry,
/// `siblings` and `cpu cores` give the count too, and each processor is its own core.
pub fn proc_cpuinfo(block: &str) -> String {
    let n = count();
    let mut s = String::new();
    for i in 0..n {
        let mut b = block.replacen("processor\t: 0\n", &format!("processor\t: {i}\n"), 1);
        b = b.replacen("siblings\t: 1\n", &format!("siblings\t: {n}\n"), 1);
        b = b.replacen("core id\t\t: 0\n", &format!("core id\t\t: {i}\n"), 1);
        b = b.replacen("cpu cores\t: 1\n", &format!("cpu cores\t: {n}\n"), 1);
        s.push_str(&b);
    }
    s
}

/// Tests that set the count hold this, since the count is process-wide.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_report_agrees() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set(6);
        assert_eq!(count(), 6);
        assert_eq!(mask(), 0b11_1111);
        let stat = proc_stat();
        assert_eq!(stat.lines().filter(|l| l.starts_with("cpu") && !l.starts_with("cpu ")).count(), 6);
        let info = proc_cpuinfo(crate::x86::CPUINFO);
        assert_eq!(info.matches("processor\t:").count(), 6);
        assert!(info.contains("processor\t: 5\n") && info.contains("cpu cores\t: 6\n"));
        set(1000);
        assert_eq!(count(), MAX);
        assert_eq!(mask(), u64::MAX);
        set(0);
        assert_eq!(count(), 1);
    }
}
