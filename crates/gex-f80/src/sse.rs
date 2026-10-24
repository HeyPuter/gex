//! SSE's scalar floating point as MXCSR controls it: the rounding control
//! (bits 13-14, the x87's RC encoding), DAZ (bit 6: a denormal operand is
//! read as a zero of its sign) and FTZ (bit 15: a denormal result is
//! written as one). Results are single or double, rounded once into their
//! format, exponent range included; NaNs follow SSE's rules (the first
//! operand's if it is one, else the second's, quieted; an invalid operation
//! gives the default NaN, negative and quiet).
//!
//! One function, [`op`], as `gex_ir::Expr::Mx`'s slow path calls it: the
//! operation as a code, the operands and result as raw bits.
//! Exception flags are not reported.

use super::*;

/// MXCSR's DAZ bit.
pub const DAZ: u32 = 1 << 6;
/// MXCSR's FTZ bit.
pub const FTZ: u32 = 1 << 15;

/// The operations of [`op`], in its code's low four bits; [`DOUBLE`] says
/// the width of the operation (for the conversions, of the result).
pub const ADD: u32 = 0;
pub const SUB: u32 = 1;
pub const MUL: u32 = 2;
pub const DIV: u32 = 3;
pub const SQRT: u32 = 4;
/// Round to an integral value by the rounding control (`roundsd` with
/// imm bit 2, and a conversion to integer's rounding).
pub const RINT: u32 = 5;
/// A signed 32-bit integer to a float.
pub const FROM_I32: u32 = 6;
/// A signed 64-bit integer to a float.
pub const FROM_I64: u32 = 7;
/// Double to single (`cvtsd2ss`); [`DOUBLE`] is not set.
pub const NARROW: u32 = 8;
/// Single to double (`cvtss2sd`); [`DOUBLE`] is set.
pub const WIDEN: u32 = 9;
/// The operation is on doubles.
pub const DOUBLE: u32 = 1 << 4;

fn fmt_of(double: bool) -> (Fmt, Mem) {
    if double {
        (DBL, Mem::F64)
    } else {
        (SGL, Mem::F32)
    }
}

/// The exponent field and fraction of `raw` in `m`'s format, and its sign
/// bit's value.
fn fields(raw: u64, m: Mem) -> (u64, u64, u64) {
    match m {
        Mem::F32 => ((raw >> 23) & 0xff, raw & 0x7f_ffff, raw & 0x8000_0000),
        _ => ((raw >> 52) & 0x7ff, raw & 0xf_ffff_ffff_ffff, raw & 0x8000_0000_0000_0000),
    }
}

/// `raw`, a denormal of `m`'s format flushed to a zero of its sign.
fn flush(raw: u64, m: Mem) -> u64 {
    let (e, f, s) = fields(raw, m);
    if e == 0 && f != 0 {
        s
    } else {
        raw
    }
}

fn is_nan(raw: u64, m: Mem) -> bool {
    let (e, f, _) = fields(raw, m);
    let all = if m == Mem::F32 { 0xff } else { 0x7ff };
    e == all && f != 0
}

fn quieted(raw: u64, m: Mem) -> u64 {
    raw | if m == Mem::F32 { 1 << 22 } else { 1 << 51 }
}

/// `code` (one of the operations above, with [`DOUBLE`]) on `a` and `b`
/// (raw bits; `b` unused by the one-operand operations, a float operand
/// zero-extended, an integer one as it is) under the MXCSR value `mxcsr`.
/// The result's raw bits, zero-extended.
pub fn op(code: u32, a: u64, b: u64, mxcsr: u32) -> u64 {
    let double = code & DOUBLE != 0;
    let mut env = Env::from_cw(0x37f);
    env.round = match (mxcsr >> 13) & 3 {
        0 => Round::Nearest,
        1 => Round::Down,
        2 => Round::Up,
        _ => Round::Zero,
    };
    let daz = |raw: u64, m: Mem| if mxcsr & DAZ != 0 { flush(raw, m) } else { raw };
    // FTZ writes a tiny result (after rounding, exponent unbounded: one
    // that rounds to the smallest normal only in the format's own denormals
    // is tiny) as a zero of its sign, exact or not. The operation's own
    // rounding says so, or the store's when the operation passed an
    // operand through unrounded.
    let ftz = |env: &mut Env, r: F80, m: Mem| {
        let t = env.tiny;
        env.tiny = false;
        let raw = store(env, r, m, false);
        if mxcsr & FTZ != 0 && (t || env.tiny) {
            fields(raw, m).2
        } else {
            raw
        }
    };
    match code & 0xf {
        k @ (ADD | SUB | MUL | DIV | SQRT | RINT) => {
            let (f, m) = fmt_of(double);
            let (a, b) = (daz(a, m), daz(b, m));
            let unary = k >= SQRT;
            if is_nan(a, m) {
                return quieted(a, m);
            }
            if !unary && is_nan(b, m) {
                return quieted(b, m);
            }
            let fa = load(&mut env, a, m);
            let fb = load(&mut env, b, m);
            env.tiny = false;
            env.fmt = f;
            env.prec = f.bits;
            let r = match k {
                ADD => add(&mut env, fa, fb, false),
                SUB => add(&mut env, fa, fb, true),
                MUL => mul(&mut env, fa, fb),
                DIV => div(&mut env, fa, fb),
                SQRT => sqrt(&mut env, fa),
                _ => round_int(&mut env, fa, false),
            };
            env.fmt = EXT;
            ftz(&mut env, r, m)
        }
        k @ (FROM_I32 | FROM_I64) => {
            let v = load(&mut env, a, if k == FROM_I32 { Mem::I32 } else { Mem::I64 });
            store(&mut env, v, fmt_of(double).1, false)
        }
        NARROW => {
            let a = daz(a, Mem::F64);
            let v = load(&mut env, a, Mem::F64);
            env.tiny = false;
            ftz(&mut env, v, Mem::F32)
        }
        WIDEN => {
            let a = daz(a & 0xffff_ffff, Mem::F32);
            let v = load(&mut env, a, Mem::F32);
            store(&mut env, v, Mem::F64, false)
        }
        _ => panic!("sse::op: unknown operation {code:#x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RN: u32 = 0x1f80;
    const RD: u32 = 0x3f80;
    const RU: u32 = 0x5f80;
    const RZ: u32 = 0x7f80;

    fn d(code: u32, a: f64, b: f64, mx: u32) -> f64 {
        f64::from_bits(op(code | DOUBLE, a.to_bits(), b.to_bits(), mx))
    }
    fn s(code: u32, a: f32, b: f32, mx: u32) -> f32 {
        f32::from_bits(op(code, a.to_bits() as u64, b.to_bits() as u64, mx) as u32)
    }

    /// To nearest, every operation is the host's.
    #[test]
    fn nearest_is_ieee() {
        let vals = [1.0, 1.0 / 3.0, -2.0 / 7.0, 1e300, -1e-300, 4.9e-324, 2.2250738585072014e-308, 0.0, -0.0, 1e16 + 3.0];
        for &a in &vals {
            for &b in &vals {
                assert_eq!(d(ADD, a, b, RN).to_bits(), (a + b).to_bits(), "{a} + {b}");
                assert_eq!(d(SUB, a, b, RN).to_bits(), (a - b).to_bits(), "{a} - {b}");
                assert_eq!(d(MUL, a, b, RN).to_bits(), (a * b).to_bits(), "{a} * {b}");
                if !(a == 0.0 && b == 0.0) {
                    assert_eq!(d(DIV, a, b, RN).to_bits(), (a / b).to_bits(), "{a} / {b}");
                }
                let (fa, fb) = (a as f32, b as f32);
                assert_eq!(s(ADD, fa, fb, RN).to_bits(), (fa + fb).to_bits(), "{fa} + {fb}");
                assert_eq!(s(MUL, fa, fb, RN).to_bits(), (fa * fb).to_bits(), "{fa} * {fb}");
                if !(fa == 0.0 && fb == 0.0) {
                    assert_eq!(s(DIV, fa, fb, RN).to_bits(), (fa / fb).to_bits(), "{fa} / {fb}");
                }
            }
            if a >= 0.0 {
                assert_eq!(d(SQRT, a, 0.0, RN).to_bits(), a.sqrt().to_bits(), "sqrt {a}");
            }
            assert_eq!(op(NARROW, a.to_bits(), 0, RN) as u32, (a as f32).to_bits(), "narrow {a}");
        }
    }

    /// Directed rounding brackets the exact result.
    #[test]
    fn directed() {
        assert_eq!(d(DIV, 1.0, 3.0, RD), 0.3333333333333333);
        assert_eq!(d(DIV, 1.0, 3.0, RU), 0.33333333333333337);
        assert_eq!(d(DIV, -1.0, 3.0, RZ), -0.3333333333333333);
        assert_eq!(d(DIV, -1.0, 3.0, RD), -0.33333333333333337);
        assert_eq!(d(ADD, 1.7e308, 1.7e308, RZ), f64::MAX);
        assert_eq!(d(ADD, 1.7e308, 1.7e308, RN), f64::INFINITY);
        assert_eq!(d(MUL, 1e300, 1e300, RD), f64::MAX);
        assert_eq!(d(MUL, -1e300, 1e300, RD), f64::NEG_INFINITY);
        assert_eq!(d(SUB, 1.0, 1.0, RD).to_bits(), (-0.0f64).to_bits());
        assert_eq!(s(ADD, 1.0, 1e-30, RU), 1.0000001);
        assert_eq!(op(NARROW, (1.0f64 / 3.0).to_bits(), 0, RZ) as u32, 0.3333333f32.to_bits());
        assert_eq!(op(NARROW, (1.0f64 / 3.0).to_bits(), 0, RU) as u32, 0.33333334f32.to_bits());
        assert_eq!(op(FROM_I64 | DOUBLE, 9007199254740993, 0, RU), 9007199254740994.0f64.to_bits());
        assert_eq!(op(FROM_I64 | DOUBLE, 9007199254740993, 0, RD), 9007199254740992.0f64.to_bits());
        assert_eq!(op(FROM_I32, 16777217, 0, RU) as u32, 16777218.0f32.to_bits());
        assert_eq!(d(RINT, 2.5, 0.0, RN), 2.0);
        assert_eq!(d(RINT, 2.5, 0.0, RU), 3.0);
        assert_eq!(d(RINT, -2.5, 0.0, RZ), -2.0);
        // A denormal result, rounded once into the format's own denormals.
        assert_eq!(d(MUL, 3.0e-310, 0.5, RU), f64::from_bits(3.0e-310f64.to_bits() / 2 + 1));
    }

    #[test]
    fn ftz_daz() {
        let den = 3.0e-310;
        assert_eq!(d(ADD, den, 0.0, RN | DAZ).to_bits(), 0);
        assert_eq!(d(ADD, -den, 0.0, RN | DAZ).to_bits(), 0);
        assert_eq!(d(MUL, -den, 1.0, RN | DAZ).to_bits(), (-0.0f64).to_bits());
        assert_eq!(d(ADD, den, 0.0, RN | FTZ).to_bits(), 0);
        assert_eq!(d(MUL, 1e-300, 1e-10, RN | FTZ).to_bits(), 0);
        assert_eq!(d(MUL, -1e-300, 1e-10, RN | FTZ).to_bits(), (-0.0f64).to_bits());
        assert_eq!(d(ADD, den, den, RN).to_bits(), (den + den).to_bits());
        assert_eq!(op(NARROW, 1e-39f64.to_bits(), 0, RN | FTZ), 0);
        assert_eq!(op(WIDEN | DOUBLE, 1e-39f32.to_bits() as u64, 0, RN | DAZ), 0);
        // Rounds to the smallest normal only in the format's denormals: tiny.
        let min = f64::MIN_POSITIVE;
        assert_eq!(d(MUL, f64::from_bits(0x3fef_ffff_ffff_ffff), min, RN | FTZ).to_bits(), 0);
        assert_eq!(d(MUL, f64::from_bits(0x3fef_ffff_ffff_ffff), min, RN).to_bits(), min.to_bits());
        // Rounds to it with the exponent unbounded too: not tiny.
        let below = d(MUL, f64::from_bits(0x3ff0_0000_0000_0001), f64::from_bits(0x000f_ffff_ffff_ffff), RN | FTZ);
        assert_eq!(below, min);
        assert_eq!(s(MUL, 0.99999994, f32::MIN_POSITIVE, RN | FTZ).to_bits(), 0);
    }

    #[test]
    fn nans() {
        let qa = f64::from_bits(0x7ff8_0000_0000_0001);
        let sb = f64::from_bits(0x7ff0_0000_0000_0002);
        assert_eq!(op(ADD | DOUBLE, qa.to_bits(), sb.to_bits(), RZ), 0x7ff8_0000_0000_0001);
        assert_eq!(op(ADD | DOUBLE, 1f64.to_bits(), sb.to_bits(), RZ), 0x7ff8_0000_0000_0002);
        assert_eq!(op(SUB | DOUBLE, f64::INFINITY.to_bits(), f64::INFINITY.to_bits(), RZ), 0xfff8_0000_0000_0000);
        assert_eq!(op(SQRT, (-1f32).to_bits() as u64, 0, RU), 0xffc0_0000);
    }
}
