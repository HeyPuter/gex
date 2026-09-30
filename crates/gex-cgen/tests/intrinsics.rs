//! Every SIMD operation the C side names must lower to the very instruction
//! tier-1 emits for it: compile one tiny function per (op, lane) with clang
//! and look for the opcode in its body.

use gex_ir::{VLane, VLaneBinOp, VLaneUnOp};
use std::process::Command;

const LANES: [VLane; 6] = [VLane::I8, VLane::I16, VLane::I32, VLane::I64, VLane::F32, VLane::F64];

fn leb(mut v: u32) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            return out;
        }
    }
}

#[test]
fn lane_ops_lower_to_tier1_opcodes() {
    use VLaneBinOp::*;
    use VLaneUnOp::*;
    let bins = [
        Add, Sub, Mul, Div, MinS, MinU, MaxS, MaxU, AddSatS, AddSatU, SubSatS, SubSatU, AvgrU, Eq,
        Ne, LtS, LtU, GtS, GtU, LeS, LeU, GeS, GeU,
    ];
    let uns = [Neg, Abs, Sqrt, Ceil, Floor, Trunc, Nearest];
    let mut src = String::from("#include <wasm_simd128.h>\n");
    let mut want: Vec<(String, u32)> = Vec::new();
    for lane in LANES {
        for op in bins {
            if let Some(code) = op.wasm_opcode(lane) {
                let f = gex_cgen::emit::lane_bin_name(op, lane).unwrap();
                let name = format!("t{}", want.len());
                src += &format!("v128_t {name}(v128_t a, v128_t b) {{ return {f}(a, b); }}\n");
                want.push((format!("{op:?} {lane:?} via {f}"), code));
            }
        }
        for op in uns {
            if let Some(code) = op.wasm_opcode(lane) {
                let f = gex_cgen::emit::lane_un_name(op, lane).unwrap();
                let name = format!("t{}", want.len());
                src += &format!("v128_t {name}(v128_t a) {{ return {f}(a); }}\n");
                want.push((format!("{op:?} {lane:?} via {f}"), code));
            }
        }
    }
    let dir = std::env::temp_dir().join(format!("gex-cgen-intrinsics-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let c = dir.join("t.c");
    let o = dir.join("t.o");
    std::fs::write(&c, src).unwrap();
    let st = Command::new(gex_cgen::toolchain::clang())
        .args(gex_cgen::toolchain::CFLAGS)
        .arg("-c")
        .arg(&c)
        .arg("-o")
        .arg(&o)
        .status()
        .expect("clang");
    assert!(st.success());
    let wasm = std::fs::read(&o).unwrap();
    let mut bodies = Vec::new();
    for p in wasmparser::Parser::new(0).parse_all(&wasm) {
        if let wasmparser::Payload::CodeSectionEntry(b) = p.unwrap() {
            bodies.push(b.as_bytes().to_vec());
        }
    }
    assert_eq!(bodies.len(), want.len());
    let mut bad = Vec::new();
    for ((what, code), body) in want.iter().zip(&bodies) {
        let mut pat = vec![0xfd];
        pat.extend(leb(*code));
        if !body.windows(pat.len()).any(|w| w == pat.as_slice()) {
            bad.push(format!("{what}: want 0xfd {code:#x}, body {body:02x?}"));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
