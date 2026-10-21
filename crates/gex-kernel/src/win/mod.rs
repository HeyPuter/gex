//! What a Windows program's process has besides Wine itself: the page of
//! trampolines gex writes for Wine's dispatchers, the gex-private system
//! call numbers they issue, and ntdll's unixlib handle.

/// The page of real code gex writes for Wine's dispatchers.
pub const TRAMPOLINES: u64 = 0x003f_0000;
pub const TRAMPOLINES_END: u64 = 0x0040_0000;

/// ntdll's own unixlib handle (`__wine_unixlib_handle`).
pub const NTDLL_HANDLE: u64 = 1;

/// gex-private system call numbers, issued only by gex's own trampolines.
pub mod thunk;

pub mod nr {
    /// `__wine_unix_call_dispatcher(handle, code, args)`.
    pub const UNIX_CALL: u32 = 0x7f00_0001;
    /// The x86-64 `__wine_syscall_dispatcher` was called, which Wine's
    /// x86-64 stubs never do while `USD.SystemCall` is 0.
    pub const BAD_DISPATCH: u32 = 0x7f00_0002;
    /// From native Wine: guest code entered native code from above the
    /// innermost native entry's stack, i.e. it left those native frames
    /// earlier (a catch block continuing in an outer frame) and has been
    /// running nested inside them since. The host abandons the innermost
    /// call-out (its native caller then drops its frames), and the guest
    /// re-enters from the context's PC, one level flatter.
    pub const NATIVE_LEFT: u32 = 0x7f00_0003;
}

/// Read a whole file by guest path.
pub fn read_guest_file(ns: &crate::vfs::Namespace, path: &str) -> Result<Vec<u8>, crate::Errno> {
    let h = ns.open(path, crate::fs::oflags::O_RDONLY, 0)?;
    let size = h.metadata()?.size as usize;
    let mut v = vec![0u8; size];
    let mut done = 0;
    while done < size {
        let n = h.read_at(done as u64, &mut v[done..])? as usize;
        if n == 0 {
            break;
        }
        done += n;
    }
    v.truncate(done);
    Ok(v)
}

/// The trampolines at [`TRAMPOLINES`].
pub mod tramp {
    use super::TRAMPOLINES;
    /// `__wine_unix_call_dispatcher` for x86-64:
    /// `mov r10, rcx; mov eax, UNIX_CALL; syscall; ret`.
    pub const UNIX_CALL_64: u64 = TRAMPOLINES;
    /// `__wine_syscall_dispatcher` for x86-64. Wine's x86-64 stubs never
    /// reach it while `USD.SystemCall` is 0; if anything does, it reports
    /// itself: `mov eax, BAD_DISPATCH; syscall; ud2`.
    pub const SYSCALL_DISPATCHER_64: u64 = TRAMPOLINES + 0x20;
    /// `__wine_syscall_dispatcher` for i386: the stub
    /// did `mov eax, id; call __wine_syscall`, which jumps here, so the
    /// arguments are above two return addresses: `int 0x80; ret`.
    pub const SYSCALL_DISPATCHER_32: u64 = TRAMPOLINES + 0x40;
    /// `__wine_unix_call_dispatcher` for i386, a stdcall of (handle: u64,
    /// code, args). The inner `call` gives it the syscall stubs' stack shape
    /// (two return addresses above the arguments):
    /// `mov eax, UNIX_CALL; call 1f; ret 16; 1: int 0x80; ret`.
    pub const UNIX_CALL_32: u64 = TRAMPOLINES + 0x60;
    /// `jmp eax`: where native Wine (gexrt) sends the guest to call a
    /// function again from the guest's side (an entry re-made after the
    /// native frames it was nested in were dropped, [`nr::NATIVE_LEFT`]),
    /// the function's address in EAX. Not the function's own address as the
    /// PC: a body that leaves for its own address has served nothing, and
    /// tier-1 would take its slot over.
    pub const JMP_EAX_32: u64 = TRAMPOLINES + 0x80;
    /// Native Wine's virtual calls on guest C++ objects (msvcrt's
    /// CALL_VTBL_FUNC, gexrt `__gexw_vtbl_target`), as i386 Wine's
    /// `vtbl_wrapper_N`: the caller pushed `this` first; it goes to ECX and
    /// the call to the vtable's entry `n`: `pop eax; pop ecx; push eax;
    /// mov eax, [ecx]; jmp [eax + 4n]`. 64 of them, 16 bytes apart.
    pub const VTBL_WRAPPERS_32: u64 = TRAMPOLINES + 0x100;
    pub const VTBL_WRAPPERS: u64 = 64;
    /// The return address of a guest call from native Wine (gexrt's
    /// `GEXW_RETURN_PAD`): the page's last word,
    /// filler no code has. A call-out waiting for it is a native caller's,
    /// whose frames lie just above the callee's stack: the stack pointer
    /// rising past its arguments means the guest left them (a catch block
    /// continuing in an outer frame).
    pub const NATIVE_RETURN_PAD: u64 = super::TRAMPOLINES_END - 0xf;
}

/// Where the trampoline page's code continues after its system call
/// instructions (and the unix-call dispatcher's inner call): where a host
/// that leaves the guest's code for a system call (the browser's) resumes
/// it. The unit has no entries there; an ejected web build gives them
/// region entries.
pub fn trampoline_resumes(class32: bool) -> Vec<u64> {
    if class32 {
        vec![tramp::SYSCALL_DISPATCHER_32 + 2, tramp::UNIX_CALL_32 + 10, tramp::UNIX_CALL_32 + 15]
    } else {
        vec![tramp::UNIX_CALL_64 + 10]
    }
}

pub fn trampoline_page() -> Vec<u8> {
    let mut page = vec![0xccu8; (TRAMPOLINES_END - TRAMPOLINES) as usize];
    let put = |page: &mut Vec<u8>, at: u64, code: &[u8]| {
        let o = (at - TRAMPOLINES) as usize;
        page[o..o + code.len()].copy_from_slice(code);
    };
    let n = nr::UNIX_CALL.to_le_bytes();
    put(&mut page, tramp::UNIX_CALL_64, &[0x4c, 0x8b, 0xd1, 0xb8, n[0], n[1], n[2], n[3], 0x0f, 0x05, 0xc3]);
    let b = nr::BAD_DISPATCH.to_le_bytes();
    put(&mut page, tramp::SYSCALL_DISPATCHER_64, &[0xb8, b[0], b[1], b[2], b[3], 0x0f, 0x05, 0x0f, 0x0b]);
    put(&mut page, tramp::SYSCALL_DISPATCHER_32, &[0xcd, 0x80, 0xc3]);
    put(
        &mut page,
        tramp::UNIX_CALL_32,
        &[0xb8, n[0], n[1], n[2], n[3], 0xe8, 0x03, 0x00, 0x00, 0x00, 0xc2, 0x10, 0x00, 0xcd, 0x80, 0xc3],
    );
    put(&mut page, tramp::JMP_EAX_32, &[0xff, 0xe0]);
    for n in 0..tramp::VTBL_WRAPPERS {
        let d = (4 * n as u32).to_le_bytes();
        put(&mut page, tramp::VTBL_WRAPPERS_32 + 16 * n, &[0x58, 0x59, 0x50, 0x8b, 0x01, 0xff, 0xa0, d[0], d[1], d[2], d[3]]);
    }
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
