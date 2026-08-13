//! A fused multiply-add built from the operations wasm has.
//!
//! wasm has no FMA, so guest `fmadd`/`fmsub` would otherwise call the shim's.
//! The unfused `a * b + c` rounds twice where an FMA rounds once, so it is
//! wrong and cannot replace the call; this uses Dekker's method, which is
//! exact until the final add. Exactness needs nothing to overflow or go
//! subnormal, so the fast path is guarded by an exponent band ([`SAFE_LO`],
//! [`SAFE_HI`], chosen by fuzzing: the band keeps `a * b` and the split's
//! `x * 2^27` clear of overflow and stays above the subnormal floor) and
//! anything outside it falls back to the call. The test below is the evidence;
//! re-run it if the constants change.

/// `2^27 + 1`, splits a double into halves that multiply exactly.
pub const SPLIT: f64 = 134_217_729.0;

/// Biased exponents the fast path accepts, roughly `2^-500` to `2^500`. Zero
/// is accepted too and is exact; everything else outside the band falls back.
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

/// The single-precision case needs neither Dekker nor a guard: the product of
/// two `f32` is exact in `f64`, so only the sum rounds. Round-to-odd on that
/// sum stops the narrowing to `f32` from breaking a halfway tie the wrong way
/// -- the step must go *toward the error's sign*, not simply up in magnitude.
pub fn fmaf_soft(a: f32, b: f32, c: f32) -> f32 {
    let (a, b, c) = (a as f64, b as f64, c as f64);
    let p = a * b;

    let s = p + c;
    let bb = s - p;
    let t = (p - (s - bb)) + (c - bb);

    let s = if t != 0.0 && s.to_bits() & 1 == 0 {
        let up = (t > 0.0) == (s > 0.0);
        f64::from_bits(if up { s.to_bits() + 1 } else { s.to_bits() - 1 })
    } else {
        s
    };
    s as f32
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
    /// Three populations: random bit patterns, values at the band's edges, and
    /// zeros, which the guard lets through by a special case.
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

    /// Single precision, unguarded, against the hardware. Random bit patterns,
    /// so subnormals, infinities and NaNs all appear.
    #[test]
    fn single_precision_is_exact_without_a_guard() {
        let mut st = 0x51ed_270b_0009_e377u64;
        let mut wrong = Vec::new();
        for _ in 0..3_000_000u64 {
            let r = xorshift(&mut st);
            let a = f32::from_bits(r as u32);
            let b = f32::from_bits((r >> 32) as u32);
            let c = f32::from_bits(xorshift(&mut st) as u32);
            let (want, got) = (a.mul_add(b, c), fmaf_soft(a, b, c));
            if want.to_bits() != got.to_bits() && !(want.is_nan() && got.is_nan()) && wrong.len() < 4
            {
                wrong.push(format!(
                    "fmaf({a:e}, {b:e}, {c:e}) want {:08x} got {:08x}",
                    want.to_bits(),
                    got.to_bits()
                ));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// The band is not so narrow that ordinary magnitudes miss the fast path.
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
