//! What gex does when an executable mapping of a guest library appears (a `mmap` with `PROT_EXEC` of a file): bind
//! functions of it to the host's, before any of its code runs.
//!
//! The guest's libc keeps its own code but not its heap: scudo reserves gigabytes of address space at start, which
//! a 4 GiB memory shared with the host can't give, so `malloc` and the rest are the host's dlmalloc, and scudo's
//! entry points are no-ops so it never starts. And libc is where `__aw_thread_attach` is, which adopts host threads.

use crate::cpu;
use std::collections::HashMap;
use std::sync::Mutex;

/// Paths of the guest's open file descriptors, so a mapping can be named.
static PATHS: Mutex<Option<HashMap<i32, String>>> = Mutex::new(None);

pub fn opened(fd: i32, path: String) {
    PATHS.lock().unwrap().get_or_insert_with(HashMap::new).insert(fd, path);
}

pub fn closed(fd: i32) {
    if let Some(m) = PATHS.lock().unwrap().as_mut() {
        m.remove(&fd);
    }
}

/// A library whose functions are the host's (`gex_embed_provide`).
struct Provider {
    fns: HashMap<String, (crate::imports::HostFn, u32)>,
    /// For every exported function the table doesn't name; none: only the named ones are bound.
    missing: Option<crate::imports::HostFn>,
}

static PROVIDERS: Mutex<Option<HashMap<String, Provider>>> = Mutex::new(None);

/// When a library named `soname` is mapped executable, bind its functions to the host's: the table's, by name, and,
/// with `missing`, every other function it exports, to `missing` with the name ("soname!symbol", NUL-terminated,
/// never freed) as its user value.
pub fn provide(soname: &str, fns: Vec<(String, crate::imports::HostFn, u32)>, missing: Option<crate::imports::HostFn>) {
    let fns = fns.into_iter().map(|(n, f, u)| (n, (f, u))).collect();
    PROVIDERS.lock().unwrap().get_or_insert_with(HashMap::new).insert(soname.to_string(), Provider { fns, missing });
}

/// A library a provider stands for was mapped at load bias `bias`.
fn bind_provided(soname: &str, e: &crate::elf::Elf, bias: u64) {
    let guard = PROVIDERS.lock().unwrap();
    let Some(p) = guard.as_ref().and_then(|m| m.get(soname)) else { return };
    let (mut bound, mut missing) = (0, 0);
    for (name, value) in e.exported_functions() {
        let Ok(name) = std::str::from_utf8(name) else { continue };
        if let Some(&(f, user)) = p.fns.get(name) {
            crate::bind(bias + value, f, user);
            bound += 1;
        } else if let Some(m) = p.missing {
            let label = format!("{soname}!{name}\0").leak();
            crate::bind(bias + value, m, label.as_ptr() as usize as u32);
            missing += 1;
        }
    }
    for name in p.fns.keys() {
        if !e.exported_functions().iter().any(|(n, _)| *n == name.as_bytes()) {
            crate::host::log(&format!("gex-embed: {soname} has no {name} to bind\n"));
        }
    }
    crate::host::log(&format!("gex-embed: {soname} at {bias:#x}: {bound} functions bound to the host, {missing} to the missing handler\n"));
}

/// `fd` (`path`) was just mapped executable at `addr`, from file offset `off`.
pub fn exec_mapped(fd: i32, addr: u64, off: u64) {
    let Some(path) = PATHS.lock().unwrap().as_ref().and_then(|m| m.get(&fd).cloned()) else { return };
    let soname = path.rsplit('/').next().unwrap_or(&path).to_string();
    let provided = PROVIDERS.lock().unwrap().as_ref().is_some_and(|m| m.contains_key(&soname));
    let Ok(bytes) = crate::elf::read_file(&path) else { return };
    let Ok(e) = crate::elf::Elf::parse(&bytes) else { return };
    // The executable segment this offset is the start of gives the load bias. (Only executable ones: a small library's
    // code can share its first page of the file with the read-only segment before it.)
    let Some(seg) = e.phdrs().into_iter().find(|p| p.kind == 1 && p.flags & 1 != 0 && p.offset & !(crate::vm::PAGE - 1) == off)
    else {
        return;
    };
    let bias = addr - (seg.vaddr & !(crate::vm::PAGE - 1));
    // Where the code went, for naming addresses (diag).
    crate::diag::mapped(addr, bias + seg.vaddr + seg.memsz, &path, bias);
    if soname != "libc.so" && !provided {
        return;
    }
    if provided {
        bind_provided(&soname, &e, bias);
    }
    if soname != "libc.so" {
        return;
    }
    let syms = e.symbols();
    let mut redirected = 0;
    for (name, value) in &syms {
        let Ok(name) = std::str::from_utf8(name) else { continue };
        let f: Option<crate::imports::HostFn> = match name {
            "malloc" | "scudo_malloc" => Some(h_malloc),
            "free" | "scudo_free" => Some(h_free),
            "calloc" | "scudo_calloc" => Some(h_calloc),
            "realloc" | "scudo_realloc" => Some(h_realloc),
            "memalign" | "scudo_memalign" | "aligned_alloc" | "scudo_aligned_alloc" => Some(h_memalign),
            "posix_memalign" | "scudo_posix_memalign" => Some(h_posix_memalign),
            "malloc_usable_size" | "scudo_malloc_usable_size" => Some(h_usable_size),
            "valloc" | "scudo_valloc" | "pvalloc" | "scudo_pvalloc" => Some(h_valloc),
            n if n.starts_with("scudo_") => Some(h_zero),
            _ => None,
        };
        if let Some(f) = f {
            crate::bind(bias + value, f, 0);
            redirected += 1;
        }
        if name == "__aw_thread_attach" {
            crate::thread::ATTACH.store(bias + value, std::sync::atomic::Ordering::Release);
        }
    }
    crate::host::log(&format!("gex-embed: {path} at {:#x}, {redirected} functions bound to the host\n", bias));
}

extern "C" {
    fn malloc(n: usize) -> *mut u8;
    fn calloc(n: usize, s: usize) -> *mut u8;
    fn realloc(p: *mut u8, n: usize) -> *mut u8;
    fn memalign(a: usize, n: usize) -> *mut u8;
    fn free(p: *mut u8);
    fn malloc_usable_size(p: *mut u8) -> usize;
}

/// A size the guest passed, as the host's (anything past 4 GiB can't be had).
fn size(v: u64) -> Option<usize> {
    (v < 1 << 32).then_some(v as usize)
}

fn ret(c: u32, p: *mut u8) {
    cpu::set_x(c, 0, p as usize as u64);
}

extern "C" fn h_malloc(c: u32, _: u32) {
    let p = size(cpu::x(c, 0)).map_or(core::ptr::null_mut(), |n| unsafe { malloc(n) });
    ret(c, p);
}

extern "C" fn h_free(c: u32, _: u32) {
    unsafe { free(cpu::x(c, 0) as usize as *mut u8) };
}

extern "C" fn h_calloc(c: u32, _: u32) {
    let p = match (size(cpu::x(c, 0)), size(cpu::x(c, 1))) {
        (Some(n), Some(s)) => unsafe { calloc(n, s) },
        _ => core::ptr::null_mut(),
    };
    ret(c, p);
}

extern "C" fn h_realloc(c: u32, _: u32) {
    let p = size(cpu::x(c, 1)).map_or(core::ptr::null_mut(), |n| unsafe { realloc(cpu::x(c, 0) as usize as *mut u8, n) });
    ret(c, p);
}

extern "C" fn h_memalign(c: u32, _: u32) {
    let p = match (size(cpu::x(c, 0)), size(cpu::x(c, 1))) {
        (Some(a), Some(n)) => unsafe { memalign(a.max(8), n) },
        _ => core::ptr::null_mut(),
    };
    ret(c, p);
}

extern "C" fn h_posix_memalign(c: u32, _: u32) {
    let (out, a, n) = (cpu::x(c, 0), cpu::x(c, 1), cpu::x(c, 2));
    if !a.is_power_of_two() || a % 8 != 0 {
        return cpu::set_x(c, 0, 22);
    }
    let p = size(n).map_or(core::ptr::null_mut(), |n| unsafe { memalign(a as usize, n) });
    if p.is_null() {
        return cpu::set_x(c, 0, 12);
    }
    // SAFETY: the guest's out-parameter, an 8-byte pointer.
    unsafe { (out as usize as *mut u64).write_unaligned(p as usize as u64) };
    cpu::set_x(c, 0, 0);
}

extern "C" fn h_usable_size(c: u32, _: u32) {
    let p = cpu::x(c, 0) as usize as *mut u8;
    cpu::set_x(c, 0, if p.is_null() { 0 } else { (unsafe { malloc_usable_size(p) }) as u64 });
}

extern "C" fn h_valloc(c: u32, _: u32) {
    let p = size(cpu::x(c, 0)).map_or(core::ptr::null_mut(), |n| unsafe { memalign(4096, n) });
    ret(c, p);
}

extern "C" fn h_zero(c: u32, _: u32) {
    cpu::set_x(c, 0, 0);
}

/// The default for a provided library's functions the host has no implementation of: say which, once, and return 0.
pub extern "C" fn missing(c: u32, label: u32) {
    static SEEN: Mutex<Option<std::collections::HashSet<u32>>> = Mutex::new(None);
    if SEEN.lock().unwrap().get_or_insert_with(Default::default).insert(label) {
        // SAFETY: the NUL-terminated label `bind_provided` made.
        let name = unsafe { std::ffi::CStr::from_ptr(label as usize as *const core::ffi::c_char) };
        crate::host::log(&format!("gex-embed: {} is not implemented (returns 0)\n", name.to_string_lossy()));
    }
    cpu::set_x(c, 0, 0);
}
