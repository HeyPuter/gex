//! The "AOT share" line of `GEX_WIN_STATS`: where the
//! process's CPU samples land, by the kind of code they hit.
//!
//! Every compiled module is registered here with its code range and kind
//! when it is published (`note`). A sampler (gexc's `GEX_PROF` SIGPROF
//! profiler, `set_sampler`) hands over the PCs it collected since the last
//! report; `line` classifies them. Guest code is units (AOT, clang), export
//! providers, the function JIT, tier-1, and the ftab runtime module (bridge,
//! resolve, root); the rest is the host.

use std::sync::{Mutex, OnceLock, RwLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Unit,
    Provider,
    Jit,
    Tier1,
    Runtime,
    /// Modules a provider's own runtime JIT compiled (the .NET Mono
    /// provider's wasm JIT, shims/native/src/wasmjit.rs).
    Managed,
}

const KINDS: [(Kind, &str); 6] = [
    (Kind::Unit, "units"),
    (Kind::Provider, "providers"),
    (Kind::Jit, "jit"),
    (Kind::Tier1, "tier-1"),
    (Kind::Runtime, "runtime"),
    (Kind::Managed, "provider-jit"),
];

static RANGES: RwLock<Vec<(usize, usize, Kind)>> = RwLock::new(Vec::new());

type Sampler = Box<dyn Fn() -> Vec<u64> + Send + Sync>;
static SAMPLER: OnceLock<Sampler> = OnceLock::new();
/// Totals since the start, by kind (and host last).
static TOTALS: Mutex<[u64; 7]> = Mutex::new([0; 7]);

/// The source of samples: PCs collected since the previous call.
pub fn set_sampler(f: Sampler) {
    let _ = SAMPLER.set(f);
}

/// Register a module's compiled code.
pub fn note(module: &wasmtime::Module, kind: Kind) {
    let r = module.image_range();
    let (lo, hi) = (r.start as usize, r.end as usize);
    if lo < hi {
        let mut v = RANGES.write().unwrap_or_else(|e| e.into_inner());
        let at = v.partition_point(|x| x.0 < lo);
        // The first registration of a module's code wins (a provider is
        // noted as one before ftab publishes it as a unit).
        if v.get(at).is_some_and(|x| x.0 == lo) {
            return;
        }
        v.insert(at, (lo, hi, kind));
    }
}

fn classify(pc: usize, v: &[(usize, usize, Kind)]) -> Option<Kind> {
    let i = v.partition_point(|x| x.0 <= pc);
    (i > 0 && pc < v[i - 1].1).then(|| v[i - 1].2)
}

/// The line, for the samples since the last call, or `None` without a
/// sampler or samples. The share is of guest samples (everything but the
/// host), AOT meaning units and providers.
pub fn line() -> Option<String> {
    let samples = SAMPLER.get()?();
    if samples.is_empty() {
        return None;
    }
    let mut n = [0u64; 7];
    {
        let v = RANGES.read().unwrap_or_else(|e| e.into_inner());
        for pc in &samples {
            match classify(*pc as usize, &v) {
                Some(k) => n[KINDS.iter().position(|x| x.0 == k).unwrap()] += 1,
                None => n[6] += 1,
            }
        }
    }
    let mut tot = TOTALS.lock().unwrap_or_else(|e| e.into_inner());
    for i in 0..7 {
        tot[i] += n[i];
    }
    let fmt = |n: &[u64; 7]| {
        let guest: u64 = n[..6].iter().sum();
        let all = guest + n[6];
        let pct = |x: u64, of: u64| if of == 0 { 0.0 } else { 100.0 * x as f64 / of as f64 };
        let parts: Vec<String> = KINDS.iter().enumerate().map(|(i, (_, name))| format!("{name} {:.1}%", pct(n[i], guest))).collect();
        format!(
            "AOT {:.1}% of guest ({}); host {:.1}% of all; {all} samples",
            pct(n[0] + n[1], guest),
            parts.join(" "),
            pct(n[6], all)
        )
    };
    Some(format!("{} | since start: {}", fmt(&n), fmt(&tot)))
}
