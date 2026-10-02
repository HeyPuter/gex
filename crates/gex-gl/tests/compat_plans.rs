//! The generated compatibility-profile plans (tools/gen_gl_compat.py) are
//! well-formed: every opcode is its own and past the hand-written and GL 4.4
//! ones, every token is one the encoders and decoders know, and every count
//! names a parameter the command has.

use gex_gl::compat_plans::{COMPAT_BASE, PLANS};
use std::collections::HashSet;

#[test]
fn plans_are_well_formed() {
    assert!(PLANS.len() > 2000, "the registry's commands are planned");
    let mut opcodes = HashSet::new();
    for (name, opcode, plan) in PLANS {
        assert!(*opcode >= COMPAT_BASE, "{name}: opcode {opcode}");
        assert!(opcodes.insert(*opcode), "{name}: opcode {opcode} twice");
        let toks: Vec<&str> = plan.split(' ').collect();
        assert!(matches!(toks[0], "v" | "w" | "f" | "q"), "{name}: return {}", toks[0]);
        let params = toks.len() - 1;
        for t in &toks[1..] {
            match t.as_bytes()[0] {
                b'w' | b'f' | b'q' | b'P' => assert_eq!(t.len(), 1, "{name}: {t}"),
                b'i' | b'j' | b'o' => {
                    let (size, spec) = t[1..].split_once(':').unwrap_or_else(|| panic!("{name}: {t}"));
                    assert!(matches!(size, "1" | "2" | "4" | "8"), "{name}: element size {size}");
                    let kind = spec.as_bytes()[0];
                    assert!(b"npezLsSXBTU".contains(&kind), "{name}: count {spec}");
                    for arg in spec[1..].split([',', '*']).filter(|a| !a.is_empty() && *a != "_") {
                        let v: usize = arg.parse().unwrap_or_else(|_| panic!("{name}: {spec}"));
                        // A multiplier, an element count, or a parameter.
                        if kind != b'n' && !spec.contains(&format!("*{arg}")) {
                            assert!(v < params, "{name}: {spec} names parameter {v} of {params}");
                        }
                    }
                }
                _ => panic!("{name}: token {t}"),
            }
        }
    }
}

#[test]
fn fixed_function_commands_are_planned() {
    // What a GL 1.1 renderer (GoldSrc's) and wined3d's fixed-function
    // pipeline call that no hand-written command covers.
    for want in ["glOrtho", "glTexCoord2f", "glFogfv", "glClipPlane", "glMultiTexCoord2f", "glGetTexEnvfv", "glDrawPixels", "glBitmap"] {
        assert!(PLANS.iter().any(|(n, _, _)| *n == want), "{want} has no plan");
    }
    let fog = PLANS.iter().find(|(n, _, _)| *n == "glFogfv").unwrap();
    assert_eq!(fog.2, "v w i4:e0", "glFogfv: a pname-sized float vector");
}
