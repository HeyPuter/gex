//! Native wasm32 programs: a Linux
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

/// Whether `program` is a native wasm32 one (its header, from the image's
/// segments, before anything is placed).
pub(crate) fn is_wasm32(program: &Program) -> bool {
    let at = program.exe.entry.wrapping_sub(0x40);
    program.exe.segments.iter().any(|s| {
        at >= s.vaddr && at + 8 <= s.vaddr + s.bytes.len() as u64 && {
            let o = (at - s.vaddr) as usize;
            &s.bytes[o..o + 8] == MAGIC
        }
    })
}

/// Header flag (offset 36): the program runs i386 code (32-bit Wine), so the
/// process translates i386 and leaves Windows' fixed addresses free.
pub const FLAG_GUEST_I386: u32 = 1;
/// Header flag: the program runs x86-64 code (64-bit native Wine). It
/// shares 32-bit Wine's address space and trampoline
/// page ([`FLAG_WINDOWS_SPACE`]), never its i386 decoding.
pub const FLAG_GUEST_X86_64: u32 = 2;
/// The flags whose process leaves Windows' fixed addresses free and gets
/// the trampoline page.
pub const FLAG_WINDOWS_SPACE: u32 = FLAG_GUEST_I386 | FLAG_GUEST_X86_64;
/// Header flag: this i386 Wine maps KUSER_SHARED_DATA at
/// `gex_abi::WINE32_KUSER` (wasm-elf.py, from Wine's `gex.kuser` section), so
/// the process folds Windows' constant addresses there.
pub const FLAG_KUSER_WINE32: u32 = 4;

/// Header flag, with [`FLAG_GUEST_I386`]: the program's i386 code is its
/// own (a JIT's, gexcc -fgex-guest-i386), not Windows': the process keeps
/// Linux's address space, only the trampoline page added (the return pad
/// of its calls into the guest).
pub const FLAG_LINUX_SPACE: u32 = 8;

/// The header's flags, 0 for a program without them (or not wasm32).
pub(crate) fn flags(program: &Program) -> u32 {
    let at = program.exe.entry.wrapping_sub(0x40);
    program
        .exe
        .segments
        .iter()
        .find(|s| at >= s.vaddr && at + 40 <= s.vaddr + s.bytes.len() as u64)
        .map(|s| {
            let o = (at - s.vaddr) as usize;
            if &s.bytes[o..o + 8] != MAGIC {
                return 0;
            }
            u32::from_le_bytes(s.bytes[o + 36..o + 40].try_into().unwrap())
        })
        .unwrap_or(0)
}

/// Whether `wasm` imports a memory that is not a 64-bit one.
fn imports_memory32(wasm: &[u8]) -> bool {
    for p in wasmparser::Parser::new(0).parse_all(wasm) {
        let Ok(wasmparser::Payload::ImportSection(r)) = p else { continue };
        return r.into_iter().flatten().any(|i| matches!(i.ty, wasmparser::TypeRef::Memory(m) if !m.memory64));
    }
    false
}

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
    // A wasm32 module imports a 32-bit memory: one host process is one
    // memory flavour, so a run that started with a 64-bit memory (`--mem64`,
    // or what an earlier run of its first program learned) cannot run it.
    if gex_abi::mem64() && imports_memory32(&wasm) {
        let path = store.data_mut().with_kernel(|k| k.exe_path.clone());
        bail!("{path}: a wasm32 program, which needs a 32-bit memory, and this run has a 64-bit one (run its first program with --mem32)");
    }
    // 32-bit Wine's loader: refused when its inline probe reads another layout.
    if flags(program) & FLAG_GUEST_I386 != 0 {
        let path = store.data_mut().with_kernel(|k| k.exe_path.clone());
        if let Some(e) = gex_cgen::header::abi_refusal(&wasm, &format!("Wine's loader {path}")) {
            bail!("{e}");
        }
    }
    // A program linked against shared libraries.
    if let Some(info) = crate::dylink::wanted(&wasm) {
        let path = store.data_mut().with_kernel(|k| k.exe_path.clone());
        crate::dylink::install_program(store, memory, &path, wasm, info, mem_base, entry, thread_entry)?;
        return Ok(true);
    }
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
    let module = gex_cache::compile_cached(&engine, &wasm).context("compiling the wasm32 program")?;
    let shared = store
        .data()
        .jit
        .as_ref()
        .map(|j| Arc::clone(&j.shared))
        .ok_or_else(|| anyhow!("wasm32 program: the function table is off (--no-jit?)"))?;
    shared.lock().unwrap_or_else(|e| e.into_inner()).wasm_program = true;
    // gex-kernel's wasm32 personality: signal frames and sigreturns stay in
    // the context for the program's syscall stub.
    store.data_mut().with_kernel(|k| k.wasm32 = true);
    // dlsym on the program's own symbols (a program linked -rdynamic:
    // GtkBuilder's signal handlers): every exported function gets a table
    // slot, as a shared library's do (dylink)
    let info = gex_dylink::Info::parse(&wasm)?;
    let len = (slots.len() as u32).max(info.slots().0);
    let n = crate::ftab::publish_stateful(&shared, memory, module, len, &pairs, crate::libprov::Extras { memory_base: mem_base })?;
    let path = store.data_mut().with_kernel(|k| k.exe_path.clone());
    {
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        let inst = s.ftab.map.install(n).clone();
        // What a door instance needs to run this program again on its memory
        // (`door`: a wineserver).
        crate::door::note_program(
            store.data_mut().with_kernel(|k| k.pid) as u32,
            crate::door::Prog {
                module: s.ftab.modules[inst.module as usize].clone(),
                memory_base: mem_base as u32,
                table_base: inst.base,
                table_len: len,
                thread_start: inst.base + slot("__gex_thread_start")?,
            },
        );
        let extra = info.extra_slots().into_iter().map(|(k, name)| (inst.base + k, name)).collect();
        // undefined weak symbols: null, as ELF leaves them
        let imports = info
            .static_weak_binds()
            .map_err(|e| anyhow!("{path}: {e}"))?
            .into_iter()
            .map(|(m, name, b)| {
                let nb = match b {
                    gex_dylink::Bind::GotMem(v) | gex_dylink::Bind::GotFunc(v) => crate::dylink::NBind::Global(v),
                    _ => crate::dylink::NBind::Missing(String::new()),
                };
                (m, name, nb)
            })
            .collect();
        s.ftab.dylink.insert(inst.module, Arc::new(crate::dylink::Binds { path: path.clone(), imports, extra, tls: None }));
        s.ftab.dyproc = Some(crate::dylink::DyProcess {
            link: gex_dylink::Link {
                mods: vec![gex_dylink::Loaded { name: path.clone(), path, info, memory_base: mem_base as u32, table_base: inst.base }],
            },
            module_of: vec![inst.module],
            ldpath: Vec::new(),
            origin: String::new(),
        });
    }
    // Instantiate it in this (the first) store now, and apply its data
    // relocations here. libprov applies them once per data address for the
    // whole host (`relocate_once`), which a program exec'd into a fresh
    // address space -- the same module at the same address -- would skip.
    // They are stores of fixed values, so applying them again is harmless.
    crate::ftab::catch_up(&mut *store)?;
    let inst = store.data().jit.as_ref().and_then(|j| j.instances.last().copied()).ok_or_else(|| anyhow!("wasm32 program: not instantiated"))?;
    if let Ok(f) = inst.get_typed_func::<(), ()>(&mut *store, "__wasm_apply_data_relocs") {
        f.call(&mut *store, ()).context("applying the wasm32 program's data relocations")?;
    }
    if std::env::var_os("GEX_WASM_TRACE").is_some() {
        eprintln!(
            "gex: wasm32 program: {} KiB wasm, data at {mem_base:#x}, entry {entry:#x}, compiled in {:.2}s",
            wasm.len() >> 10,
            t0.elapsed().as_secs_f64()
        );
    }
    Ok(true)
}
