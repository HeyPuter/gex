//! Dump gex's lifted IR + emitted WASM for a block, to find a mistranslation.
//! Usage: dumpir <file.so> <hex-file-offset>
use gex_lift::{lift_block, BlockLimits};
use std::collections::HashMap;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let start = u64::from_str_radix(args[2].trim_start_matches("0x"), 16).unwrap();
    let data = std::fs::read(path).unwrap();
    let fetch = |pc: u64| -> Option<u32> {
        let o = pc as usize;
        data.get(o..o + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    let block = lift_block(&fetch, start, BlockLimits::default());
    println!("=== IR ===\n{}", gex_ir::print::block(&block));

    let blocks: HashMap<u64, u32> = HashMap::new();
    let gir: HashMap<u64, &gex_ir::Block> = HashMap::new();
    let cx = gex_emit::tier1::Context {
        syscall: 0,
        illegal: 1,
        breakpoint: 2,
        sysreg_read: 3,
        fma: 4,
        sysreg_write: 5,
        cache_op: 6,
        dispatch_miss: 7,
        safepoint: 8,
        x87: 9,
        block_ty: 0,
        blocks: &blocks,
        gir: &gir,
        profile: None,
    };
    let wasm = gex_emit::tier1::compile(&block, &cx);
    println!("=== WASM ({} bytes) ===", wasm.len());
    disasm(&wasm);
}

// A minimal WASM-body disassembler: enough to read memory ops, arithmetic,
// locals, consts, and the atomic-prefixed ops that carry offsets.
fn disasm(code: &[u8]) {
    let mut i = 0;
    while i < code.len() {
        let op = code[i];
        i += 1;
        let name = match op {
            0x20 => "local.get",
            0x21 => "local.set",
            0x22 => "local.tee",
            0x28 => "i32.load",
            0x29 => "i64.load",
            0x2c => "i32.load8_s",
            0x2d => "i32.load8_u",
            0x2e => "i32.load16_s",
            0x2f => "i32.load16_u",
            0x30 => "i64.load8_s",
            0x31 => "i64.load8_u",
            0x34 => "i64.load32_s",
            0x35 => "i64.load32_u",
            0x36 => "i32.store",
            0x37 => "i64.store",
            0x3a => "i32.store8",
            0x3b => "i32.store16",
            0x3c => "i64.store8",
            0x3e => "i64.store32",
            0x6a => "i32.add",
            0x6b => "i32.sub",
            0x7c => "i64.add",
            0x7d => "i64.sub",
            0x71 => "i32.and",
            0x67..=0x69 => "i32.clz/ctz/popcnt",
            0xac => "i64.extend_i32_s",
            0xad => "i64.extend_i32_u",
            0xa7 => "i32.wrap_i64",
            0x41 => {
                let (v, n) = leb_s(&code[i..]);
                i += n;
                println!("  i32.const {v}");
                continue;
            }
            0x42 => {
                let (v, n) = leb_s(&code[i..]);
                i += n;
                println!("  i64.const {v} ({v:#x})");
                continue;
            }
            0x10 => {
                let (v, n) = leb_u(&code[i..]);
                i += n;
                println!("  call {v}");
                continue;
            }
            0x0b => {
                println!("  end");
                continue;
            }
            0xfe => {
                // atomic prefix: sub-op (leb), then memarg (align, offset)
                let (sub, n) = leb_u(&code[i..]);
                i += n;
                let (al, n2) = leb_u(&code[i..]);
                i += n2;
                let (off, n3) = leb_u(&code[i..]);
                i += n3;
                println!("  atomic.{sub:#x} align={al} off={off}");
                continue;
            }
            _ => {
                println!("  op {op:#04x}");
                continue;
            }
        };
        // memory ops carry (align, offset); locals/calls carry one leb.
        if (0x28..=0x3e).contains(&op) {
            let (al, n) = leb_u(&code[i..]);
            i += n;
            let (off, n2) = leb_u(&code[i..]);
            i += n2;
            println!("  {name} align={al} off={off}");
        } else if (0x20..=0x22).contains(&op) {
            let (v, n) = leb_u(&code[i..]);
            i += n;
            println!("  {name} {v}");
        } else {
            println!("  {name}");
        }
    }
}

fn leb_u(b: &[u8]) -> (u64, usize) {
    let (mut r, mut s, mut n) = (0u64, 0, 0);
    loop {
        let x = b[n];
        r |= ((x & 0x7f) as u64) << s;
        n += 1;
        if x & 0x80 == 0 {
            break;
        }
        s += 7;
    }
    (r, n)
}

fn leb_s(b: &[u8]) -> (i64, usize) {
    let (mut r, mut s, mut n) = (0i64, 0u32, 0);
    loop {
        let x = b[n];
        r |= ((x & 0x7f) as i64) << s;
        s += 7;
        n += 1;
        if x & 0x80 == 0 {
            if s < 64 && x & 0x40 != 0 {
                r |= -(1i64 << s);
            }
            break;
        }
    }
    (r, n)
}
