//! Tier-1 every block of a PE's code, looking for emitter panics.
//! Usage (built with GEX_ARCH=x86): t1sweep <pe> [window bytes, default 65536]
//!
//! Lifts a block at every byte offset of each executable section (a superset
//! of the real block starts), optimises it as the translator does, and
//! compiles each with tier 1 against its window's blocks (so regions form as
//! in the browser), catching panics. Prints each panic once with its block.
use std::collections::HashMap;

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let window: u64 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(65536);
    let data = std::fs::read(path).unwrap();
    let pe = u32le(&data, 0x3c) as usize;
    let nsec = u16le(&data, pe + 6) as usize;
    let optsz = u16le(&data, pe + 20) as usize;
    let opt = pe + 24;
    let base = u64::from_le_bytes(data[opt + 24..opt + 32].try_into().unwrap());
    let sec0 = opt + optsz;
    // The image as mapped, executable ranges noted.
    let mut image: HashMap<u64, u8> = HashMap::new();
    let mut exec: Vec<(u64, u64)> = Vec::new();
    for i in 0..nsec {
        let s = sec0 + i * 40;
        let vsize = u32le(&data, s + 8) as u64;
        let rva = u32le(&data, s + 12) as u64;
        let rsize = u32le(&data, s + 16) as usize;
        let raw = u32le(&data, s + 20) as usize;
        let ch = u32le(&data, s + 36);
        if ch & 0x2000_0000 == 0 {
            continue;
        }
        let n = rsize.min(vsize as usize).min(data.len().saturating_sub(raw));
        for k in 0..n {
            image.insert(base + rva + k as u64, data[raw + k]);
        }
        exec.push((base + rva, base + rva + n as u64));
    }
    let fetch = |pc: u64, buf: &mut gex_xlate::arch::FetchBuf| -> usize {
        let mut n = 0;
        while n < 16 {
            match image.get(&(pc + n as u64)) {
                Some(b) => buf[n] = *b,
                None => break,
            }
            n += 1;
        }
        n
    };
    if std::env::var_os("T1SWEEP_M64").is_some() {
        gex_abi::set_mem_flavor(gex_abi::MemFlavor::M64);
    }
    eprintln!("x86 {} mem64 {} base {base:#x} exec {exec:x?}", gex_abi::X86, gex_abi::mem64());
    if let Some(a) = std::env::var("T1SWEEP_SHOW").ok().and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok()) {
        let mut b = gex_xlate::lift_block(&fetch, a, gex_xlate::BlockLimits::default());
        gex_ir::pass::optimize(&mut b);
        println!("{}", gex_ir::print::block(&b));
        return;
    }
    let guard = std::env::var_os("T1SWEEP_GUARD").is_some();
    let pages = std::env::var_os("T1SWEEP_PAGES").is_some();
    std::panic::set_hook(Box::new(|_| {}));
    let mut seen: HashMap<String, u64> = HashMap::new();
    let (mut total, mut panics) = (0u64, 0u64);
    for &(lo, hi) in &exec {
        let mut w = lo;
        while w < hi {
            let we = (w + window).min(hi);
            let blocks: Vec<gex_ir::Block> = (w..we)
                .map(|a| {
                    let mut b = gex_xlate::lift_block(&fetch, a, gex_xlate::BlockLimits::default());
                    gex_ir::pass::optimize(&mut b);
                    if guard {
                        // As a volatile (JIT) block: its bytes as its guard.
                        let n = ((b.end.max(a + 1) - a) as usize).min(256);
                        let g: Vec<u8> = (0..n as u64).filter_map(|k| image.get(&(a + k)).copied()).collect();
                        if !g.is_empty() {
                            b.guard = Some(g);
                        }
                    }
                    b
                })
                .collect();
            let gir: HashMap<u64, &gex_ir::Block> = blocks.iter().map(|b| (b.start, b)).collect();
            let heads = gex_emit::tier1::loop_heads(blocks.iter());
            let ids: HashMap<u64, u32> = if std::env::var_os("T1SWEEP_IDS").is_some() {
                blocks.iter().enumerate().map(|(i, b)| (b.start, 20 + i as u32)).collect()
            } else {
                HashMap::new()
            };
            let mut page_bases: HashMap<u64, u32> = HashMap::new();
            if pages {
                for b in &blocks {
                    for p in gex_abi::dispatch_page(b.start)..=gex_abi::dispatch_page(b.end.max(b.start + 1) - 1) {
                        page_bases.insert(p, 7);
                    }
                }
            }
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
                blocks: &ids,
                gir: &gir,
                profile: None,
                page_bases: pages.then_some(&page_bases),
                dense_global: None,
                module_id: 0,
                loop_heads: Some(&heads),
                enter: Some(10),
            };
            for b in &blocks {
                total += 1;
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| gex_emit::tier1::compile_with_relocs(b, &cx)));
                if let Err(p) = r {
                    panics += 1;
                    let msg = p
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_default();
                    let key: String = msg.chars().take(60).collect();
                    let n = seen.entry(key).or_insert(0);
                    *n += 1;
                    if *n <= 3 {
                        println!("PANIC at {:#x}: {}", b.start, msg.chars().take(400).collect::<String>());
                        println!("{}", gex_ir::print::block(b));
                        if let Some(r) = gex_emit::tier1::region_members(b, &cx) {
                            for m in r {
                                if m != b.start {
                                    if let Some(mb) = gir.get(&m) {
                                        println!("-- region member\n{}", gex_ir::print::block(mb));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            w = we;
        }
    }
    println!("blocks {total} panics {panics}");
    for (k, n) in &seen {
        println!("  {n:8}  {k}");
    }
}
