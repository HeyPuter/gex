//! A fused multiply-add built from the operations wasm has.
//!
//! # Why
//!
//! wasm has no FMA instruction, so every guest `fmadd`/`fmsub` leaves the
//! translated module and calls the shim's. That is not a small cost in
//! floating-point code: nbench's LU decomposition makes **706 million** of
//! those calls in a single kernel, and a build that replaces them with an
//! unfused multiply and add runs the same kernel at 362-418 iterations per
//! second against 60-76. Six times faster, in the browser, on the only
//! benchmark leg that matters.
//!
//! The unfused version is also wrong. `a * b + c` rounds twice where a fused
//! multiply-add rounds once, and the results differ -- so it cannot ship, and
//! the 6x is a measurement of the prize rather than a way to claim it.
//!
//! # The arithmetic
//!
//! Dekker's method. Splitting each operand into halves that multiply without
//! error gives `p + e == a * b` exactly, with no rounding anywhere. Folding
//! the three-term sum `p + e + c` with two-sums is exact for the same reason,
//! so the only rounding left is the final add -- which is what a fused
//! multiply-add is defined to do.
//!
//! # Why it needs a guard
//!
//! The exactness depends on nothing overflowing or falling into the
//! subnormals. Fuzzed against the hardware over random bit patterns, the bare
//! algorithm is wrong on about 7% of cases -- every one of them an extreme.
//! Over three million triples of ordinary magnitude it is wrong on none.
//!
//! So the fast path is guarded by an exponent band, and anything outside it
//! falls back to the call it was replacing. [`SAFE_LO`] and [`SAFE_HI`] were
//! chosen by fuzzing: the band has to keep `a * b` clear of overflow (twice
//! the exponent, so the band cannot simply be "finite"), keep the split's
//! `x * 2^27` clear of it too, and stay far enough above the subnormal floor
//! that the error terms remain normal.
//!
//! The test below is the evidence, and it is the thing to re-run if the
//! constants are ever touched.

/// `2^27 + 1`, the multiplier that splits a double into halves narrow enough
/// to multiply exactly.
pub const SPLIT: f64 = 134_217_729.0;

/// Biased exponents the fast path accepts, roughly `2^-500` to `2^500`.
///
/// Zero is accepted too, and is exact -- the algorithm carries it through
/// untouched. Everything else outside the band takes the fallback.
pub const SAFE_LO: u64 = 523;
pub const SAFE_HI: u64 = 1523;

/// Whether `x` is one the fast path handles exactly.
pub fn in_fast_range(x: f64) -> bool {
    let e = (x.to_bits() >> 52) & 0x7ff;
    (e == 0 && x == 0.0) || (e >= SAFE_LO && e <= SAFE_HI)
}

/// The reference the emitted wasm mirrors, kept in Rust so it can be fuzzed
/// against the hardware's own FMA.
pub fn fma_soft(a: f64, b: f64, c: f64) -> f64 {
    // Exact: p + e == a * b.
    let p = a * b;
    let ta = a * SPLIT;
    let ah = ta - (ta - a);
    let al = a - ah;
    let tb = b * SPLIT;
    let bh = tb - (tb - b);
    let bl = b - bh;
    let e = ((ah * bh - p) + ah * bl + al * bh) + al * bl;

    // Exact: s + t == p + c.
    let s = p + c;
    let bb = s - p;
    let t = (p - (s - bb)) + (c - bb);

    // The one rounding.
    s + (t + e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// Inside the guard, the software FMA is the hardware's, bit for bit.
    ///
    /// Three populations, because they fail differently: random bit patterns
    /// reach the extremes, values deliberately at the band's edges reach the
    /// place where the reasoning about overflow is thinnest, and zeros are
    /// let through the guard by a special case and have to be checked for it.
    #[test]
    fn the_guarded_fast_path_is_exact() {
        let mut st = 0xdead_beef_cafe_f00du64;
        let mut wrong = Vec::new();
        let mut check = |a: f64, b: f64, c: f64, wrong: &mut Vec<String>| {
            if !(in_fast_range(a) && in_fast_range(b) && in_fast_range(c)) {
                return false;
            }
            let (want, got) = (a.mul_add(b, c), fma_soft(a, b, c));
            if want.to_bits() != got.to_bits() && wrong.len() < 4 {
                wrong.push(format!(
                    "fma({a:e}, {b:e}, {c:e}) want {:016x} got {:016x}",
                    want.to_bits(),
                    got.to_bits()
                ));
            }
            true
        };

        let mut guarded = 0u64;
        for _ in 0..2_000_000u64 {
            let a = f64::from_bits(xorshift(&mut st));
            let b = f64::from_bits(xorshift(&mut st));
            let c = f64::from_bits(xorshift(&mut st));
            guarded += check(a, b, c, &mut wrong) as u64;
        }

        let at = |e: u64, m: u64| f64::from_bits((e << 52) | (m & 0xf_ffff_ffff_ffff));
        for &ea in &[SAFE_LO, SAFE_LO + 1, SAFE_HI - 1, SAFE_HI] {
            for &eb in &[SAFE_LO, SAFE_LO + 1, SAFE_HI - 1, SAFE_HI] {
                for &ec in &[SAFE_LO, SAFE_HI, 1023] {
                    for _ in 0..2_000 {
                        let a = at(ea, xorshift(&mut st));
                        let b = at(eb, xorshift(&mut st));
                        let c = at(ec, xorshift(&mut st));
                        guarded += check(a, b, c, &mut wrong) as u64;
                    }
                }
            }
        }

        for &z in &[0.0f64, -0.0f64] {
            for _ in 0..20_000 {
                let x = f64::from_bits(xorshift(&mut st));
                for (a, b, c) in [(z, x, x), (x, z, x), (x, x, z)] {
                    guarded += check(a, b, c, &mut wrong) as u64;
                }
            }
        }

        assert!(wrong.is_empty(), "{guarded} guarded cases, wrong: {wrong:#?}");
        assert!(guarded > 300_000, "the guard rejected almost everything: {guarded}");
    }

    /// And the band is not so narrow that real code misses it. Ordinary
    /// magnitudes must take the fast path essentially always, or the
    /// fallback is the path and none of this is worth doing.
    #[test]
    fn ordinary_magnitudes_take_the_fast_path() {
        let mut st = 0x243f_6a88_85a3_08d3u64;
        let mut fast = 0u64;
        const N: u64 = 200_000;
        for _ in 0..N {
            let m = |x: u64| (x as f64 / u64::MAX as f64) * 2000.0 - 1000.0;
            let a = m(xorshift(&mut st));
            let b = m(xorshift(&mut st));
            let c = m(xorshift(&mut st));
            if in_fast_range(a) && in_fast_range(b) && in_fast_range(c) {
                fast += 1;
            }
        }
        assert_eq!(fast, N, "only {fast} of {N} ordinary triples took the fast path");
    }
}
