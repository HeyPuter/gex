//! x87 80-bit extended-precision arithmetic, exact.
//!
//! Every operation the x87 rounds is computed here on integers, bit for bit
//! as the hardware does it: the precision-control field of the control word
//! (24, 53 or 64 significand bits, over the full extended exponent range),
//! the four rounding modes, denormals, the exception flags of the status
//! word, `C1` as the round-up indicator, the x87's NaN propagation rules
//! and its unsupported encodings (unnormals, pseudo-NaNs, pseudo-infinities).
//! The tests check all of it against the host's own x87.
//!
//! A value is `(-1)^s × sig × 2^(e - 63)` for a finite operand, `sig` holding
//! the explicit integer bit at bit 63. Results are rounded from a 128-bit
//! significand whose bit 0 is sticky: `(-1)^s × sig × 2^(e - 127)`.

#![forbid(unsafe_code)]

/// Status-word exception flags, in their status-word bit positions (and the
/// control word's mask bits, which sit at the same positions).
pub const IE: u16 = 1 << 0;
pub const DE: u16 = 1 << 1;
pub const ZE: u16 = 1 << 2;
pub const OE: u16 = 1 << 3;
pub const UE: u16 = 1 << 4;
pub const PE: u16 = 1 << 5;
/// Stack fault: with `IE`, an operand register was empty (C1 clear) or a
/// push found the register full (C1 set).
pub const SF: u16 = 1 << 6;

/// An 80-bit extended-precision value as it is stored: the significand with
/// its explicit integer bit, and the sign and 15-bit biased exponent.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct F80 {
    pub sig: u64,
    pub se: u16,
}

impl F80 {
    /// The "real indefinite" QNaN every masked invalid operation returns.
    pub const INDEFINITE: F80 = F80 {
        sig: 0xc000_0000_0000_0000,
        se: 0xffff,
    };
    pub const ZERO: F80 = F80 { sig: 0, se: 0 };
    pub const ONE: F80 = F80 {
        sig: 1 << 63,
        se: 0x3fff,
    };

    pub fn sign(self) -> bool {
        self.se & 0x8000 != 0
    }

    fn exp(self) -> u16 {
        self.se & 0x7fff
    }

    fn with_sign(self, s: bool) -> F80 {
        F80 {
            sig: self.sig,
            se: (self.se & 0x7fff) | if s { 0x8000 } else { 0 },
        }
    }

    fn inf(s: bool) -> F80 {
        F80 { sig: 1 << 63, se: 0x7fff }.with_sign(s)
    }

    fn zero(s: bool) -> F80 {
        F80::ZERO.with_sign(s)
    }

    fn quiet(self) -> F80 {
        F80 {
            sig: self.sig | 1 << 62,
            se: self.se,
        }
    }

    pub fn from_bytes(b: [u8; 10]) -> F80 {
        F80 {
            sig: u64::from_le_bytes(b[..8].try_into().unwrap()),
            se: u16::from_le_bytes([b[8], b[9]]),
        }
    }

    pub fn to_bytes(self) -> [u8; 10] {
        let mut b = [0u8; 10];
        b[..8].copy_from_slice(&self.sig.to_le_bytes());
        b[8..].copy_from_slice(&self.se.to_le_bytes());
        b
    }
}

/// Rounding control, the control word's RC field.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Round {
    Nearest,
    Down,
    Up,
    Zero,
}

/// The state an operation reads and reports: rounding and precision from
/// the control word, and the exception flags and `C1` it raises.
#[derive(Copy, Clone, Debug)]
pub struct Env {
    pub round: Round,
    /// Significand bits the precision-control field allows: 24, 53 or 64.
    pub prec: u32,
    /// Exceptions raised, as status-word bits.
    pub flags: u16,
    /// `C1`: set when the result was rounded up in magnitude.
    pub c1: bool,
}

impl Env {
    pub fn from_cw(cw: u16) -> Env {
        Env {
            round: match (cw >> 10) & 3 {
                0 => Round::Nearest,
                1 => Round::Down,
                2 => Round::Up,
                _ => Round::Zero,
            },
            // PC 01 is reserved; the hardware rounds to 64 bits then (the
            // tests draw it too).
            prec: match (cw >> 8) & 3 {
                0 => 24,
                2 => 53,
                _ => 64,
            },
            flags: 0,
            c1: false,
        }
    }
}

/// A floating-point format results are rounded into.
#[derive(Copy, Clone)]
struct Fmt {
    /// Significand bits, integer bit included.
    bits: u32,
    /// Exponent of the smallest normal number.
    emin: i32,
    /// Exponent of the largest finite number (also the bias).
    emax: i32,
}

const EXT: Fmt = Fmt {
    bits: 64,
    emin: -16382,
    emax: 16383,
};
const DBL: Fmt = Fmt {
    bits: 53,
    emin: -1022,
    emax: 1023,
};
const SGL: Fmt = Fmt {
    bits: 24,
    emin: -126,
    emax: 127,
};

/// What an operand is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Class {
    Zero,
    /// `sig` normalised (bit 63 set); denormals are normalised on the way in.
    Finite { e: i32, sig: u64 },
    Inf,
    QNaN,
    SNaN,
    /// Unnormals, pseudo-NaNs and pseudo-infinities: the x87 treats them as
    /// invalid operands.
    Unsupported,
}

/// Classify `x`, and report whether it is a denormal (pseudo-denormals
/// included).
fn classify(x: F80) -> (Class, bool) {
    let exp = x.exp();
    let j = x.sig >> 63 != 0;
    match exp {
        0 if x.sig == 0 => (Class::Zero, false),
        0 => {
            // A pseudo-denormal (J set) has the same value as the denormal
            // encoding would: sig × 2^(-16382 - 63).
            let lz = x.sig.leading_zeros() as i32;
            (
                Class::Finite {
                    e: -16382 - lz,
                    sig: x.sig << lz,
                },
                true,
            )
        }
        0x7fff if !j => (Class::Unsupported, false),
        0x7fff if x.sig << 1 == 0 => (Class::Inf, false),
        0x7fff if x.sig & 1 << 62 != 0 => (Class::QNaN, false),
        0x7fff => (Class::SNaN, false),
        _ if !j => (Class::Unsupported, false),
        _ => (
            Class::Finite {
                e: exp as i32 - 16383,
                sig: x.sig,
            },
            false,
        ),
    }
}

fn is_nan(c: Class) -> bool {
    matches!(c, Class::QNaN | Class::SNaN)
}

/// Shift right, folding every bit shifted out into bit 0.
fn shr_jam(v: u128, n: u32) -> u128 {
    if n == 0 {
        v
    } else if n >= 128 {
        (v != 0) as u128
    } else {
        (v >> n) | ((v & ((1u128 << n) - 1)) != 0) as u128
    }
}

/// Round `(-1)^sign × sig × 2^(e - 127)` (`sig` normalised, bit 0 sticky) to
/// `prec` significand bits in format `f`, and return the biased exponent and
/// the significand with the integer bit at `f.bits - 1` (0 exponent for
/// denormals and zero, all-ones for infinity). Raises PE, UE, OE and C1.
fn round_pack(env: &mut Env, sign: bool, e: i32, sig: u128, prec: u32, f: Fmt) -> (u32, u64) {
    debug_assert!(sig >> 127 == 1);
    // The weight of the last kept bit: `prec` bits below the leading one,
    // but never below `prec` bits under the smallest normal. (With precision
    // control narrower than the format, the x87 denormalises at that
    // precision too, not at the format's own denormal limit.)
    let min_lsb = f.emin - (prec as i32 - 1);
    let den_lsb = f.emin - (f.bits as i32 - 1);
    let lsb = (e - (prec as i32 - 1)).max(min_lsb);
    // Tininess is detected after rounding: a result that rounds up to the
    // smallest normal (at `prec` bits, exponent unbounded) is not tiny.
    let tiny = e < f.emin && !(e == f.emin - 1 && rounds_to_carry(env, sign, sig, prec));
    // Bits of `sig` below the kept part.
    let drop = (127 - (e - lsb)) as u32;
    let (kept, rem, half) = if drop >= 129 {
        (0u128, 1u128, 2u128) // far below half an lsb, but nonzero
    } else if drop == 128 {
        (0, sig, 1u128 << 127)
    } else {
        (sig >> drop, sig & ((1u128 << drop) - 1), 1u128 << (drop - 1))
    };
    let inexact = rem != 0;
    let up = match env.round {
        Round::Nearest => rem > half || (rem == half && kept & 1 == 1),
        Round::Zero => false,
        Round::Up => !sign && inexact,
        Round::Down => sign && inexact,
    };
    let kept = kept + up as u128;
    if inexact {
        env.flags |= PE;
        if tiny {
            env.flags |= UE;
        }
    }
    env.c1 = up;
    if kept == 0 {
        return (0, 0);
    }
    // The rounded value's leading-bit exponent.
    let er = lsb + (127 - kept.leading_zeros() as i32);
    if er > f.emax {
        env.flags |= OE | PE;
        let to_inf = match env.round {
            Round::Nearest => true,
            Round::Zero => false,
            Round::Up => !sign,
            Round::Down => sign,
        };
        env.c1 = to_inf;
        if to_inf {
            return ((2 * f.emax + 1) as u32, 1 << (f.bits - 1));
        }
        let ones = (u64::MAX >> (64 - prec)) << (f.bits - prec);
        return ((f.emax + f.emax) as u32, ones);
    }
    if er < f.emin {
        // Denormal: the integer bit's weight is 2^emin with exponent zero.
        return (0, (kept << (lsb - den_lsb)) as u64);
    }
    let shift = (f.bits as i32 - 1) - (er - lsb);
    let mant = if shift >= 0 { kept << shift } else { kept >> -shift };
    ((er + f.emax) as u32, mant as u64)
}

/// Whether rounding the normalised `sig` to `prec` bits carries out of the
/// top (every kept bit is one and the rounding goes up).
fn rounds_to_carry(env: &Env, sign: bool, sig: u128, prec: u32) -> bool {
    let drop = 128 - prec;
    let kept = sig >> drop;
    if kept != (1u128 << prec) - 1 {
        return false;
    }
    let rem = sig & ((1u128 << drop) - 1);
    let half = 1u128 << (drop - 1);
    match env.round {
        Round::Nearest => rem >= half, // kept is odd
        Round::Zero => false,
        Round::Up => !sign && rem != 0,
        Round::Down => sign && rem != 0,
    }
}

/// Round to an extended-format result at precision `prec`.
fn round_ext(env: &mut Env, sign: bool, e: i32, sig: u128, prec: u32) -> F80 {
    let (be, m) = round_pack(env, sign, e, sig, prec, EXT);
    F80 {
        sig: m,
        se: be as u16 | if sign { 0x8000 } else { 0 },
    }
}

/// The x87's choice between NaN operands (Intel SDM vol. 1, table 4-7): an
/// SNaN raises IE; a QNaN beats an SNaN; between two of a kind the larger
/// significand wins. The result is always quiet.
fn propagate(env: &mut Env, a: F80, ca: Class, b: F80, cb: Class) -> F80 {
    if ca == Class::SNaN || cb == Class::SNaN {
        env.flags |= IE;
    }
    let r = match (is_nan(ca), is_nan(cb)) {
        (true, true) => {
            if ca != cb {
                if ca == Class::QNaN {
                    a
                } else {
                    b
                }
            } else if (a.sig | 1 << 62) > (b.sig | 1 << 62) {
                a
            } else if (a.sig | 1 << 62) < (b.sig | 1 << 62) {
                b
            } else if !a.sign() {
                // Equal significands: the positive one.
                a
            } else {
                b
            }
        }
        (true, false) => a,
        _ => b,
    };
    r.quiet()
}

/// Classify two operands of an arithmetic operation, handling the operands
/// that decide the result on their own: unsupported encodings and NaNs.
/// Otherwise raise DE for a denormal operand and return the classes.
///
/// DE is left to the caller (`denormal`): an invalid operation or a
/// division by zero reports only that, not DE as well.
fn operands(env: &mut Env, a: F80, b: F80) -> Result<(Class, Class, bool), F80> {
    let (ca, da) = classify(a);
    let (cb, db) = classify(b);
    if ca == Class::Unsupported || cb == Class::Unsupported {
        env.flags |= IE;
        return Err(F80::INDEFINITE);
    }
    if is_nan(ca) || is_nan(cb) {
        return Err(propagate(env, a, ca, b, cb));
    }
    Ok((ca, cb, da || db))
}

fn invalid(env: &mut Env) -> F80 {
    env.flags |= IE;
    F80::INDEFINITE
}

/// `a + b`, or `a - b` for `sub`.
pub fn add(env: &mut Env, a: F80, b: F80, sub: bool) -> F80 {
    env.c1 = false;
    let (ca, cb, den) = match operands(env, a, b) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let sa = a.sign();
    let sb = b.sign() ^ sub;
    if den {
        env.flags |= DE;
    }
    match (ca, cb) {
        (Class::Inf, Class::Inf) if sa != sb => invalid(env),
        (Class::Inf, _) => F80::inf(sa),
        (_, Class::Inf) => F80::inf(sb),
        (Class::Zero, Class::Zero) => F80::zero(if sa == sb { sa } else { env.round == Round::Down }),
        (Class::Zero, Class::Finite { e, sig }) => round_ext(env, sb, e, (sig as u128) << 64, env.prec),
        (Class::Finite { e, sig }, Class::Zero) => round_ext(env, sa, e, (sig as u128) << 64, env.prec),
        (Class::Finite { e: ea, sig: ma }, Class::Finite { e: eb, sig: mb }) => {
            // Order by magnitude.
            let ((ea, ma, sa), (eb, mb, sb)) = if (ea, ma) >= (eb, mb) {
                ((ea, ma, sa), (eb, mb, sb))
            } else {
                ((eb, mb, sb), (ea, ma, sa))
            };
            // Leading bits at bit 126, so a carry has room.
            let x = (ma as u128) << 63;
            let y = shr_jam((mb as u128) << 63, (ea - eb) as u32);
            if sa == sb {
                let s = x + y;
                if s >> 127 == 1 {
                    round_ext(env, sa, ea + 1, s, env.prec)
                } else {
                    round_ext(env, sa, ea, s << 1, env.prec)
                }
            } else {
                let d = x - y;
                if d == 0 {
                    return F80::zero(env.round == Round::Down);
                }
                let lz = d.leading_zeros();
                round_ext(env, sa, ea + 1 - lz as i32, d << lz, env.prec)
            }
        }
        _ => unreachable!(),
    }
}

pub fn mul(env: &mut Env, a: F80, b: F80) -> F80 {
    env.c1 = false;
    let (ca, cb, den) = match operands(env, a, b) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let s = a.sign() ^ b.sign();
    if den {
        env.flags |= DE;
    }
    match (ca, cb) {
        (Class::Inf, Class::Zero) | (Class::Zero, Class::Inf) => invalid(env),
        (Class::Inf, _) | (_, Class::Inf) => F80::inf(s),
        (Class::Zero, _) | (_, Class::Zero) => F80::zero(s),
        (Class::Finite { e: ea, sig: ma }, Class::Finite { e: eb, sig: mb }) => {
            let p = ma as u128 * mb as u128;
            if p >> 127 == 1 {
                round_ext(env, s, ea + eb + 1, p, env.prec)
            } else {
                round_ext(env, s, ea + eb, p << 1, env.prec)
            }
        }
        _ => unreachable!(),
    }
}

pub fn div(env: &mut Env, a: F80, b: F80) -> F80 {
    env.c1 = false;
    let (ca, cb, den) = match operands(env, a, b) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let s = a.sign() ^ b.sign();
    if den && cb != Class::Zero {
        env.flags |= DE;
    }
    match (ca, cb) {
        (Class::Inf, Class::Inf) | (Class::Zero, Class::Zero) => invalid(env),
        (Class::Inf, _) => F80::inf(s),
        (_, Class::Inf) => F80::zero(s),
        (Class::Zero, _) => F80::zero(s),
        (_, Class::Zero) => {
            env.flags |= ZE;
            F80::inf(s)
        }
        (Class::Finite { e: ea, sig: ma }, Class::Finite { e: eb, sig: mb }) => {
            // A 64-bit quotient with its leading bit at 63, then 64 more
            // bits, then the sticky remainder.
            let (num, e) = if ma >= mb {
                ((ma as u128) << 63, ea - eb)
            } else {
                ((ma as u128) << 64, ea - eb - 1)
            };
            let q1 = num / mb as u128;
            let r1 = num % mb as u128;
            let n2 = r1 << 64;
            let q2 = n2 / mb as u128;
            let r2 = n2 % mb as u128;
            let sig = q1 << 64 | q2 | (r2 != 0) as u128;
            round_ext(env, s, e, sig, env.prec)
        }
        _ => unreachable!(),
    }
}

/// Integer square root of `n` and the remainder `n - r^2`.
fn isqrt(n: u128) -> (u128, u128) {
    let mut x = n;
    let mut r = 0u128;
    let mut bit = 1u128 << 126;
    while bit > n {
        bit >>= 2;
    }
    while bit != 0 {
        if x >= r + bit {
            x -= r + bit;
            r = (r >> 1) + bit;
        } else {
            r >>= 1;
        }
        bit >>= 2;
    }
    (r, x)
}

pub fn sqrt(env: &mut Env, a: F80) -> F80 {
    env.c1 = false;
    let (ca, da) = classify(a);
    match ca {
        Class::Unsupported => return invalid(env),
        Class::QNaN | Class::SNaN => return propagate(env, a, ca, a, ca),
        _ => {}
    }
    if da && !a.sign() {
        env.flags |= DE;
    }
    match ca {
        Class::Zero => a,
        _ if a.sign() => invalid(env),
        Class::Inf => a,
        Class::Finite { e, sig } => {
            // Radicand sig × 2^k with an even exponent left over.
            let k = if e & 1 != 0 { 64 } else { 63 };
            let (s, rem) = isqrt((sig as u128) << k);
            // The root is s + f with f in [0, 1); f is never exactly 1/2.
            let low = if rem > s { 1u128 << 63 } else { 0 } | (rem != 0) as u128;
            let m = e - 63 - k as i32;
            round_ext(env, false, m / 2 + 63, s << 64 | low, env.prec)
        }
        _ => unreachable!(),
    }
}

/// Round `a` to an integral value, per the rounding mode (`frndint`), or
/// toward zero for `trunc`. Precision control does not apply.
pub fn round_int(env: &mut Env, a: F80, trunc: bool) -> F80 {
    env.c1 = false;
    let (ca, da) = classify(a);
    match ca {
        Class::Unsupported => return invalid(env),
        Class::QNaN | Class::SNaN => return propagate(env, a, ca, a, ca),
        _ => {}
    }
    if da {
        env.flags |= DE;
    }
    let Class::Finite { e, sig } = ca else {
        return a;
    };
    if e >= 63 {
        return a;
    }
    let sign = a.sign();
    let (kept, up, inexact) = round_to_int(env.round, trunc, sign, e, sig);
    if inexact {
        env.flags |= PE;
    }
    env.c1 = up;
    if kept == 0 {
        return F80::zero(sign);
    }
    let lz = kept.leading_zeros();
    F80 {
        sig: kept << lz,
        se: (16383 + 63 - lz) as u16 | if sign { 0x8000 } else { 0 },
    }
}

/// `sig × 2^(e-63)` (e < 63) rounded to an integer magnitude: (value,
/// rounded up, inexact).
fn round_to_int(round: Round, trunc: bool, sign: bool, e: i32, sig: u64) -> (u64, bool, bool) {
    let drop = 63 - e; // >= 1
    let (kept, rem, half) = if drop >= 66 {
        (0u128, 1u128, 4u128)
    } else {
        let s = (sig as u128) << 1; // one guard bit so drop may reach 64
        let d = drop as u32 + 1;
        (s >> d, s & ((1u128 << d) - 1), 1u128 << (d - 1))
    };
    let inexact = rem != 0;
    let up = if trunc {
        false
    } else {
        match round {
            Round::Nearest => rem > half || (rem == half && kept & 1 == 1),
            Round::Zero => false,
            Round::Up => !sign && inexact,
            Round::Down => sign && inexact,
        }
    };
    ((kept + up as u128) as u64, up, inexact)
}

/// Where a comparison came out.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Cmp {
    Less,
    Equal,
    Greater,
    Unordered,
}

/// Compare `a` with `b`. `quiet` (`fucom`) raises IE only for an SNaN;
/// otherwise (`fcom`, `ftst`) any NaN raises it.
pub fn compare(env: &mut Env, a: F80, b: F80, quiet: bool) -> Cmp {
    env.c1 = false;
    let (ca, da) = classify(a);
    let (cb, db) = classify(b);
    if ca == Class::Unsupported || cb == Class::Unsupported {
        env.flags |= IE;
        return Cmp::Unordered;
    }
    if is_nan(ca) || is_nan(cb) {
        if !quiet || ca == Class::SNaN || cb == Class::SNaN {
            env.flags |= IE;
        }
        return Cmp::Unordered;
    }
    if da || db {
        env.flags |= DE;
    }
    let key = |c: Class, s: bool| -> (i32, i128) {
        // A total order on the ordered values: (sign-adjusted class, magnitude).
        let (rank, mag) = match c {
            Class::Zero => (0, 0i128),
            Class::Finite { e, sig } => (1, ((e as i128 + 20000) << 64) | sig as i128),
            _ => (2, 0),
        };
        if s && rank != 0 {
            (-rank, -mag)
        } else {
            (rank, mag)
        }
    };
    match key(ca, a.sign()).cmp(&key(cb, b.sign())) {
        std::cmp::Ordering::Less => Cmp::Less,
        std::cmp::Ordering::Equal => Cmp::Equal,
        std::cmp::Ordering::Greater => Cmp::Greater,
    }
}

/// `fscale`: `a × 2^trunc(b)`.
pub fn scale(env: &mut Env, a: F80, b: F80) -> F80 {
    env.c1 = false;
    let (ca, cb, den) = match operands(env, a, b) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let s = a.sign();
    if den {
        env.flags |= DE;
    }
    match (ca, cb) {
        (Class::Zero, Class::Inf) if !b.sign() => invalid(env),
        (Class::Inf, Class::Inf) if b.sign() => invalid(env),
        (Class::Zero, _) | (Class::Inf, _) => a,
        (Class::Finite { .. }, Class::Inf) => {
            if b.sign() {
                F80::zero(s)
            } else {
                F80::inf(s)
            }
        }
        (Class::Finite { e, sig }, Class::Zero) => round_ext(env, s, e, (sig as u128) << 64, 64),
        (Class::Finite { e, sig }, Class::Finite { e: eb, sig: mb }) => {
            // trunc(b), saturated well beyond any exponent that matters.
            let n: i64 = if eb >= 20 {
                1 << 20
            } else if eb < 0 {
                0
            } else {
                (mb >> (63 - eb)) as i64
            };
            let n = if b.sign() { -n } else { n };
            round_ext(env, s, e + n as i32, (sig as u128) << 64, 64)
        }
        _ => unreachable!(),
    }
}

/// Memory formats `fld` reads and `fst` writes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mem {
    F32,
    F64,
    I16,
    I32,
    I64,
}

/// Convert a memory operand (its raw bits) to extended. Exact; raises IE
/// for an SNaN (which it quiets) and DE for a denormal.
pub fn load(env: &mut Env, raw: u64, m: Mem) -> F80 {
    env.c1 = false;
    let int = |v: i64| -> F80 {
        if v == 0 {
            return F80::ZERO;
        }
        let mag = v.unsigned_abs();
        let lz = mag.leading_zeros();
        F80 {
            sig: mag << lz,
            se: (16383 + 63 - lz) as u16 | if v < 0 { 0x8000 } else { 0 },
        }
    };
    let float = |env: &mut Env, raw: u64, f: Fmt| -> F80 {
        let frac_bits = f.bits - 1;
        let exp_bits = if f.bits == 24 { 8 } else { 11 };
        let sign = raw >> (frac_bits + exp_bits) & 1 != 0;
        let exp = ((raw >> frac_bits) & ((1 << exp_bits) - 1)) as i32;
        let frac = raw & ((1u64 << frac_bits) - 1);
        let emax_field = (1 << exp_bits) - 1;
        let r = if exp == emax_field {
            if frac == 0 {
                F80::inf(false)
            } else {
                let nan = F80 {
                    sig: 1 << 63 | frac << (64 - f.bits),
                    se: 0x7fff,
                };
                if nan.sig & 1 << 62 == 0 {
                    env.flags |= IE;
                }
                nan.quiet()
            }
        } else if exp == 0 {
            if frac == 0 {
                F80::ZERO
            } else {
                env.flags |= DE;
                let lz = frac.leading_zeros() as i32;
                // value = frac × 2^(emin - (bits-1)); leading bit at 63 - lz.
                let e = f.emin - (f.bits as i32 - 1) + (63 - lz);
                F80 {
                    sig: frac << lz,
                    se: (e + 16383) as u16,
                }
            }
        } else {
            F80 {
                sig: 1 << 63 | frac << (64 - f.bits),
                se: (exp - f.emax + 16383) as u16,
            }
        };
        r.with_sign(sign)
    };
    match m {
        Mem::F32 => float(env, raw & 0xffff_ffff, SGL),
        Mem::F64 => float(env, raw, DBL),
        Mem::I16 => int(raw as i16 as i64),
        Mem::I32 => int(raw as i32 as i64),
        Mem::I64 => int(raw as i64),
    }
}

/// Convert `a` to a memory format (`fst`, `fist`; `trunc` for `fisttp`),
/// returning the raw bits. Out-of-range and NaN integer results are the
/// integer indefinite, with IE.
pub fn store(env: &mut Env, a: F80, m: Mem, trunc: bool) -> u64 {
    env.c1 = false;
    let (ca, _) = classify(a);
    let sign = a.sign();
    match m {
        Mem::F32 | Mem::F64 => {
            let f = if m == Mem::F32 { SGL } else { DBL };
            let frac_bits = f.bits - 1;
            let sign_bit = (sign as u64) << (if m == Mem::F32 { 31 } else { 63 });
            let exp_all = ((2 * f.emax + 1) as u64) << frac_bits;
            match ca {
                Class::Unsupported => {
                    env.flags |= IE;
                    // The indefinite, narrowed.
                    exp_all | 1 << (frac_bits - 1) | 1u64 << (if m == Mem::F32 { 31 } else { 63 })
                }
                Class::QNaN | Class::SNaN => {
                    if ca == Class::SNaN {
                        env.flags |= IE;
                    }
                    let q = a.quiet();
                    sign_bit | exp_all | ((q.sig << 1) >> (64 - frac_bits))
                }
                Class::Inf => sign_bit | exp_all,
                Class::Zero => sign_bit,
                Class::Finite { e, sig } => {
                    let (be, mant) = round_pack(env, sign, e, (sig as u128) << 64, f.bits, f);
                    let frac = mant & ((1u64 << frac_bits) - 1);
                    sign_bit | (be as u64) << frac_bits | frac
                }
            }
        }
        Mem::I16 | Mem::I32 | Mem::I64 => {
            let bits = match m {
                Mem::I16 => 16,
                Mem::I32 => 32,
                _ => 64,
            };
            let indefinite = 1u64 << (bits - 1);
            let v: Option<i128> = match ca {
                Class::Zero => Some(0),
                Class::Finite { e, sig } => {
                    if e >= 64 {
                        None
                    } else if e == 63 {
                        Some(sig as i128)
                    } else {
                        let (k, up, inexact) = round_to_int(env.round, trunc, sign, e, sig);
                        if inexact {
                            env.flags |= PE;
                        }
                        env.c1 = up;
                        Some(k as i128)
                    }
                }
                _ => None,
            };
            let v = v.map(|v| if sign { -v } else { v });
            match v {
                Some(v) if v >= -(1i128 << (bits - 1)) && v < (1i128 << (bits - 1)) => {
                    (v as i64 as u64) & (u64::MAX >> (64 - bits))
                }
                _ => {
                    // Invalid wins over the precision exception.
                    env.flags = (env.flags & !PE) | IE;
                    env.c1 = false;
                    indefinite
                }
            }
        }
    }
}

/// `f64` bits to extended, exactly (for callers computing in double).
pub fn from_f64(v: f64) -> F80 {
    let mut env = Env::from_cw(0x37f);
    load(&mut env, v.to_bits(), Mem::F64)
}

/// Extended to `f64`, rounding to nearest.
pub fn to_f64(a: F80) -> f64 {
    let mut env = Env::from_cw(0x37f);
    f64::from_bits(store(&mut env, a, Mem::F64, false))
}

/// The x87 class `fxam` reports, as C3/C2/C0 (bits 14, 10, 8 of the status
/// word) plus C1 for the sign. `empty` for an empty register.
pub fn examine(a: F80, empty: bool) -> u16 {
    const C0: u16 = 1 << 8;
    const C1: u16 = 1 << 9;
    const C2: u16 = 1 << 10;
    const C3: u16 = 1 << 14;
    let class = if empty {
        C3 | C0
    } else {
        match classify(a) {
            (Class::Unsupported, _) => 0,
            (Class::QNaN | Class::SNaN, _) => C0,
            (Class::Inf, _) => C2 | C0,
            (Class::Zero, _) => C3,
            (Class::Finite { .. }, true) => C3 | C2,
            (Class::Finite { .. }, false) => C2,
        }
    };
    class | if a.sign() { C1 } else { 0 }
}

/// The constants `fld1`, `fldl2t`, `fldl2e`, `fldpi`, `fldlg2`, `fldln2` and
/// `fldz` load. The x87 holds each to 66 bits and rounds to 64 by the
/// rounding mode.
pub fn constant(env: &Env, which: u32) -> F80 {
    // (se, significand rounded down, whether rounding to nearest goes up)
    let (se, lo, near_up): (u16, u64, bool) = match which {
        0 => return F80::ONE,
        1 => (0x4000, 0xd49a_784b_cd1b_8afe, false), // log2(10)
        2 => (0x3fff, 0xb8aa_3b29_5c17_f0bb, true),  // log2(e)
        3 => (0x4000, 0xc90f_daa2_2168_c234, true),  // pi
        4 => (0x3ffd, 0x9a20_9a84_fbcf_f798, true),  // log10(2)
        5 => (0x3ffe, 0xb172_17f7_d1cf_79ab, true),  // ln(2)
        _ => return F80::ZERO,
    };
    let up = match env.round {
        Round::Nearest => near_up,
        Round::Up => true,
        Round::Down | Round::Zero => false,
    };
    F80 {
        sig: lo + up as u64,
        se,
    }
}

/// `fprem` (`nearest` false: the quotient truncated, as C's `fmod`) and
/// `fprem1` (`nearest`: rounded to even, IEEE `remainder`). Returns the
/// partial remainder, the quotient's low three bits, and whether the
/// reduction is complete (C2 clear). When the exponents differ by 64 or
/// more the x87 reduces only partway, retiring 32 to 63 quotient bits; the
/// guest loops until C2 clears.
pub fn rem(env: &mut Env, a: F80, b: F80, nearest: bool) -> (F80, u8, bool) {
    env.c1 = false;
    let (ca, cb, den) = match operands(env, a, b) {
        Ok(c) => c,
        Err(r) => return (r, 0, true),
    };
    match (ca, cb) {
        (Class::Inf, _) | (_, Class::Zero) => return (invalid(env), 0, true),
        _ => {}
    }
    if den {
        env.flags |= DE;
    }
    let (Class::Finite { e: ea, sig: ma }, Class::Finite { e: eb, sig: mb }) = (ca, cb) else {
        // A zero dividend, or an infinite divisor: the dividend is the
        // remainder (a pseudo-denormal comes back in canonical form).
        if let Class::Finite { e, sig } = ca {
            return (round_ext(env, a.sign(), e, (sig as u128) << 64, 64), 0, true);
        }
        return (a, 0, true);
    };
    let d = ea - eb;
    let sign = a.sign();
    // Partial reduction: divide by b × 2^(d - n) instead, retiring n
    // quotient bits, n = 32 + (d mod 32) as the hardware does.
    let (d, eb, complete) = if d >= 64 {
        let n = 32 + (d & 31);
        (n, ea - n, false)
    } else {
        (d, eb, true)
    };
    let (mut q, mut r, mut flip) = if d < 0 {
        (0u128, ma as u128, false)
    } else {
        let num = (ma as u128) << d;
        (num / mb as u128, num % mb as u128, false)
    };
    // `r` is in units of 2^(eb - 63), or of 2^(ea - 63) when d < 0.
    let unit = if d < 0 { ea } else { eb };
    if nearest && complete {
        if d >= 0 {
            let twice = r << 1;
            if twice > mb as u128 || (twice == mb as u128 && q & 1 == 1) {
                q += 1;
                r = mb as u128 - r;
                flip = true;
            }
        } else if d == -1 && ma > mb {
            // |a| / |b| = ma / 2mb lies in (1/2, 1): the quotient rounds to
            // one (at exactly 1/2, ma == mb, it ties to the even zero), and
            // the remainder is |b| - |a|, in units of 2^(ea - 63).
            q = 1;
            r = ((mb as u128) << 1) - ma as u128;
            flip = true;
        }
    }
    let s = sign ^ flip;
    let r = if r == 0 {
        F80::zero(sign)
    } else {
        let lz = r.leading_zeros();
        round_ext(env, s, unit - 63 + (127 - lz as i32), r << lz, 64)
    };
    (r, (q & 7) as u8, complete)
}


/// `fxtract`: (exponent, significand), the significand to be pushed on top
/// of the exponent. A zero raises ZE and has exponent -inf.
pub fn extract(env: &mut Env, a: F80) -> (F80, F80) {
    env.c1 = false;
    let (ca, da) = classify(a);
    match ca {
        Class::Unsupported => {
            let r = invalid(env);
            (r, r)
        }
        Class::QNaN | Class::SNaN => {
            let r = propagate(env, a, ca, a, ca);
            (r, r)
        }
        Class::Zero => {
            env.flags |= ZE;
            (F80::inf(true), a)
        }
        Class::Inf => (F80::inf(false), a),
        Class::Finite { e, sig } => {
            if da {
                env.flags |= DE;
            }
            let exp = load(&mut Env::from_cw(0x37f), e as i64 as u64, Mem::I64);
            let m = F80 {
                sig,
                se: 0x3fff | if a.sign() { 0x8000 } else { 0 },
            };
            (exp, m)
        }
    }
}

/// The status-word bits an operation reports: its exception flags, and C1
/// at bit 9.
fn sw_bits(env: &Env) -> u32 {
    env.flags as u32 | (env.c1 as u32) << 9
}

/// `fst m32`/`fst m64` as one pure function -- what `Stmt::X87` with
/// `X87Op::Narrow` computes: the format's raw bits (zero-extended), and the
/// status-word bits (exception flags, C1 at bit 9).
pub fn narrow_op(sig: u64, se: u16, cw: u16, single: bool) -> (u64, u32) {
    let mut env = Env::from_cw(cw);
    let m = if single { Mem::F32 } else { Mem::F64 };
    let bits = store(&mut env, F80 { sig, se }, m, false);
    (bits, sw_bits(&env))
}

/// `fist`/`fisttp` as one pure function -- `X87Op::ToInt`: the integer
/// (its low `bits` bits, zero-extended) and the status-word bits.
pub fn to_int_op(sig: u64, se: u16, cw: u16, bits: u8, trunc: bool) -> (u64, u32) {
    let mut env = Env::from_cw(cw);
    let m = match bits {
        16 => Mem::I16,
        32 => Mem::I32,
        _ => Mem::I64,
    };
    let v = store(&mut env, F80 { sig, se }, m, trunc);
    (v, sw_bits(&env))
}

/// The f80 value as an `f64`, when it is one exactly and normal (or zero).
#[inline]
fn exact_f64(a: F80) -> Option<f64> {
    let e = (a.se & 0x7fff) as i32;
    let neg = ((a.se & 0x8000) as u64) << 48;
    if a.sig == 0 {
        return if e == 0 {
            Some(f64::from_bits(neg))
        } else {
            None
        };
    }
    let de = e - 16383 + 1023;
    if a.sig >> 63 == 0 || a.sig & 0x7ff != 0 || de < 1 || de > 2046 {
        return None;
    }
    Some(f64::from_bits(
        neg | (de as u64) << 52 | (a.sig >> 11) & ((1 << 52) - 1),
    ))
}

/// An `f64` that is zero or normal, as f80.
#[inline]
fn f64_to_f80(r: f64) -> Option<F80> {
    let b = r.to_bits();
    let s = ((b >> 48) & 0x8000) as u16;
    let de = ((b >> 52) & 0x7ff) as u16;
    let m = b & ((1 << 52) - 1);
    if de == 0 {
        return if m == 0 {
            Some(F80 { sig: 0, se: s })
        } else {
            None
        };
    }
    if de == 0x7ff {
        return None;
    }
    Some(F80 {
        sig: 1 << 63 | m << 11,
        se: s | (de + 16383 - 1023),
    })
}

/// How many bits the odd part of an `f64`'s significand takes (0 for zero).
#[inline]
fn odd_bits(v: f64) -> u32 {
    let b = v.to_bits();
    if b << 1 == 0 {
        return 0;
    }
    let m = (b & ((1 << 52) - 1)) | 1 << 52;
    let o = m >> m.trailing_zeros();
    64 - o.leading_zeros()
}

/// `arith_op` for operands and a result that are all exact `f64`s: then the
/// extended result is that exact value whatever the precision control (53
/// or 64 bits) and no exception is raised, so `f64` arithmetic gives it bit
/// for bit. Round-to-nearest only (the sign of an exact zero sum depends on
/// the rounding mode). `None` when it cannot prove the result exact.
#[inline]
pub fn arith_exact(k: u8, a: F80, b: F80, cw: u16) -> Option<F80> {
    if cw & 0x0c00 != 0 || cw & 0x0300 == 0 {
        return None;
    }
    let x = exact_f64(a)?;
    let y = exact_f64(b)?;
    let r = match k {
        0 | 1 => {
            let y = if k == 1 { -y } else { y };
            let s = x + y;
            let bb = s - x;
            let err = (x - (s - bb)) + (y - bb);
            if err != 0.0 {
                return None;
            }
            s
        }
        2 => {
            let p = x * y;
            // A product that underflowed to zero is not exact.
            if odd_bits(x) + odd_bits(y) > 53 || p == 0.0 && x != 0.0 && y != 0.0 {
                return None;
            }
            p
        }
        _ => {
            if y == 0.0 || x == 0.0 {
                return None;
            }
            let q = x / y;
            if odd_bits(q) + odd_bits(y) > 53 || q * y != x {
                return None;
            }
            q
        }
    };
    f64_to_f80(r)
}

/// The arithmetic `X87Op::Arith` names: 0 add, 1 subtract, 2 multiply,
/// 3 divide.
pub fn arith_op(k: u8, a: F80, b: F80, cw: u16) -> (F80, u32) {
    if let Some(r) = arith_exact(k, a, b, cw) {
        return (r, 0);
    }
    arith_ext(k, a, b, cw)
}

/// `arith_op` in extended arithmetic throughout, never through `f64`.
pub fn arith_ext(k: u8, a: F80, b: F80, cw: u16) -> (F80, u32) {
    let mut env = Env::from_cw(cw);
    let r = match k {
        0 => add(&mut env, a, b, false),
        1 => add(&mut env, a, b, true),
        2 => mul(&mut env, a, b),
        _ => div(&mut env, a, b),
    };
    (r, sw_bits(&env))
}

/// `fcom` (`quiet` false) or `fucom` as one pure function --
/// `X87Op::Compare`: the condition codes as status-word bits (C0 for less,
/// C3 for equal, C0|C2|C3 unordered, none for greater) and the exception
/// flags (C1 clear).
pub fn compare_op(a: F80, b: F80, quiet: bool) -> (u32, u32) {
    let mut env = Env::from_cw(0x37f);
    let cc = match compare(&mut env, a, b, quiet) {
        Cmp::Greater => 0,
        Cmp::Less => 1 << 8,
        Cmp::Equal => 1 << 14,
        Cmp::Unordered => 1 << 8 | 1 << 10 | 1 << 14,
    };
    (cc, sw_bits(&env))
}
