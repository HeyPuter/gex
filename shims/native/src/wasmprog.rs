//! Native wasm32 programs (stream W, docs/wine-native.md §3.2): a Linux
//! program compiled to wasm32 against gex's musl port, run as a gex process
//! with the i386 Linux ABI. No guest code is translated: the program is one
//! position-independent wasm module, installed in the function table like a
//! stateful provider (`libprov`).
//!
//! The container is an ordinary ELF32 i386 executable, so exec, the initial
//! stack, auxv and brk are the kernel's as for any i386 program
//! (`tools/wine-wasm/wasm-elf.py` writes it). A read-only page holds a
//! header ([`MAGIC`]) 0x40 bytes below the entry; nothing in the image is
//! executable, so gex translates none of it:
//!
//! | offset | field |
//! |---|---|
//! | 0 | `GEXWASM\0` |
//! | 8 | version (1) |
//! | 12 | guest address of the wasm module (a read-only segment) |
//! | 16 | its length |
//! | 20 | `__memory_base`: the data region (a read-write segment) |
//! | 24 | the region's size |
//! | 28 | the entry (`AT_ENTRY`): `__gex_start` is bound there |
//! | 32 | the thread entry (entry + 16): `__gex_thread_start` |
//!
//! Both entries are `F_body`s that never return (musl's `gex_thread.s`).
//! The module is instantiated once per thread; its `__wasm_init_memory`
//! initialises the data once.

use super::*;

pub const MAGIC: &[u8; 8] = b"GEXWASM\0";

/// If the program just placed is a native wasm32 one, install its module
/// in the function table at its entries. Returns whether it was.
pub(crate) fn install(store: &mut Store<HostState>, memory: &gex_mem::GuestMemory, program: &Program) -> Result<bool> {
    // The header is 0x40 bytes below the entry.
    let text = program.exe.entry.wrapping_sub(0x40);
    let hdr = {
        let b = memory.bytes();
        let Some(h) = b.get(text as usize..text as usize + 36) else { return Ok(false) };
        if &h[..8] != MAGIC {
            return Ok(false);
        }
        let w = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap()) as u64;
        if w(8) != 1 {
            bail!("wasm32 program: header version {} (want 1)", w(8));
        }
        (w(12), w(16), w(20), w(24), w(28), w(32))
    };
    let (wasm_at, wasm_len, mem_base, _mem_size, entry, thread_entry) = hdr;
    let wasm = memory
        .bytes()
        .get(wasm_at as usize..(wasm_at + wasm_len) as usize)
        .ok_or_else(|| anyhow!("wasm32 program: module outside memory"))?
        .to_vec();
    let slots = gex_cgen::provider::slot_names(&wasm)?;
    let slot = |name: &str| -> Result<u32> {
        slots
            .iter()
            .position(|s| s.as_deref() == Some(name))
            .map(|k| k as u32)
            .ok_or_else(|| anyhow!("wasm32 program: {name} is not in its table"))
    };
    let pairs = [(entry, slot("__gex_start")?), (thread_entry, slot("__gex_thread_start")?)];
    let engine = store.engine().clone();
    let t0 = std::time::Instant::now();
    let module = Module::new(&engine, &wasm).context("compiling the wasm32 program")?;
    let shared = store
        .data()
        .jit
        .as_ref()
        .map(|j| Arc::clone(&j.shared))
        .ok_or_else(|| anyhow!("wasm32 program: the function table is off (--no-jit?)"))?;
    crate::ftab::publish_stateful(&shared, memory, module, slots.len() as u32, &pairs, crate::libprov::Extras { memory_base: mem_base as u32 })?;
    if std::env::var_os("GEX_WASM_TRACE").is_some() {
        eprintln!(
            "gex: wasm32 program: {} KiB wasm, data at {mem_base:#x}, entry {entry:#x}, compiled in {:.2}s",
            wasm.len() >> 10,
            t0.elapsed().as_secs_f64()
        );
    }
    Ok(true)
}
