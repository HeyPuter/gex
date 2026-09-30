//! gex's x87 extended-precision arithmetic (`gex_kernel::f80`) against the
//! host's own x87, instruction by instruction, over operands biased toward
//! every edge the format has. (A separate test crate: the kernel forbids
//! `unsafe`, and the oracle is inline assembly.)
#![cfg(target_arch = "x86_64")]

use gex_kernel::f80::*;
use std::arch::asm;

const C0: u16 = 1 << 8;
const C1: u16 = 1 << 9;
const C2: u16 = 1 << 10;
const C3: u16 = 1 << 14;

/// Run `$ins` natively with `st0 = a`, `st1 = b` under control word `cw`,
/// and return st0 and the status word.
macro_rules! native2 {
    ($name:ident, $ins:literal) => {
        fn $name(a: F80, b: F80, cw: u16) -> (F80, u16) {
            let ab = a.to_bytes();
            let bb = b.to_bytes();
            let mut out = [0u8; 10];
            let mut sw: u16 = 0;
            unsafe {
                asm!(
                    "fninit",
                    "fldcw word ptr [{cw}]",
                    "fld tbyte ptr [{b}]",
                    "fld tbyte ptr [{a}]",
                    $ins,
                    "fnstsw word ptr [{sw}]",
                    "fstp tbyte ptr [{out}]",
                    "fninit",
                    cw = in(reg) &cw,
                    a = in(reg) ab.as_ptr(),
                    b = in(reg) bb.as_ptr(),
                    out = in(reg) out.as_mut_ptr(),
                    sw = in(reg) &mut sw,
                    out("st(0)") _, out("st(1)") _, out("st(2)") _, out("st(3)") _,
                    out("st(4)") _, out("st(5)") _, out("st(6)") _, out("st(7)") _,
                );
            }
            (F80::from_bytes(out), sw)
        }
    };
}

native2!(n_add, "fadd st(0), st(1)");
native2!(n_sub, "fsub st(0), st(1)");
native2!(n_mul, "fmul st(0), st(1)");
native2!(n_div, "fdiv st(0), st(1)");
native2!(n_sqrt, "fsqrt");
native2!(n_rndint, "frndint");
native2!(n_scale, "fscale");
native2!(n_xam, "fxam");

macro_rules! native_com {
    ($name:ident, $ins:literal) => {
        fn $name(a: F80, b: F80, cw: u16) -> u16 {
            let ab = a.to_bytes();
            let bb = b.to_bytes();
            let mut sw: u16 = 0;
            unsafe {
                asm!(
                    "fninit", "fldcw word ptr [{cw}]",
                    "fld tbyte ptr [{b}]", "fld tbyte ptr [{a}]",
                    $ins, "fnstsw word ptr [{sw}]", "fninit",
                    cw = in(reg) &cw, a = in(reg) ab.as_ptr(), b = in(reg) bb.as_ptr(),
                    sw = in(reg) &mut sw,
                    out("st(0)") _, out("st(1)") _, out("st(2)") _, out("st(3)") _,
                    out("st(4)") _, out("st(5)") _, out("st(6)") _, out("st(7)") _,
                );
            }
            sw
        }
    };
}

native_com!(n_fcom, "fcom st(1)");
native_com!(n_fucom, "fucom st(1)");

/// `fld` from memory format `m` natively: (st0, status word).
fn n_load(raw: u64, m: Mem, cw: u16) -> (F80, u16) {
    let mut out = [0u8; 10];
    let mut sw: u16 = 0;
    macro_rules! go {
        ($ins:literal) => {
            unsafe {
                asm!(
                    "fninit", "fldcw word ptr [{cw}]",
                    $ins,
                    "fnstsw word ptr [{sw}]",
                    "fstp tbyte ptr [{out}]", "fninit",
                    cw = in(reg) &cw, p = in(reg) &raw, out = in(reg) out.as_mut_ptr(),
                    sw = in(reg) &mut sw,
                    out("st(0)") _, out("st(1)") _, out("st(2)") _, out("st(3)") _,
                    out("st(4)") _, out("st(5)") _, out("st(6)") _, out("st(7)") _,
                )
            }
        };
    }
    match m {
        Mem::F32 => go!("fld dword ptr [{p}]"),
        Mem::F64 => go!("fld qword ptr [{p}]"),
        Mem::I16 => go!("fild word ptr [{p}]"),
        Mem::I32 => go!("fild dword ptr [{p}]"),
        Mem::I64 => go!("fild qword ptr [{p}]"),
    }
    (F80::from_bytes(out), sw)
}

/// `fst`/`fist`/`fisttp` to memory format `m` natively: (raw, status word).
fn n_store(a: F80, m: Mem, cw: u16, trunc: bool) -> (u64, u16) {
    let ab = a.to_bytes();
    let mut raw: u64 = 0;
    let mut sw: u16 = 0;
    macro_rules! go {
        ($ins:literal) => {
            unsafe {
                asm!(
                    "fninit", "fldcw word ptr [{cw}]",
                    "fld tbyte ptr [{a}]",
                    $ins,
                    "fnstsw word ptr [{sw}]", "fninit",
                    cw = in(reg) &cw, a = in(reg) ab.as_ptr(), p = in(reg) &mut raw,
                    sw = in(reg) &mut sw,
                    out("st(0)") _, out("st(1)") _, out("st(2)") _, out("st(3)") _,
                    out("st(4)") _, out("st(5)") _, out("st(6)") _, out("st(7)") _,
                )
            }
        };
    }
    match (m, trunc) {
        (Mem::F32, _) => go!("fstp dword ptr [{p}]"),
        (Mem::F64, _) => go!("fstp qword ptr [{p}]"),
        (Mem::I16, false) => go!("fistp word ptr [{p}]"),
        (Mem::I32, false) => go!("fistp dword ptr [{p}]"),
        (Mem::I64, false) => go!("fistp qword ptr [{p}]"),
        (Mem::I16, true) => go!("fisttp word ptr [{p}]"),
        (Mem::I32, true) => go!("fisttp dword ptr [{p}]"),
        (Mem::I64, true) => go!("fisttp qword ptr [{p}]"),
    }
    (raw, sw)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[(self.next() % v.len() as u64) as usize]
    }

    /// An operand biased toward the edges: zeros, denormals and
    /// pseudo-denormals, the ends of the exponent range and of double's and
    /// single's, infinities, both kinds of NaN, unsupported encodings, and
    /// short significands whose sums and products land exactly on rounding
    /// boundaries.
    fn f80(&mut self) -> F80 {
        let sign = if self.next() & 1 == 1 { 0x8000 } else { 0 };
        let r = self.next();
        let sig = match self.next() % 6 {
            0 => 1 << 63 | (r & 0xffff) << 40,
            1 => 1 << 63 | r & 0x7ff,
            2 => u64::MAX - (r & 0xfff),
            3 => 1 << 63 | r >> 1 & !0x7ff,
            _ => 1 << 63 | r,
        };
        let exp = match self.next() % 12 {
            0 => 0,
            1 => 1 + self.next() % 70,
            2 => 0x7ffe - self.next() % 70,
            3 => 0x7fff,
            4 => 0x3fff - 1100 + self.next() % 80,
            5 => 0x3fff + 1000 + self.next() % 50,
            6 => 0x3fff - 150 + self.next() % 40,
            _ => 0x3fff - 64 + self.next() % 128,
        } as u16;
        let sig = match exp {
            // Zero, a pseudo-denormal or a denormal.
            0 => match self.next() % 4 {
                0 => 0,
                1 => sig,
                _ => sig >> (1 + self.next() % 63),
            },
            0x7fff => match self.next() % 5 {
                0 => 1 << 63,
                1 => 0xc000_0000_0000_0000 | r >> 2,
                2 => 0x8000_0000_0000_0000 | (r >> 2).max(1),
                3 => r >> 1,
                _ => 0xc000_0000_0000_0000,
            },
            _ if self.next() % 40 == 0 => sig >> 1, // an unnormal
            _ => sig,
        };
        F80 { sig, se: exp | sign }
    }

    /// A control word with every exception masked and any PC and RC.
    fn cw(&mut self) -> u16 {
        0x3f | 0x40 | ((self.next() % 4) as u16) << 8 | ((self.next() % 4) as u16) << 10
    }
}

/// The status-word bits an arithmetic operation defines.
const SW_BITS: u16 = IE | DE | ZE | OE | UE | PE | SF | C1;

fn sw_of(env: &Env) -> u16 {
    env.flags | if env.c1 { C1 } else { 0 }
}

fn check2(name: &str, n: fn(F80, F80, u16) -> (F80, u16), g: fn(&mut Env, F80, F80) -> F80) {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ name.len() as u64);
    let mut bad = 0;
    for _ in 0..200_000 {
        let (a, b, cw) = (rng.f80(), rng.f80(), rng.cw());
        let (want, wsw) = n(a, b, cw);
        let mut env = Env::from_cw(cw);
        let got = g(&mut env, a, b);
        if got != want || sw_of(&env) != wsw & SW_BITS {
            bad += 1;
            if bad < 12 {
                eprintln!(
                    "{name} cw={cw:04x} a={a:x?} b={b:x?}\n  want {want:x?} sw {:04x}\n  got  {got:x?} sw {:04x}",
                    wsw & SW_BITS,
                    sw_of(&env)
                );
            }
        }
    }
    assert_eq!(bad, 0, "{name}: {bad} mismatches");
}

#[test]
fn add_matches_the_hardware() {
    check2("fadd", n_add, |e, a, b| add(e, a, b, false));
}

#[test]
fn sub_matches_the_hardware() {
    check2("fsub", n_sub, |e, a, b| add(e, a, b, true));
}

#[test]
fn mul_matches_the_hardware() {
    check2("fmul", n_mul, mul);
}

#[test]
fn div_matches_the_hardware() {
    check2("fdiv", n_div, div);
}

#[test]
fn sqrt_matches_the_hardware() {
    check2("fsqrt", n_sqrt, |e, a, _| sqrt(e, a));
}

#[test]
fn frndint_matches_the_hardware() {
    check2("frndint", n_rndint, |e, a, _| round_int(e, a, false));
}

#[test]
fn fscale_matches_the_hardware() {
    check2("fscale", n_scale, scale);
}

#[test]
fn fxam_matches_the_hardware() {
    let mut rng = Rng(77);
    for _ in 0..50_000 {
        let a = rng.f80();
        let (_, sw) = n_xam(a, F80::ZERO, 0x37f);
        assert_eq!(examine(a, false), sw & (C0 | C1 | C2 | C3), "fxam {a:x?}");
    }
}

#[test]
fn compare_matches_the_hardware() {
    let mut rng = Rng(5);
    let mut bad = 0;
    for _ in 0..100_000 {
        let (mut a, b, cw) = (rng.f80(), rng.f80(), rng.cw());
        if rng.next() % 4 == 0 {
            a = b;
        }
        for quiet in [false, true] {
            let sw = if quiet { n_fucom(a, b, cw) } else { n_fcom(a, b, cw) };
            let mut env = Env::from_cw(cw);
            let cc = match compare(&mut env, a, b, quiet) {
                Cmp::Greater => 0,
                Cmp::Less => C0,
                Cmp::Equal => C3,
                Cmp::Unordered => C0 | C2 | C3,
            };
            if cc | sw_of(&env) != sw & (SW_BITS | C0 | C2 | C3) {
                bad += 1;
                if bad < 12 {
                    eprintln!(
                        "{} cw={cw:04x} {a:x?} {b:x?}: want {:04x} got {:04x}",
                        if quiet { "fucom" } else { "fcom" },
                        sw & (SW_BITS | C0 | C2 | C3),
                        cc | sw_of(&env)
                    );
                }
            }
        }
    }
    assert_eq!(bad, 0);
}

#[test]
fn loads_match_the_hardware() {
    let mut rng = Rng(11);
    let mut bad = 0;
    for _ in 0..100_000 {
        let m = rng.pick(&[Mem::F32, Mem::F64, Mem::I16, Mem::I32, Mem::I64]);
        let r = rng.next();
        let raw = match rng.next() % 6 {
            0 => r & 0x800f_ffff_ffff_ffff,
            1 => r | 0x7ff0_0000_0000_0000,
            2 => r & 0x807f_ffff,
            3 => r | 0x7f80_0000,
            4 => r & 0xffff,
            _ => r,
        };
        let raw = if m == Mem::F32 { raw & 0xffff_ffff } else { raw };
        let cw = rng.cw();
        let (want, wsw) = n_load(raw, m, cw);
        let mut env = Env::from_cw(cw);
        let got = load(&mut env, raw, m);
        if (got, sw_of(&env)) != (want, wsw & SW_BITS) {
            bad += 1;
            if bad < 12 {
                eprintln!(
                    "fld {m:?} {raw:016x}: want {want:x?} sw {:04x}, got {got:x?} sw {:04x}",
                    wsw & SW_BITS,
                    sw_of(&env)
                );
            }
        }
    }
    assert_eq!(bad, 0);
}

#[test]
fn stores_match_the_hardware() {
    let mut rng = Rng(13);
    let mut bad = 0;
    for _ in 0..200_000 {
        let m = rng.pick(&[Mem::F32, Mem::F64, Mem::I16, Mem::I32, Mem::I64]);
        let int = matches!(m, Mem::I16 | Mem::I32 | Mem::I64);
        let trunc = int && rng.next() % 3 == 0;
        let mut a = rng.f80();
        if int && rng.next() % 2 == 0 {
            // Integers near the range limits, and small fractions.
            let e = 16383 - 2 + (rng.next() % 68) as u16;
            a.se = (a.se & 0x8000) | e;
            a.sig |= 1 << 63;
        }
        let cw = rng.cw();
        let (want, wsw) = n_store(a, m, cw, trunc);
        let mut env = Env::from_cw(cw);
        let got = store(&mut env, a, m, trunc);
        let mask = match m {
            Mem::F32 | Mem::I32 => 0xffff_ffff,
            Mem::I16 => 0xffff,
            _ => u64::MAX,
        };
        if (got & mask, sw_of(&env)) != (want & mask, wsw & SW_BITS) {
            bad += 1;
            if bad < 12 {
                eprintln!(
                    "fst {m:?} trunc={trunc} cw={cw:04x} {a:x?}: want {:x} sw {:04x}, got {:x} sw {:04x}",
                    want & mask,
                    wsw & SW_BITS,
                    got & mask,
                    sw_of(&env)
                );
            }
        }
    }
    assert_eq!(bad, 0);
}

#[test]
fn constants_match_the_hardware() {
    for rc in 0..4u16 {
        let cw = 0x37f | rc << 10;
        let env = Env::from_cw(cw);
        macro_rules! k {
            ($i:literal, $ins:literal) => {{
                let mut out = [0u8; 10];
                unsafe {
                    asm!("fninit", "fldcw word ptr [{cw}]", $ins, "fstp tbyte ptr [{out}]", "fninit",
                        cw = in(reg) &cw, out = in(reg) out.as_mut_ptr(),
                        out("st(0)") _, out("st(1)") _, out("st(2)") _, out("st(3)") _,
                        out("st(4)") _, out("st(5)") _, out("st(6)") _, out("st(7)") _);
                }
                assert_eq!(constant(&env, $i), F80::from_bytes(out), "{} rc={rc}", $ins);
            }};
        }
        k!(0, "fld1");
        k!(1, "fldl2t");
        k!(2, "fldl2e");
        k!(3, "fldpi");
        k!(4, "fldlg2");
        k!(5, "fldln2");
        k!(6, "fldz");
    }
}

/// The long-double printf bug's repro: 1/3 as a double, times 3 in
/// extended precision, is 1 - 2^-54 (0.99999999999999994449...), not 1.
#[test]
fn one_third_times_three() {
    let mut env = Env::from_cw(0x37f);
    let third = load(&mut env, (1.0f64 / 3.0).to_bits(), Mem::F64);
    let three = load(&mut env, 3.0f64.to_bits(), Mem::F64);
    let r = mul(&mut env, third, three);
    assert_eq!(r, F80 { sig: 0xffff_ffff_ffff_fc00, se: 0x3ffe });
    assert_eq!(n_mul(third, three, 0x37f).0, r);
}

native2!(n_prem, "fprem");
native2!(n_prem1, "fprem1");

#[test]
fn fprem_matches_the_hardware() {
    let mut rng = Rng(21);
    let mut bad = 0;
    for _ in 0..200_000 {
        let (mut a, b, cw) = (rng.f80(), rng.f80(), rng.cw());
        if rng.next() % 2 == 0 {
            // Exponents close enough for a complete reduction, sometimes.
            a.se = (a.se & 0x8000) | (b.se & 0x7fff).wrapping_add(((rng.next() % 90) as u16).wrapping_sub(10));
        }
        for nearest in [false, true] {
            let (want, wsw) = if nearest { n_prem1(a, b, cw) } else { n_prem(a, b, cw) };
            let mut env = Env::from_cw(cw);
            let (got, q, complete) = rem(&mut env, a, b, nearest);
            // A partial reduction reports C2 alone (Intel leaves C0, C1 and
            // C3 undefined then; the hardware clears them).
            let cc = if complete {
                (if q & 4 != 0 { C0 } else { 0 })
                    | (if q & 2 != 0 { C3 } else { 0 })
                    | (if q & 1 != 0 { C1 } else { 0 })
            } else {
                C2
            };
            let mask = SW_BITS | C0 | C2 | C3;
            let gsw = (env.flags | cc) & mask;
            if got != want || gsw != wsw & mask {
                bad += 1;
                if bad < 12 {
                    eprintln!(
                        "fprem{} cw={cw:04x} a={a:x?} b={b:x?}\n  want {want:x?} sw {:04x}\n  got  {got:x?} sw {gsw:04x}",
                        if nearest { "1" } else { "" },
                        wsw & mask
                    );
                }
            }
        }
    }
    assert_eq!(bad, 0);
}

#[test]
fn fxtract_matches_the_hardware() {
    let mut rng = Rng(23);
    let mut bad = 0;
    for _ in 0..100_000 {
        let (a, cw) = (rng.f80(), rng.cw());
        // Natively: st0 = significand, st1 = exponent afterwards.
        let ab = a.to_bytes();
        let mut sig = [0u8; 10];
        let mut exp = [0u8; 10];
        let mut sw: u16 = 0;
        unsafe {
            asm!(
                "fninit", "fldcw word ptr [{cw}]", "fld tbyte ptr [{a}]", "fxtract",
                "fnstsw word ptr [{sw}]", "fstp tbyte ptr [{s}]", "fstp tbyte ptr [{e}]", "fninit",
                cw = in(reg) &cw, a = in(reg) ab.as_ptr(), s = in(reg) sig.as_mut_ptr(),
                e = in(reg) exp.as_mut_ptr(), sw = in(reg) &mut sw,
                out("st(0)") _, out("st(1)") _, out("st(2)") _, out("st(3)") _,
                out("st(4)") _, out("st(5)") _, out("st(6)") _, out("st(7)") _,
            );
        }
        let mut env = Env::from_cw(cw);
        let (e, s) = extract(&mut env, a);
        if (e, s, sw_of(&env)) != (F80::from_bytes(exp), F80::from_bytes(sig), sw & SW_BITS) {
            bad += 1;
            if bad < 12 {
                eprintln!(
                    "fxtract {a:x?}: want {:x?} {:x?} sw {:04x}, got {e:x?} {s:x?} sw {:04x}",
                    F80::from_bytes(exp),
                    F80::from_bytes(sig),
                    sw & SW_BITS,
                    sw_of(&env)
                );
            }
        }
    }
    assert_eq!(bad, 0);
}

/// Precision control: 53- and 24-bit modes round the significand to double
/// and single width over the full extended exponent range (unlike a store
/// to double or single, which also narrows the range).
#[test]
fn precision_control_rounds_to_53_and_24_bits() {
    let mut env = Env::from_cw(0x37f);
    let one = F80::ONE;
    let three = load(&mut env, 3.0f64.to_bits(), Mem::F64);
    for (pc, sig) in [
        (0x37fu16, 0xaaaa_aaaa_aaaa_aaabu64), // 64 bits
        (0x27f, 0xaaaa_aaaa_aaaa_a800),       // 53 bits
        (0x07f, 0xaaaa_ab00_0000_0000),       // 24 bits
    ] {
        let mut env = Env::from_cw(pc);
        let third = div(&mut env, one, three);
        assert_eq!(third, F80 { sig, se: 0x3ffd }, "cw {pc:#x}");
        assert_eq!(n_div(one, three, pc).0, third);
        assert_eq!(env.flags & PE, PE);
    }
    // A tiny quotient under 53-bit precision keeps the extended exponent: no
    // underflow where a double would have one.
    let tiny = F80 { sig: 1 << 63, se: 0x3fff - 2000 };
    let mut env = Env::from_cw(0x27f);
    let r = div(&mut env, tiny, three);
    assert_eq!(r, n_div(tiny, three, 0x27f).0);
    assert_eq!(r.se, 0x3fff - 2002);
    assert_eq!(env.flags & UE, 0);
}

