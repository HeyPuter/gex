//! Native wasm32 programs in the browser (the native host's `wasmprog`):
//! a Linux program compiled to wasm32 against
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
    /// One Worker is applying the data relocations (`relocate` claimed and
    /// not yet [`gex_wasmprog_relocated`]).
    pub relocating: bool,
    /// The bytes' content key, made on first asking (`gex_cgen_pmod_wasm_key`).
    pub key: Option<(u64, u64)>,
    /// The file the bytes are from, for the system-wide key cache.
    pub path: String,
    /// Built LP64 for wasm64 and lowered to wasm32 (64-bit Wine):
    /// its base and stack-pointer globals are i64.
    pub lowered: bool,
    /// The bytes are file `path`'s as `dylink::read` gives them (a library),
    /// so they can go once the page holds the compiled module
    /// ([`gex_wasmprog_release`]) and be read again if a Worker asks.
    pub from_file: bool,
    /// Their length, kept when they go.
    pub len: u32,
    /// The file's identity (inode, mtime, size) when they went.
    pub gone: Option<(u64, i64, u64)>,
}

/// Whether `wasm` imports `env.__memory_base` as an i64: a module built for
/// wasm64 and lowered to wasm32 (decided by the import's type only).
pub(super) fn is_lowered(wasm: &[u8]) -> bool {
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        let Ok(wasmparser::Payload::ImportSection(r)) = payload else { continue };
        return r.into_iter().flatten().any(|i| {
            i.module == "env"
                && i.name == "__memory_base"
                && matches!(i.ty, wasmparser::TypeRef::Global(g) if g.content_type == wasmparser::ValType::I64)
        });
    }
    false
}

pub(crate) use gex_cgen::header::{abi_refusal, ABI_REFUSED};

pub(super) static PROGS: Mutex<Vec<Prog>> = Mutex::new(Vec::new());
/// The program to publish (entry, whether its data is fresh, the table base
/// it must have: a fork's child's, the parent's).
type Staged = (u64, bool, Option<u32>);
/// [`Staged`], boxed, or null: one slot, set by a load, exec or fork and
/// taken by whichever Worker serves next, with no lock (every syscall's
/// serve asks: a load of null).
static STAGED: std::sync::atomic::AtomicPtr<Staged> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

fn put_staged(v: Option<Staged>) {
    let p = v.map_or(std::ptr::null_mut(), |s| Box::into_raw(Box::new(s)));
    let old = STAGED.swap(p, Ordering::AcqRel);
    if !old.is_null() {
        // SAFETY: every non-null pointer in the slot came from
        // `Box::into_raw` above, and the swap made this one ours alone.
        drop(unsafe { Box::from_raw(old) });
    }
}

/// The provider module numbers of the program and its libraries, for
/// `cgen::on_exec` to kill at an `execve`: a Worker for a thread the new
/// program starts would otherwise instantiate them too, and their active
/// data segments would land over the new program's data at their old
/// places.
pub(crate) fn numbers() -> Vec<u32> {
    lock(&PROGS).iter().map(|p| p.number).collect()
}

/// `execve`: the old program's modules (it and its libraries) are gone with
/// it.
pub(crate) fn on_exec() {
    super::dylink::on_exec();
    lock(&PROGS).clear();
}

/// A program is staged for JS to publish (`gex_wasmprog_serve`).
pub(crate) fn staged_any() -> bool {
    !STAGED.load(Ordering::Acquire).is_null()
}

fn take_staged() -> Option<Staged> {
    if STAGED.load(Ordering::Acquire).is_null() {
        return None;
    }
    let p = STAGED.swap(std::ptr::null_mut(), Ordering::AcqRel);
    // SAFETY: as in `put_staged`: ours alone after the swap.
    (!p.is_null()).then(|| *unsafe { Box::from_raw(p) })
}
/// This process's program's entry while it is a wasm32 one, for a fork.
pub(super) static CURRENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// And its module's `__table_base`.
pub(super) static CURRENT_BASE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub(crate) const MAGIC: &[u8; 8] = b"GEXWASM\0";

/// Header flag (offset 36): the program runs i386 code (Wine, 32-bit Wine).
const FLAG_GUEST_I386: u32 = gex_cgen::image::program_flags::GUEST_I386;
/// Header flag: the program runs x86-64 code (64-bit native Wine, 64-bit Wine). It
/// shares 32-bit Wine's address-space split and trampoline page.
const FLAG_GUEST_X86_64: u32 = gex_cgen::image::program_flags::GUEST_X86_64;
/// Header flag, with [`FLAG_GUEST_I386`]: the program's i386 code is its own
/// (a JIT's, gexcc -fgex-guest-i386), not Windows': Linux's address space,
/// only the trampoline page added (the return pad of its calls into the
/// guest).
const FLAG_LINUX_SPACE: u32 = 8;

/// The running program's header has [`FLAG_GUEST_X86_64`] (64-bit Wine), set at
/// every publish (a fork's child has its parent's, in its copy of memory).
static GUEST_X86_64: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether this process runs 64-bit native Wine: what only 64-bit Wine has
/// been tested on stays off 32-bit Wine (it hung the shell's boot there).
pub(crate) fn guest_x86_64() -> bool {
    GUEST_X86_64.load(Ordering::Relaxed)
}

/// [`guest_x86_64`], for the host: 64-bit Wine's fault hook takes a 64-bit address
/// (Wine's `__wine_gex_fault`, signal_wasm64.c), 32-bit Wine's a 32-bit one.
#[no_mangle]
pub extern "C" fn gex_guest_x86_64() -> i32 {
    guest_x86_64() as i32
}

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
    put_staged(program.filter(|p| is_wasm32(p)).map(|p| (p.exe.entry, true, None)));
}

/// [`stage`] for an exec, which places its program from the files and knows
/// whether it is a wasm32 one by the placed bytes: its entry if so.
pub(crate) fn stage_entry(entry: Option<u64>) {
    CURRENT.store(0, Ordering::Relaxed);
    put_staged(entry.map(|e| (e, true, None)));
}

/// A fork's child of a wasm32 program at `entry`: the same module, over the
/// parent's data.
#[no_mangle]
pub extern "C" fn gex_wasmprog_adopt(entry: u64) {
    if entry != 0 {
        put_staged(Some((entry, false, None)));
    }
}

/// [`gex_wasmprog_adopt`] with the parent's table base
/// ([`gex_wasmprog_base`]): the data the child copied holds function
/// pointers for that base, so the module goes there.
#[no_mangle]
pub extern "C" fn gex_wasmprog_adopt_at(entry: u64, base: u32) {
    if entry != 0 {
        put_staged(Some((entry, false, (base != 0).then_some(base))));
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
    let Some((entry, relocate, at)) = take_staged() else { return -1 };
    match publish(entry, relocate, at) {
        Ok(n) => n as i32,
        Err(e) => {
            crate::warn_line(&format!("gex: wasm32 program: {e}\n"));
            crate::exit_process(127);
            -1
        }
    }
}

/// Whether the ntdll beside 64-bit Wine's loader `exe` is the native PE
/// core's skeleton, whose KUSER_SHARED_DATA reads ask gex where it is
/// (`gex.kuser_address`): an x86-64 ntdll reads it through a pointer at
/// Windows' address, which the compact space's fold does not reach.
fn wine64_native_ntdll(exe: &str) -> bool {
    let Some((dir, _)) = exe.rsplit_once("/x86_64-unix/") else { return false };
    crate::sigs::proc_home()
        .read_file(&format!("{dir}/x86_64-windows/ntdll.dll"))
        .and_then(|b| gex_ld::pe::parse(&b).ok().map(|pe| super::wasmdll::is_skeleton(&pe)))
        .unwrap_or(false)
}

fn publish(entry: u64, relocate: bool, want_base: Option<u32>) -> Result<u32, String> {
    let mem = crate::guest();
    let h = mem.read(entry.wrapping_sub(0x40), 36).map_err(|_| "header outside memory")?;
    if &h[..8] != MAGIC {
        return Err("no GEXWASM header".into());
    }
    let w = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap()) as u64;
    if w(8) != 1 {
        return Err(format!("header version {} (want 1)", w(8)));
    }
    let (wasm_at, wasm_len, mem_base, thread_entry) = (w(12), w(16), w(20), w(32));
    // The header's flags (offset 36; a version-1 header without them is 0):
    // Wine's loader (32-bit Wine) runs i386 code, so as natively (hosts/native
    // lib.rs) the process leaves Windows' fixed addresses free -- no mapping
    // above KUSER_SHARED_DATA unless asked for there -- and gets the NT
    // trampoline page native Wine's gexrt sends the guest to. Once per fresh
    // image: a fork's child has its parent's.
    let flags = mem.read_u32(entry.wrapping_sub(0x40) + 36).unwrap_or(0);
    GUEST_X86_64.store(flags & FLAG_GUEST_X86_64 != 0, Ordering::Relaxed);
    if flags & FLAG_LINUX_SPACE != 0 && relocate {
        use gex_kernel::win::{TRAMPOLINES, TRAMPOLINES_END};
        let page = gex_kernel::win::trampoline_page();
        let _ = crate::guest().write(TRAMPOLINES, &page);
        crate::sigs::proc_home().mm().lock().insert(TRAMPOLINES, TRAMPOLINES_END, gex_kernel::mem::prot::READ | gex_kernel::mem::prot::EXEC, "[nt-trampolines]");
    } else if flags & (FLAG_GUEST_I386 | FLAG_GUEST_X86_64) != 0 && relocate {
        use gex_kernel::win::{TRAMPOLINES, TRAMPOLINES_END};
        let page = gex_kernel::win::trampoline_page();
        let _ = crate::guest().write(TRAMPOLINES, &page);
        // 32-bit Wine's compact address space (`winspace::wine32_windows_space`): nothing
        // grown up front; Wine's views come from the arena bottom-up, shared
        // with the shim's heap, so the memory reaches only as high as used.
        // Or the 2 GiB split, for a site that asks (`?winesplit=1`: a game
        // whose views and the shim's heap do not fit together under 2 GiB).
        // A 64-bit Wine process in a wasm32 memory takes the compact space
        // too (`winspace::wine64_compact`) when its ntdll is the native PE
        // core's, which asks gex where KUSER_SHARED_DATA is; with another
        // ntdll, the split. Not in a process that has its space already (an
        // exec in the same memory keeps it).
        // (`gex_cgen::image::ProcessFlavour`: what `gexc cgen-flavour --page` names)
        let fresh = crate::winspace::windows_space() == 0;
        let pf = gex_cgen::image::ProcessFlavour {
            page: true,
            flags,
            mem64: gex_abi::mem64(),
            split: crate::winspace::split_asked(),
            native_ntdll: fresh && flags & FLAG_GUEST_X86_64 != 0 && wine64_native_ntdll(&crate::sigs::proc_home().exe()),
        };
        let compact64 = fresh && pf.compact64();
        if flags & FLAG_GUEST_X86_64 != 0 && !compact64 {
            crate::winspace::set_wine_split(true);
        }
        let lo = if compact64 {
            crate::winspace::wine64_compact_space()
        } else {
            crate::winspace::wine32_space().map_err(|e| format!("wine32: {e}"))?
        };
        if flags & FLAG_GUEST_I386 != 0 && pf.kuser_fold() {
            crate::winspace::wine32_kuser();
        }
        {
            let mut mm = crate::sigs::proc_home().mm().lock();
            crate::wine32_address_space(&mut mm, lo);
            if flags & FLAG_GUEST_X86_64 != 0 {
                crate::wine64_reserve_pe_data(&mut mm);
            }
            mm.insert(TRAMPOLINES, TRAMPOLINES_END, gex_kernel::mem::prot::READ | gex_kernel::mem::prot::EXEC, "[nt-trampolines]");
        }
    }
    let wasm = mem.read(wasm_at, wasm_len as usize).map_err(|_| "module outside memory")?.to_vec();
    // Wine's loader: refused when its inline probe reads another layout. Every
    // 32-bit guest program is Wine's loader; a 64-bit one is when it is bound
    // to the probe layout (abi_bound: its gex.abi section, or its gex.call_out
    // import), as gex's own x86-64 guest programs are not.
    if flags & FLAG_GUEST_I386 != 0 || (flags & FLAG_GUEST_X86_64 != 0 && gex_cgen::header::abi_bound(&wasm)) {
        if let Some(e) = abi_refusal(&wasm, &format!("Wine's loader {}", exe_path())) {
            return Err(e);
        }
    }
    // Parsed once for everything below. Three parses and a name for every
    // slot of the element segment (`slot_names`) made publishing bash 1.38 ms
    // of every fork child's start; this is 0.85 ms (demo page).
    let (info, libs) = match gex_dylink::Info::parse(&wasm).ok() {
        Some(i) if !i.needed.is_empty() => (None, Some(i)),
        other => (other, None),
    };
    // One that names shared libraries: gex's loader (`dylink`).
    if let Some(info) = libs {
        let n = if relocate {
            super::dylink::install(wasm, info, mem_base as u32, entry, thread_entry)
        } else {
            super::dylink::adopt(wasm, entry, thread_entry)
        }?;
        CURRENT.store(entry, Ordering::Relaxed);
        return Ok(n);
    }
    const ENTRIES: [&str; 2] = ["__gex_start", "__gex_thread_start"];
    let (found, elem_len) = named_slots(&wasm, ENTRIES)?;
    let slot = |k: usize| found[k].ok_or(format!("{} is not in its table", ENTRIES[k]));
    let pairs = vec![(entry, slot(0)?), (thread_entry, slot(1)?)];
    let len = super::dylink::static_program(info.as_ref(), elem_len);
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
    super::dylink::static_published(number, info, &path, base, mem_base as u32);
    // The bytes before the record: a Worker catching up takes the record
    // (`gex_cgen_pmod_ready`) as the sign that both are there.
    let lowered = is_lowered(&wasm);
    lock(&PROGS).push(Prog { number, len: wasm.len() as u32, wasm, relocate, relocating: false, key: None, path: path.clone(), lowered, from_file: false, gone: None });
    lock(&PROVIDERS).push(ProviderModule { file: format!("wasmprog:{path}"), regs, dirs, slots, memory_base: mem_base as u32 as u64 });
    CURRENT.store(entry, Ordering::Relaxed);
    Ok(number)
}

/// The element segment slots of the functions named `names` (as
/// `gex_cgen::provider::slot_names` numbers them: every segment's
/// functions in order), and how many slots the segments fill. Only the
/// wanted names are compared; none is copied.
fn named_slots<const N: usize>(wasm: &[u8], names: [&str; N]) -> Result<([Option<u32>; N], u32), String> {
    let mut elems: Vec<u32> = Vec::new();
    let mut funcs = [None; N];
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        match payload.map_err(|e| e.to_string())? {
            wasmparser::Payload::ElementSection(r) => {
                for el in r {
                    if let wasmparser::ElementItems::Functions(fs) = el.map_err(|e| e.to_string())?.items {
                        for f in fs {
                            elems.push(f.map_err(|e| e.to_string())?);
                        }
                    }
                }
            }
            wasmparser::Payload::CustomSection(c) if c.name() == "name" => {
                let reader = wasmparser::NameSectionReader::new(wasmparser::BinaryReader::new(c.data(), c.data_offset()));
                for n in reader.flatten() {
                    if let wasmparser::Name::Function(map) = n {
                        for naming in map.into_iter().flatten() {
                            if let Some(k) = names.iter().position(|&w| w == naming.name) {
                                funcs[k] = Some(naming.index);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let slots = funcs.map(|f| f.and_then(|f| elems.iter().position(|&e| e == f)).map(|k| k as u32));
    Ok((slots, elems.len() as u32))
}

/// The program's path, as published at the load or exec, with the process
/// marked a wasm32 one (gex-kernel's wasm32 personality: signal frames stay
/// in the context for the program's syscall stub). `WASM32` is what says so;
/// the kernel's own flag follows it at each locked call (`gex_syscall`).
pub(super) fn exe_path() -> String {
    crate::WASM32.store(true, std::sync::atomic::Ordering::Release);
    crate::sigs::proc_home().exe()
}

/// The bridges for a program's entries (`__gex_start`, `__gex_thread_start`,
/// whichever module defines them), their pages in the directory.
pub(super) fn entry_bridges(addrs: &[u64]) -> (Vec<u32>, Vec<(u64, u32)>, u64) {
    let mut mem = crate::guest();
    for &a in addrs {
        gex_kernel::ftab::fcache_clear(&mut mem, a);
    }
    let (regs, mut dirs, slots) = bridges(addrs);
    // Every entry's page in the directory, not only the pages numbered now: a
    // fork's child has the parent's numbering (`gex_adopt_page`) but may have
    // had its directory rebuilt without it (`gex_reset_directory`).
    crate::with_jit_only(|j| {
        for &a in addrs {
            let page = a / gex_abi::DISPATCH_PAGE;
            if let Some(b) = j.page_table.lookup(page) {
                dirs.push((page, b));
            }
        }
    });
    (regs, dirs, slots)
}

/// Provider module `number`'s bytes in the shim's heap when it is a wasm32
/// program whose bytes are still held (a library's go once it is
/// instantiated, [`gex_wasmprog_release`]; [`owned`] reads them again).
pub(super) fn wasm(number: u32) -> Option<(u32, u32)> {
    let g = lock(&PROGS);
    let p = g.iter().find(|p| p.number == number)?;
    Some((if p.gone.is_some() { 0 } else { p.wasm.as_ptr() as u32 }, p.len))
}

/// Provider module `number`'s length when it is a wasm32 program, held or
/// not.
pub(super) fn len(number: u32) -> Option<u32> {
    lock(&PROGS).iter().find(|p| p.number == number).map(|p| p.len)
}

/// A copy of provider module `number`'s bytes that the caller owns, when it
/// is a wasm32 program: copied under the lock while they are held, else
/// read again from the file they came from, which must be the same file
/// (dpkg's rename gives a new inode). A pointer into the held bytes would
/// not do: another Worker of the process instantiating the same module
/// releases them, which freed them under a Worker still copying them out.
pub(super) fn owned(number: u32) -> Option<Vec<u8>> {
    let (path, id, len) = {
        let g = lock(&PROGS);
        let p = g.iter().find(|p| p.number == number)?;
        match p.gone {
            None => return Some(p.wasm.clone()),
            Some(id) => (p.path.clone(), id, p.len),
        }
    };
    let same = file_id(&path).is_some_and(|(_, ino, mtime, size)| (ino, mtime, size) == id);
    let b = super::dylink::read(&path).filter(|b| same && b.len() as u32 == len);
    if b.is_none() {
        crate::warn_line(&format!("gex: {path}: changed since it was loaded\n"));
    }
    b
}

/// [`owned`] for JS: the bytes in a block of exactly the module's length,
/// which the caller gives back with `gex_free(ptr, len)`; 0 when there are
/// none.
#[no_mangle]
pub extern "C" fn gex_wasmprog_owned(number: u32) -> u32 {
    match owned(number) {
        Some(b) if !b.is_empty() => {
            let b = b.into_boxed_slice();
            Box::into_raw(b) as *mut u8 as u32
        }
        _ => 0,
    }
}

/// What stands for a library's bytes once they went at load
/// ([`shed`]): their length, content key and file identity.
pub(super) struct Shed {
    pub len: u32,
    pub key: (u64, u64),
    pub gone: (u64, i64, u64),
}

/// A library's bytes, parsed at load, given up for what [`wasm`] needs to
/// read them again: a Qt program's libraries are 150 to 300 MiB, and held
/// at once they were its heap's high-water mark for the life of the
/// process (and a fork's child's). `None` (keep them) without a file to
/// read again.
pub(super) fn shed(path: &str, bytes: &[u8]) -> Option<Shed> {
    let (_, ino, mtime, size) = file_id(path)?;
    if size != bytes.len() as u64 {
        return None;
    }
    Some(Shed { len: bytes.len() as u32, key: file_key(path, bytes), gone: (ino, mtime, size) })
}

/// [`shed`] for a library whose bytes were not read: a process made its
/// file's key already ([`known_key`]), so nothing here needs them. `None`
/// for a file no process has hashed (it is read whole, and hashed then).
pub(super) fn shed_known(path: &str) -> Option<Shed> {
    let id = file_id(path)?;
    let (_, ino, mtime, size) = id;
    let len = u32::try_from(size).ok()?;
    let key = known_key(&id, len)?;
    Some(Shed { len, key, gone: (ino, mtime, size) })
}

/// The page holds wasm32 program module `number` compiled (JS, once the
/// Worker that published it has instantiated it): a library's bytes go.
/// They are most of a Qt program's heap (150 to 200 MiB for Plasma's), and
/// a fork copied them again.
#[no_mangle]
pub extern "C" fn gex_wasmprog_release(number: u32) {
    let mut g = lock(&PROGS);
    let Some(p) = g.iter_mut().find(|p| p.number == number && p.from_file && p.gone.is_none()) else { return };
    // read again only from the same file (dpkg's rename gives a new inode)
    let Some((_, ino, mtime, size)) = file_id(&p.path) else { return };
    p.gone = Some((ino, mtime, size));
    p.wasm = Vec::new();
}

/// A content key for `b`: two 64-bit lanes over its little-endian words, then
/// the tail. Made here, once per module and process, rather than in JS, which
/// cost about 35 ms of every apt process start (apt-multi is 14 MB) in a
/// trace of `apt-get update`, and copied the bytes out first even when the
/// page already had the module.
pub(super) fn content_key(b: &[u8]) -> (u64, u64) {
    let (mut a, mut c) = (0xcbf2_9ce4_8422_2325u64, 0x9e37_79b9_7f4a_7c15u64);
    let words = b.chunks_exact(8);
    let tail = words.remainder();
    for (i, w) in words.enumerate() {
        let x = u64::from_le_bytes(w.try_into().unwrap());
        a = (a ^ x).wrapping_mul(0x0000_0100_0000_01b3);
        c = (c ^ x.rotate_left(29) ^ i as u64).wrapping_mul(0xff51_afd7_ed55_8ccd);
        c ^= c >> 32;
    }
    for (i, &x) in tail.iter().enumerate() {
        a = (a ^ x as u64).wrapping_mul(0x0000_0100_0000_01b3);
        c = (c ^ ((x as u64) << (i * 8)) ^ 0xa5).wrapping_mul(0xff51_afd7_ed55_8ccd);
        c ^= c >> 32;
    }
    (a, c)
}

/// Lane `lane` (0 or 1) of wasm32 program `number`'s content key, or of
/// Wine's PE module's, or 0 when it is neither.
#[no_mangle]
pub extern "C" fn gex_cgen_pmod_wasm_key(number: u32, lane: u32) -> u64 {
    let mut g = lock(&PROGS);
    let k = match g.iter_mut().find(|p| p.number == number) {
        Some(p) => *p.key.get_or_insert_with(|| file_key(&p.path, &p.wasm)),
        None => {
            drop(g);
            match super::wasmdll::wasm_key(number) {
                Some(k) => k,
                None => return 0,
            }
        }
    };
    if lane == 0 { k.0 } else { k.1 }
}

/// The content key of `wasm`, the bytes of file `path`: from the system-wide
/// cache when some process already made it for the same file, else made and
/// noted there.
pub(super) fn file_key(path: &str, wasm: &[u8]) -> (u64, u64) {
    let id = file_id(path);
    let len = wasm.len() as u32;
    id.as_ref().and_then(|id| known_key(id, len)).unwrap_or_else(|| {
        let k = content_key(wasm);
        if let Some(id) = &id {
            note_key(id, len, k);
        }
        k
    })
}

/// Which file a program's bytes are from, as the system-wide key cache names
/// it: (path, inode, mtime, size). `None` without a global kernel or a file.
fn file_id(path: &str) -> Option<(&str, u64, i64, u64)> {
    if path.is_empty() || unsafe { crate::k_present() } == 0 {
        return None;
    }
    let m = crate::sigs::proc_home().ns().metadata(path).ok()?;
    Some((path, m.ino, m.mtime, m.size))
}

/// A program's content key as some process already made it, by its file:
/// hashing the bytes again was 6-12 ms of every `apt-get` exec. A file whose
/// identity has changed (dpkg's rename gives a new inode) is a miss, hashed
/// and noted again.
fn known_key(id: &(&str, u64, i64, u64), len: u32) -> Option<(u64, u64)> {
    let (path, ino, mtime, size) = *id;
    let mut out = [0u64; 2];
    let found = unsafe {
        crate::k_progkey_get(path.as_ptr() as gex_kernel::PAddr, path.len() as u32, ino, mtime, size, len, out.as_mut_ptr() as gex_kernel::PAddr)
    };
    (found == 1).then_some((out[0], out[1]))
}

/// Note `key` as the content key of the program `id` names.
fn note_key(id: &(&str, u64, i64, u64), len: u32, key: (u64, u64)) {
    let (path, ino, mtime, size) = *id;
    unsafe { crate::k_progkey_put(path.as_ptr() as gex_kernel::PAddr, path.len() as u32, ino, mtime, size, len, key.0, key.1) };
}

/// Whether provider module `number` is a wasm32 program.
#[no_mangle]
pub extern "C" fn gex_cgen_pmod_wasmprog(number: u32) -> i32 {
    lock(&PROGS).iter().any(|p| p.number == number) as i32
}

/// Whether provider module `number` is a wasm32 program lowered from wasm64
/// ([`is_lowered`]).
#[no_mangle]
pub extern "C" fn gex_cgen_pmod_lowered(number: u32) -> i32 {
    lock(&PROGS).iter().any(|p| p.number == number && p.lowered) as i32
}

/// Whether this instantiation of program `number` applies its data
/// relocations: once, for a fresh image. While another Worker applies them
/// this waits for it to finish ([`gex_wasmprog_relocated`]): a module is
/// run as soon as it is instantiated (a `dlopen`'s constructors and the
/// caller's first calls), and before then its function pointers in data
/// (vtables) are offsets into its own table range, other modules' slots.
#[no_mangle]
pub extern "C" fn gex_wasmprog_relocate(number: u32) -> i32 {
    let mut waits: u64 = 0;
    loop {
        {
            let mut g = lock(&PROGS);
            let Some(p) = g.iter_mut().find(|p| p.number == number) else { return 0 };
            if std::mem::replace(&mut p.relocate, false) {
                p.relocating = true;
                return 1;
            }
            if !p.relocating {
                return 0;
            }
        }
        waits += 1;
        if waits % 10_000 == 0 {
            crate::warn_line(&format!("gex: wasm32 module {number}: still waiting for another Worker to relocate its data\n"));
        }
        #[cfg(feature = "shared-memory")]
        {
            let nap = std::sync::atomic::AtomicI32::new(0);
            unsafe { crate::warch::memory_atomic_wait32(nap.as_ptr(), 0, 100_000) };
        }
    }
}

/// The Worker [`gex_wasmprog_relocate`] picked has applied program
/// `number`'s data relocations.
#[no_mangle]
pub extern "C" fn gex_wasmprog_relocated(number: u32) {
    if let Some(p) = lock(&PROGS).iter_mut().find(|p| p.number == number) {
        p.relocating = false;
    }
}
