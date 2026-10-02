//! Process creation: what Wine's unix `start_main_thread` does before the PE
//! side runs, done in Rust (docs/nt-layer.md §1).
//!
//! [`boot`] maps the program and `ntdll.dll`, fills `KUSER_SHARED_DATA`,
//! writes the NT trampolines, builds the PEB and `RTL_USER_PROCESS_PARAMETERS`,
//! maps the API set schema, creates the main thread's TEB and stack, and sets
//! the thread's registers to enter `LdrInitializeThunk`. Everything after that
//! is Wine's PE loader.

use super::object::Obj;
use super::section::{self, opt_extra};
use super::vm::{self, page_prot, Kind, GRANULE, PAGE};
use super::{off, st, to_w, Abi, Process, Status, G, R, TRAMPOLINES, TRAMPOLINES_END, USD, WAIT_WORDS, WAIT_WORDS_END};
use crate::{mem::prot, Kernel};
use gex_ld::pe::{self, PeImage};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Addresses in ntdll the kernel enters directly.
#[derive(Clone, Debug, Default)]
pub struct NtdllEntries {
    pub base: u64,
    pub ldr_initialize_thunk: u64,
    pub rtl_user_thread_start: u64,
    pub ki_user_exception_dispatcher: u64,
    pub ki_user_apc_dispatcher: u64,
    pub ki_user_callback_dispatcher: u64,
}

/// The trampolines gex writes at [`TRAMPOLINES`] (docs/nt-layer.md §1.2).
pub mod tramp {
    use super::TRAMPOLINES;
    /// `__wine_unix_call_dispatcher` for x86-64:
    /// `mov r10, rcx; mov eax, UNIX_CALL; syscall; ret`.
    pub const UNIX_CALL_64: u64 = TRAMPOLINES;
    /// `__wine_syscall_dispatcher` for x86-64. Wine's x86-64 stubs never
    /// reach it while `USD.SystemCall` is 0; if anything does, it reports
    /// itself: `mov eax, BAD_DISPATCH; syscall; ud2`.
    pub const SYSCALL_DISPATCHER_64: u64 = TRAMPOLINES + 0x20;
    /// `__wine_syscall_dispatcher` for i386 (docs/nt-layer.md §7): the stub
    /// did `mov eax, id; call __wine_syscall`, which jumps here, so the
    /// arguments are above two return addresses: `int 0x80; ret`.
    pub const SYSCALL_DISPATCHER_32: u64 = TRAMPOLINES + 0x40;
    /// `__wine_unix_call_dispatcher` for i386, a stdcall of (handle: u64,
    /// code, args). The inner `call` gives it the syscall stubs' stack shape
    /// (two return addresses above the arguments):
    /// `mov eax, UNIX_CALL; call 1f; ret 16; 1: int 0x80; ret`.
    pub const UNIX_CALL_32: u64 = TRAMPOLINES + 0x60;
}

pub fn trampoline_page() -> Vec<u8> {
    let mut page = vec![0xccu8; (TRAMPOLINES_END - TRAMPOLINES) as usize];
    let put = |page: &mut Vec<u8>, at: u64, code: &[u8]| {
        let o = (at - TRAMPOLINES) as usize;
        page[o..o + code.len()].copy_from_slice(code);
    };
    let n = super::gex_nr::UNIX_CALL.to_le_bytes();
    put(&mut page, tramp::UNIX_CALL_64, &[0x4c, 0x8b, 0xd1, 0xb8, n[0], n[1], n[2], n[3], 0x0f, 0x05, 0xc3]);
    let b = super::gex_nr::BAD_DISPATCH.to_le_bytes();
    put(&mut page, tramp::SYSCALL_DISPATCHER_64, &[0xb8, b[0], b[1], b[2], b[3], 0x0f, 0x05, 0x0f, 0x0b]);
    put(&mut page, tramp::SYSCALL_DISPATCHER_32, &[0xcd, 0x80, 0xc3]);
    put(
        &mut page,
        tramp::UNIX_CALL_32,
        &[0xb8, n[0], n[1], n[2], n[3], 0xe8, 0x03, 0x00, 0x00, 0x00, 0xc2, 0x10, 0x00, 0xcd, 0x80, 0xc3],
    );
    page
}

/// The trampoline page as a recovery input for the function tier: one
/// executable segment of [`trampoline_page`]'s bytes at [`TRAMPOLINES`], and
/// the dispatchers of the process's ABI (`class32`: i386) to start recovery
/// at. Every host builds its trampoline unit from exactly this, so they
/// compute the same unit key.
pub fn trampoline_input(class32: bool) -> (gex_ld::Image, Vec<u64>) {
    let bytes = trampoline_page();
    let starts: Vec<u64> = if class32 {
        vec![tramp::UNIX_CALL_32, tramp::SYSCALL_DISPATCHER_32]
    } else {
        vec![tramp::UNIX_CALL_64, tramp::SYSCALL_DISPATCHER_64]
    };
    let image = gex_ld::Image {
        soname: None,
        needed: Vec::new(),
        runpath: Vec::new(),
        bias: TRAMPOLINES,
        entry: starts[0],
        code: vec![(TRAMPOLINES, TRAMPOLINES_END)],
        segments: vec![gex_ld::elf::Segment {
            vaddr: TRAMPOLINES,
            mem_size: bytes.len() as u64,
            bytes,
            readable: true,
            writable: false,
            executable: true,
        }],
        phdr_vaddr: TRAMPOLINES,
        phentsize: 0,
        phnum: 0,
        brk: TRAMPOLINES_END,
        text_range: (TRAMPOLINES, TRAMPOLINES_END),
        tls: None,
        symbols: Vec::new(),
        named_relocs: Vec::new(),
        relocs: Vec::new(),
        init_array: Vec::new(),
        dynsyms: Vec::new(),
        irelative: Vec::new(),
        pie: false,
        interp: None,
        class32,
    };
    (image, starts)
}

/// What `gexc run` asks for.
pub struct BootRequest<'a> {
    /// The program, as a guest (VFS) path.
    pub exe_path: &'a str,
    /// Its arguments after argv[0].
    pub args: &'a [String],
}

/// The Windows environment Wine's prefix would give a program (user `gex`),
/// before a host's overrides.
pub fn default_environment() -> Vec<(String, String)> {
    [
        ("ALLUSERSPROFILE", "C:\\ProgramData"),
        ("APPDATA", "C:\\users\\gex\\AppData\\Roaming"),
        ("COMPUTERNAME", "GEX"),
        ("ComSpec", "C:\\windows\\system32\\cmd.exe"),
        ("LOCALAPPDATA", "C:\\users\\gex\\AppData\\Local"),
        ("NUMBER_OF_PROCESSORS", "4"),
        ("OS", "Windows_NT"),
        ("PATH", "C:\\windows\\system32;C:\\windows;C:\\windows\\system32\\wbem"),
        ("PATHEXT", ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC"),
        ("PROCESSOR_ARCHITECTURE", "AMD64"),
        ("ProgramData", "C:\\ProgramData"),
        ("ProgramFiles", "C:\\Program Files"),
        ("PUBLIC", "C:\\users\\Public"),
        ("SystemDrive", "C:"),
        ("SystemRoot", "C:\\windows"),
        ("TEMP", "C:\\users\\gex\\AppData\\Local\\Temp"),
        ("TMP", "C:\\users\\gex\\AppData\\Local\\Temp"),
        ("USERNAME", "gex"),
        // What Wine's GetUserNameW reads (its loader sets it from the Unix
        // user); shell32 builds profile paths from it.
        ("WINEUSERNAME", "gex"),
        ("USERPROFILE", "C:\\users\\gex"),
        ("windir", "C:\\windows"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// The PE ABI of an executable's bytes, or `None` if it is not one gex runs.
pub fn exe_abi(bytes: &[u8]) -> Option<Abi> {
    let img = pe::parse(bytes).ok()?;
    match img.machine {
        pe::MACHINE_AMD64 => Some(Abi::P64),
        pe::MACHINE_I386 => Some(Abi::P32),
        _ => None,
    }
}

/// A guest path as a DOS path: under the prefix's `drive_c` it is `C:\…`,
/// anything else is `Z:\…`.
pub fn dos_path(prefix: &str, guest: &str) -> String {
    let c = format!("{prefix}/drive_c");
    let (drive, rest) = match guest.strip_prefix(&c) {
        Some(r) if r.is_empty() || r.starts_with('/') => ("C:", r),
        _ => ("Z:", guest),
    };
    let rest = rest.replace('/', "\\");
    if rest.is_empty() {
        format!("{drive}\\")
    } else {
        format!("{drive}{rest}")
    }
}

/// Windows' quoting for one command-line argument.
fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut s = String::from("\"");
    let mut backslashes = 0;
    for ch in a.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                s.extend(std::iter::repeat_n('\\', 2 * backslashes + 1));
                s.push('"');
                backslashes = 0;
            }
            _ => {
                s.extend(std::iter::repeat_n('\\', backslashes));
                s.push(ch);
                backslashes = 0;
            }
        }
    }
    s.extend(std::iter::repeat_n('\\', 2 * backslashes));
    s.push('"');
    s
}

fn read_file(k: &Kernel, path: &str) -> Result<Vec<u8>, String> {
    super::file::read_guest_file(k, path).map_err(|e| format!("reading {path}: {e:?}"))
}

fn st_err(what: &str) -> impl Fn(Status) -> String + '_ {
    move |s| format!("{what}: NTSTATUS {:#010x}", s.0)
}

/// Reserve and commit a fresh private region from the NT allocator.
fn alloc(p: &Process, k: &mut Kernel, size: u64, name: &str) -> R<u64> {
    let size = (size + PAGE - 1) & !(PAGE - 1);
    let mut stl = p.st.lock().unwrap();
    let b = stl.vm.find_free((size + GRANULE - 1) & !(GRANULE - 1), false, vm::ALLOC_HIGH).ok_or(Status(st::NO_MEMORY))?;
    stl.vm.reserve(b, size, page_prot::READWRITE, true, Kind::Private, name);
    stl.vm.mirror(&mut k.mem, b, b + size);
    Ok(b)
}

/// The exports the kernel needs from ntdll, and where its three variables
/// are.
fn ntdll_exports(img: &PeImage, base: u64) -> Result<(NtdllEntries, [u64; 3]), String> {
    let find = |name: &str| -> Result<u64, String> {
        img.exports
            .entries
            .iter()
            .find(|e| e.names.iter().any(|n| n == name))
            .map(|e| base + e.rva as u64)
            .ok_or_else(|| format!("ntdll.dll has no export {name}"))
    };
    Ok((
        NtdllEntries {
            base,
            ldr_initialize_thunk: find("LdrInitializeThunk")?,
            rtl_user_thread_start: find("RtlUserThreadStart")?,
            ki_user_exception_dispatcher: find("KiUserExceptionDispatcher")?,
            ki_user_apc_dispatcher: find("KiUserApcDispatcher")?,
            ki_user_callback_dispatcher: find("KiUserCallbackDispatcher")?,
        },
        [find("__wine_syscall_dispatcher")?, find("__wine_unix_call_dispatcher")?, find("__wine_unixlib_handle")?],
    ))
}

/// Build the process: everything up to the first instruction of
/// `LdrInitializeThunk`. Returns the pc to start the main thread at; its
/// registers are already in its gex context (slot 0).
pub fn boot(p: &Arc<Process>, k: &mut Kernel, mem: &mut [u8], req: &BootRequest<'_>) -> Result<u64, String> {
    let abi = p.abi;
    let sysdir = if abi == Abi::P64 { "system32" } else { "syswow64" };
    let sys = format!("{}/drive_c/windows/{sysdir}", p.cfg.prefix);

    // Fixed pages: the wait words and the trampolines (docs/nt-layer.md §1.2).
    mem[WAIT_WORDS as usize..WAIT_WORDS_END as usize].fill(0);
    k.mem.insert(WAIT_WORDS, WAIT_WORDS_END, prot::READ | prot::WRITE, "[nt-data]");
    mem[TRAMPOLINES as usize..TRAMPOLINES_END as usize].copy_from_slice(&trampoline_page());
    k.mem.insert(TRAMPOLINES, TRAMPOLINES_END, prot::READ | prot::EXEC, "[nt-trampolines]");

    // KUSER_SHARED_DATA, carved out of the allocator's range.
    {
        let mut stl = p.st.lock().unwrap();
        stl.vm.reserve(USD, PAGE, page_prot::READONLY, true, Kind::Private, "[kuser_shared_data]");
        stl.vm.mirror(&mut k.mem, USD, USD + PAGE);
    }
    super::sysinfo::init_usd(mem, abi, &p.clocks);

    let mut g = G::new(mem, abi);

    // The staged layout, before anything is placed (`section::Layout`).
    let layout_path = format!("{}/{}", p.cfg.prefix, section::LAYOUT_FILE);
    if let Ok(text) = read_file(k, &layout_path) {
        let wine = read_file(k, &format!("{}/.wine-version", p.cfg.prefix)).unwrap_or_default();
        let layout = section::Layout::parse(&String::from_utf8_lossy(&text), &p.cfg.prefix, &String::from_utf8_lossy(&wine))?;
        let mut stl = p.st.lock().unwrap();
        stl.vm.reserved = layout.ranges_under(&sys);
        stl.layout = Some(Arc::new(layout));
    }

    // The program first, so an image that cannot move gets its base.
    let exe_bytes = read_file(k, req.exe_path)?;
    let exe = Arc::new(pe::parse(&exe_bytes).map_err(|e| format!("{}: {e}", req.exe_path))?);
    if exe.is_dll() {
        return Err(format!("{} is a DLL, not a program", req.exe_path));
    }
    let image_dos = dos_path(&p.cfg.prefix, req.exe_path);
    let exe_nt = to_w(&format!("\\??\\{image_dos}"));
    let (exe_base, _) = section::map_image(p, k, &mut g, &exe, 0, req.exe_path, &exe_nt)
        .map_err(st_err(&format!("mapping {}", req.exe_path)))?;

    // ntdll, and the three variables Wine's unix side fills
    // (`load_ntdll_functions`).
    let ntdll_path = format!("{sys}/ntdll.dll");
    let ntdll_bytes = read_file(k, &ntdll_path)?;
    let ntdll = Arc::new(pe::parse(&ntdll_bytes).map_err(|e| format!("{ntdll_path}: {e}"))?);
    let ntdll_nt = to_w(&format!("\\??\\C:\\windows\\{sysdir}\\ntdll.dll"));
    let (ntdll_base, _) =
        section::map_image(p, k, &mut g, &ntdll, 0, &ntdll_path, &ntdll_nt).map_err(st_err("mapping ntdll.dll"))?;
    let (entries, [p_sysdisp, p_unixdisp, p_unixhandle]) = ntdll_exports(&ntdll, ntdll_base)?;
    let w = |g: &mut G<'_>, a: u64, v: u64| g.wptr(a, v).map_err(st_err("writing ntdll's variables"));
    let (sysdisp, unixdisp) = match abi {
        Abi::P64 => (tramp::SYSCALL_DISPATCHER_64, tramp::UNIX_CALL_64),
        Abi::P32 => (tramp::SYSCALL_DISPATCHER_32, tramp::UNIX_CALL_32),
    };
    w(&mut g, p_sysdisp, sysdisp)?;
    w(&mut g, p_unixdisp, unixdisp)?;
    g.w64(p_unixhandle, super::unixcall::NTDLL_HANDLE).map_err(st_err("writing __wine_unixlib_handle"))?;
    *p.ntdll.lock().unwrap() = entries;

    // The API set schema: the file mapped read-only, `ApiSetMap` pointing at
    // its `.apiset` section's raw data (`load_apiset_dll`).
    let apiset_path = format!("{sys}/apisetschema.dll");
    let apiset_bytes = read_file(k, &apiset_path)?;
    let apiset = pe::parse(&apiset_bytes).map_err(|e| format!("{apiset_path}: {e}"))?;
    let raw = apiset
        .sections
        .iter()
        .find(|s| s.name == ".apiset")
        .map(|s| s.raw_offset as u64)
        .ok_or_else(|| format!("{apiset_path} has no .apiset section"))?;
    let apiset_base = {
        let size = (apiset_bytes.len() as u64 + PAGE - 1) & !(PAGE - 1);
        let mut stl = p.st.lock().unwrap();
        let b = stl.vm.find_free((size + GRANULE - 1) & !(GRANULE - 1), false, vm::ALLOC_HIGH).ok_or("no room for the API set schema")?;
        stl.vm.reserve(b, size, page_prot::READONLY, true, Kind::Mapped, "[apiset]");
        stl.vm.mirror(&mut k.mem, b, b + size);
        b
    };
    g.write(apiset_base, &apiset_bytes).map_err(st_err("writing the API set schema"))?;

    // The PEB (Wine's `init_peb`).
    let peb = alloc(p, k, 2 * PAGE, "[peb]").map_err(st_err("allocating the PEB"))?;
    p.peb.store(peb, Ordering::Release);
    let x = opt_extra(&exe);
    macro_rules! pf {
        ($f:ident) => {
            peb + off!(abi, peb::$f)
        };
    }
    let e = st_err("writing the PEB");
    g.wptr(pf!(IMAGEBASEADDRESS), exe_base).map_err(&e)?;
    g.wptr(pf!(APISETMAP), apiset_base + raw).map_err(&e)?;
    g.w32(pf!(OSMAJORVERSION), 10).map_err(&e)?;
    g.w32(pf!(OSMINORVERSION), 0).map_err(&e)?;
    g.w16(pf!(OSBUILDNUMBER), 19045).map_err(&e)?;
    g.w32(pf!(OSPLATFORMID), 2).map_err(&e)?; // VER_PLATFORM_WIN32_NT
    g.w32(pf!(IMAGESUBSYSTEM), exe.subsystem as u32).map_err(&e)?;
    g.w32(pf!(IMAGESUBSYSTEMMAJORVERSION), x.subsys_major as u32).map_err(&e)?;
    g.w32(pf!(IMAGESUBSYSTEMMINORVERSION), x.subsys_minor as u32).map_err(&e)?;
    g.w32(pf!(NUMBEROFPROCESSORS), super::sysinfo::PROCESSORS).map_err(&e)?;
    g.wptr(pf!(ACTIVEPROCESSAFFINITYMASK), (1u64 << super::sysinfo::PROCESSORS) - 1).map_err(&e)?;
    g.w32(pf!(SESSIONID), 1).map_err(&e)?;
    // load_global_options' defaults (no Session Manager key overrides them).
    g.w64(pf!(CRITICALSECTIONTIMEOUT), (-(30i64 * 24 * 60 * 60 * 10_000_000)) as u64).map_err(&e)?;
    g.wptr(pf!(HEAPSEGMENTRESERVE), 0x10_0000).map_err(&e)?;
    g.wptr(pf!(HEAPSEGMENTCOMMIT), 0x1_0000).map_err(&e)?;
    g.wptr(pf!(HEAPDECOMMITTOTALFREETHRESHOLD), 0x1_0000).map_err(&e)?;
    g.wptr(pf!(HEAPDECOMMITFREEBLOCKTHRESHOLD), 0x1000).map_err(&e)?;

    // The main thread: gex's slot 0, as for a Linux process.
    let t = super::thread::create_thread_state(p, k, &mut g, x.stack_reserve, Some((0, k.tid)))
        .map_err(st_err("creating the main thread"))?;
    p.add_thread(Arc::clone(&t));

    // Standard handles: the gex process's fds 0-2, without a console (a
    // console needs conhost; Wine's `get_initial_console` would mark ttys).
    let mut std_handles = [0u64; 3];
    for (fd, access) in [(0, 0x8010_0000u32), (1, 0x4010_0000), (2, 0x4010_0000)] {
        if let Some(f) = super::file::console_file(k, fd, access) {
            std_handles[fd as usize] = p.st.lock().unwrap().handles.insert(Obj::File(f), access) as u64;
        }
    }

    // RTL_USER_PROCESS_PARAMETERS (Wine's `build_initial_params`).
    let mut cmdline = quote_arg(&image_dos);
    for a in req.args {
        cmdline.push(' ');
        cmdline.push_str(&quote_arg(a));
    }
    let mut cwd = p.cfg.cwd.clone();
    if !cwd.ends_with('\\') {
        cwd.push('\\');
    }
    let mut env: Vec<u16> = Vec::new();
    for (k_, v) in &p.cfg.env {
        env.extend(to_w(&format!("{k_}={v}")));
        env.push(0);
    }
    env.push(0);
    let params_fixed = off!(abi, rtl_user_process_parameters::SIZEOF);
    let strings = [to_w(&image_dos), to_w(&cmdline), to_w(&image_dos)];
    let size = params_fixed
        + 260 * 2
        + strings.iter().map(|s| (s.len() as u64 + 1) * 2).sum::<u64>()
        + env.len() as u64 * 2;
    let params = alloc(p, k, size, "[process parameters]").map_err(st_err("allocating the process parameters"))?;
    macro_rules! rp {
        ($f:ident) => {
            params + off!(abi, rtl_user_process_parameters::$f)
        };
    }
    let e = st_err("writing the process parameters");
    g.w32(rp!(ALLOCATIONSIZE), size as u32).map_err(&e)?;
    g.w32(rp!(SIZE), size as u32).map_err(&e)?;
    g.w32(rp!(FLAGS), 1).map_err(&e)?; // PROCESS_PARAMS_FLAG_NORMALIZED
    g.w16(rp!(WSHOWWINDOW), 1).map_err(&e)?; // SW_SHOWNORMAL
    g.w32(rp!(PROCESSGROUPID), p.pid).map_err(&e)?;
    g.wptr(rp!(HSTDINPUT), std_handles[0]).map_err(&e)?;
    g.wptr(rp!(HSTDOUTPUT), std_handles[1]).map_err(&e)?;
    g.wptr(rp!(HSTDERROR), std_handles[2]).map_err(&e)?;
    let mut at = params + params_fixed;
    let cwd_w = to_w(&cwd);
    if cwd_w.len() >= 260 {
        return Err(format!("the current directory {cwd} is longer than MAX_PATH"));
    }
    g.write_wchars(at, &cwd_w).map_err(&e)?;
    g.w_ustr_hdr(rp!(CURRENTDIRECTORY_DOSPATH), cwd_w.len() as u16 * 2, 260 * 2, at).map_err(&e)?;
    at += 260 * 2;
    for (s, field) in strings.iter().zip([rp!(IMAGEPATHNAME), rp!(COMMANDLINE), rp!(WINDOWTITLE)]) {
        let mut z = s.clone();
        z.push(0);
        g.write_wchars(at, &z).map_err(&e)?;
        g.w_ustr_hdr(field, s.len() as u16 * 2, z.len() as u16 * 2, at).map_err(&e)?;
        at += z.len() as u64 * 2;
    }
    g.write_wchars(at, &env).map_err(&e)?;
    g.wptr(rp!(ENVIRONMENT), at).map_err(&e)?;
    g.wptr(rp!(ENVIRONMENTSIZE), env.len() as u64 * 2).map_err(&e)?;
    g.wptr(pf!(PROCESSPARAMETERS), params).map_err(&e)?;

    // The process object is waitable and never signalled while it runs.
    p.sync.lock().unwrap().states.insert(p.sync_id, super::sync::SState::Never);

    // Enter `LdrInitializeThunk` with a CONTEXT that starts
    // `RtlUserThreadStart(entry, peb)` (Wine's `init_syscall_frame`).
    let entry = exe_base + exe.entry_rva as u64;
    super::thread::init_start_frame(p, &mut g, &t, entry, peb).map_err(st_err("building the initial frame"))?;
    let pc = g.u64(t.ctx_addr + gex_abi::ctx::PC as u64).map_err(st_err("reading the start pc"))?;
    if std::env::var_os("GEX_NT_STATS").is_some() {
        crate::nt_log!("gex-nt: exe {} at {exe_base:#x}", req.exe_path);
    }
    if p.cfg.trace {
        crate::nt_log!(
            "[nt] boot: {} at {exe_base:#x}, ntdll at {ntdll_base:#x}, peb {peb:#x}, teb {:#x}, entering {pc:#x}",
            req.exe_path, t.teb
        );
    }
    Ok(pc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dos_paths() {
        assert_eq!(dos_path("/p", "/p/drive_c/windows/x.exe"), "C:\\windows\\x.exe");
        assert_eq!(dos_path("/p", "/p/drive_c"), "C:\\");
        assert_eq!(dos_path("/p", "/home/u/hello.exe"), "Z:\\home\\u\\hello.exe");
        assert_eq!(dos_path("/p", "/p/drive_cx/a"), "Z:\\p\\drive_cx\\a");
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_arg("abc"), "abc");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_arg("a b\\"), "\"a b\\\\\"");
    }

    #[test]
    fn trampolines_encode() {
        let t = trampoline_page();
        assert_eq!(&t[..3], &[0x4c, 0x8b, 0xd1]);
        assert_eq!(&t[8..11], &[0x0f, 0x05, 0xc3]);
    }
}
