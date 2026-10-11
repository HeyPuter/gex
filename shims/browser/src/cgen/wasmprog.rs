//! Native wasm32 programs in the browser (the native host's `wasmprog`,
//! docs/wine-native.md §3.11): a Linux program compiled to wasm32 against
//! gex's musl, in an ELF32 container whose header (0x40 bytes below the
//! entry) names the module. Nothing is translated: the module is installed
//! as a stateful provider (`PROVIDER | number` in the function table) at
//! the program's two entries, its data at the header's `__memory_base`.
//!
//! The protocol with `gex.mjs`:
//!   - `gex_load`, an `execve` and a fork's child stage the program
//!     (`stage`); `gex_wasmprog_serve()` (JS, before the guest runs) publishes
//!     it and returns its provider module number, or -1;
//!   - JS catches the table's log up, which instantiates the module from
//!     `gex_cgen_pmod_wasm_ptr/len` with `gex.syscall` = `#wasmprogSyscall`
//!     and asyncify's `gex.asyncify` / `gex.fork_point` (fork,
//!     `src/process/wasm32/__gex_fork.c` in the musl fork), and applies its
//!     data relocations where `gex_wasmprog_relocate` says (once, for a
//!     fresh image: not in a fork's child, whose data is the parent's);
//!   - its system calls never trap back to JS except to end the program's
//!     frames (exit, exec, a fatal signal): a wait parks the Worker in place
//!     (`lib.rs`, `wasm32_wait`), a thread or a fork is staged and returns,
//!     and `#wasmprogSyscall` hands it to the host.

use super::*;

pub(super) struct Prog {
    pub number: u32,
    pub wasm: Vec<u8>,
    pub relocate: bool,
}

pub(super) static PROGS: Mutex<Vec<Prog>> = Mutex::new(Vec::new());
/// The program to publish (entry, whether its data is fresh, the table base
/// it must have: a fork's child's, the parent's).
static STAGED: Mutex<Option<(u64, bool, Option<u32>)>> = Mutex::new(None);
/// This process's program's entry while it is a wasm32 one, for a fork.
pub(super) static CURRENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// And its module's `__table_base`.
pub(super) static CURRENT_BASE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub(crate) const MAGIC: &[u8; 8] = b"GEXWASM\0";

/// Whether `program` is a native wasm32 one (its header, from the image's
/// segments).
pub(crate) fn is_wasm32(program: &gex_ld::Program) -> bool {
    let at = program.exe.entry.wrapping_sub(0x40);
    program.exe.segments.iter().any(|s| {
        at >= s.vaddr && at + 8 <= s.vaddr + s.bytes.len() as u64 && {
            let o = (at - s.vaddr) as usize;
            &s.bytes[o..o + 8] == MAGIC
        }
    })
}

/// The process's image changed (load, exec): `program` is published before
/// it runs if it is a wasm32 one.
pub(crate) fn stage(program: Option<&gex_ld::Program>) {
    CURRENT.store(0, Ordering::Relaxed);
    *lock(&STAGED) = program.filter(|p| is_wasm32(p)).map(|p| (p.exe.entry, true, None));
}

/// A fork's child of a wasm32 program at `entry`: the same module, over the
/// parent's data.
#[no_mangle]
pub extern "C" fn gex_wasmprog_adopt(entry: u64) {
    if entry != 0 {
        *lock(&STAGED) = Some((entry, false, None));
    }
}

/// [`gex_wasmprog_adopt`] with the parent's table base
/// ([`gex_wasmprog_base`]): the data the child copied holds function
/// pointers for that base, so the module goes there.
#[no_mangle]
pub extern "C" fn gex_wasmprog_adopt_at(entry: u64, base: u32) {
    if entry != 0 {
        *lock(&STAGED) = Some((entry, false, (base != 0).then_some(base)));
    }
}

/// The running wasm32 program's `__table_base` (the fork record's).
#[no_mangle]
pub extern "C" fn gex_wasmprog_base() -> u32 {
    if CURRENT.load(Ordering::Relaxed) == 0 { 0 } else { CURRENT_BASE.load(Ordering::Relaxed) }
}

/// The running program's entry if it is a wasm32 one (the fork record's).
#[no_mangle]
pub extern "C" fn gex_wasmprog_entry() -> u64 {
    CURRENT.load(Ordering::Relaxed)
}

/// Publish the staged program: its provider module number, or -1.
#[no_mangle]
pub extern "C" fn gex_wasmprog_serve() -> i32 {
    let Some((entry, relocate, at)) = lock(&STAGED).take() else { return -1 };
    match publish(entry, relocate, at) {
        Ok(n) => n as i32,
        Err(e) => {
            crate::warn_line(&format!("gex: wasm32 program: {e}\n"));
            crate::with_host(|h| crate::exit_process(h, 127));
            -1
        }
    }
}

fn publish(entry: u64, relocate: bool, want_base: Option<u32>) -> Result<u32, String> {
    let mem = crate::guest_bytes();
    let at = entry.wrapping_sub(0x40) as usize;
    let h = mem.get(at..at + 36).ok_or("header outside memory")?;
    if &h[..8] != MAGIC {
        return Err("no GEXWASM header".into());
    }
    let w = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap()) as u64;
    if w(8) != 1 {
        return Err(format!("header version {} (want 1)", w(8)));
    }
    let (wasm_at, wasm_len, mem_base, thread_entry) = (w(12), w(16), w(20), w(32));
    let wasm = mem.get(wasm_at as usize..(wasm_at + wasm_len) as usize).ok_or("module outside memory")?.to_vec();
    // One that names shared libraries: gex's loader (`dylink`).
    if let Some(info) = super::dylink::wanted(&wasm) {
        let n = if relocate {
            super::dylink::install(wasm, info, mem_base as u32, entry, thread_entry)
        } else {
            super::dylink::adopt(wasm, entry, thread_entry)
        }?;
        CURRENT.store(entry, Ordering::Relaxed);
        return Ok(n);
    }
    let names = gex_cgen::provider::slot_names(&wasm).map_err(|e| e.to_string())?;
    let slot = |n: &str| names.iter().position(|s| s.as_deref() == Some(n)).map(|k| k as u32).ok_or(format!("{n} is not in its table"));
    let pairs = vec![(entry, slot("__gex_start")?), (thread_entry, slot("__gex_thread_start")?)];
    let len = super::dylink::static_program(&wasm, names.len() as u32);
    let number = lock(&PROVIDERS).len() as u32;
    let install = ftab(|m| match want_base {
        Some(base) => {
            let (n, ok) = m.publish_at(base, len, PROVIDER | number, Tier::Unit, &pairs);
            if !ok {
                crate::warn_line(&format!(
                    "gex: wasm32 fork child: table base {base} is taken, its function pointers will be wrong\n"
                ));
            }
            n
        }
        None => m.publish(len, PROVIDER | number, Tier::Unit, &pairs),
    });
    let base = ftab(|m| m.install(install).base);
    CURRENT_BASE.store(base, Ordering::Relaxed);
    let (regs, dirs, slots) = entry_bridges(&[entry, thread_entry]);
    let path = exe_path();
    super::dylink::static_published(number, &wasm, &path, base, mem_base as u32);
    lock(&PROVIDERS).push(ProviderModule { file: format!("wasmprog:{path}"), regs, dirs, slots, memory_base: mem_base as u32 });
    lock(&PROGS).push(Prog { number, wasm, relocate });
    CURRENT.store(entry, Ordering::Relaxed);
    Ok(number)
}

/// The program's path, with the kernel told the process is a wasm32 one
/// (gex-kernel's wasm32 personality: signal frames stay in the context for
/// the program's syscall stub).
pub(super) fn exe_path() -> String {
    crate::with_host(|h| {
        h.kernel.wasm32 = true;
        h.kernel.exe_path.clone()
    })
}

/// The bridges for a program's entries (`__gex_start`, `__gex_thread_start`,
/// whichever module defines them), their pages in the directory.
pub(super) fn entry_bridges(addrs: &[u64]) -> (Vec<u32>, Vec<(u64, u32)>, u64) {
    for &a in addrs {
        gex_kernel::ftab::fcache_clear(crate::guest_bytes(), a);
    }
    let (regs, mut dirs, slots) = bridges(addrs);
    // Every entry's page in the directory, not only the pages numbered now: a
    // fork's child has the parent's numbering (`gex_adopt_page`) but may have
    // had its directory rebuilt without it (`gex_reset_directory`).
    crate::with_host(|h| {
        for &a in addrs {
            let page = a / gex_abi::DISPATCH_PAGE;
            if let Some(b) = h.jit.page_table.lookup(page) {
                dirs.push((page, b));
            }
        }
    });
    (regs, dirs, slots)
}

/// Provider module `number`'s bytes when it is a wasm32 program.
pub(super) fn wasm(number: u32) -> Option<(u32, u32)> {
    lock(&PROGS).iter().find(|p| p.number == number).map(|p| (p.wasm.as_ptr() as u32, p.wasm.len() as u32))
}

/// Whether provider module `number` is a wasm32 program.
#[no_mangle]
pub extern "C" fn gex_cgen_pmod_wasmprog(number: u32) -> i32 {
    lock(&PROGS).iter().any(|p| p.number == number) as i32
}

/// Whether this instantiation of program `number` applies its data
/// relocations: once, for a fresh image.
#[no_mangle]
pub extern "C" fn gex_wasmprog_relocate(number: u32) -> i32 {
    lock(&PROGS).iter_mut().find(|p| p.number == number).is_some_and(|p| std::mem::replace(&mut p.relocate, false)) as i32
}
