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

/// The TEB exception frames (`Tib.ExceptionList`, at the TEB's start, each
/// frame's `Prev` its first word, -1 ending the list) a fault in 64-bit
/// Wine's native PE code leaves dead: that code's fault is a trap, which
/// ends every wasm frame it ran in, and the frames winecrt0's `__TRY`
/// pushed are on the stack below the guest's RSP, where the exception frame
/// is built next. Those below `rsp`: native code a guest call ran is above
/// the guest's stack, and translated code's own frames are not on the list.
/// The first live frame, for the list's head, when it is not `head` already.
pub fn first_live_teb_frame(head: u64, rsp: u64, read: impl Fn(u64) -> Option<u64>) -> Option<u64> {
    let mut frame = head;
    while frame != u64::MAX && frame != 0 && frame < rsp {
        frame = read(frame)?;
    }
    (frame != head).then_some(frame)
}

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
    /// WoW64's stops (wow64cpu built as native code, gexrt's `__gexw_run32`):
    /// where 32-bit code reaches 64-bit code, filler no code has. Its system
    /// call thunk (`BTCpuGetBopCode`) and its unix call one
    /// (`__wine_get_unix_opcode`), which share a dispatch slot: a call-out
    /// waiting for the first returns at either ([`returns_at`]), with the
    /// stack wherever the 32-bit code has it.
    pub const WOW64_STOP_SYSCALL: u64 = super::TRAMPOLINES_END - 0xc;
    pub const WOW64_STOP_UNIX: u64 = super::TRAMPOLINES_END - 0x9;
    /// The return address of a system call native 32-bit ntdll makes
    /// (gexrt's `__gexw_syscall`), where 32-bit state wow64 set (an APC, a
    /// context it continued) comes back to it.
    pub const WOW64_RETURN_MARK: u64 = super::TRAMPOLINES_END - 0x6;

    /// Whether a call-out waiting for `ret` returns when the guest reaches
    /// `pc`.
    pub const fn returns_at(ret: u64, pc: u64) -> bool {
        pc == ret || (ret == WOW64_STOP_SYSCALL && pc == WOW64_STOP_UNIX)
    }

    /// Whether a call-out waiting for `ret` takes it with any stack pointer
    /// (WoW64's stops: the 32-bit code reaches them from wherever it is,
    /// never by a return), and never reads the stack as unwound past it.
    pub const fn any_stack(ret: u64) -> bool {
        ret == WOW64_STOP_SYSCALL || ret == WOW64_RETURN_MARK
    }

    /// Whether `pc` is in the page's tail of filler from the return pad on
    /// (the pad, WoW64's stops, the return mark): addresses the guest
    /// reaches but no code has, which a translator must leave to the miss
    /// path that knows them, never decode.
    pub const fn filler(pc: u64) -> bool {
        pc >= NATIVE_RETURN_PAD && pc < super::TRAMPOLINES_END
    }

    /// What a guest call from native Wine must find left of its thread's
    /// stack: native Wine's own frames run on that stack (below the guest's
    /// stack pointer, as Windows' do), as wasm code whose stores no guard
    /// page stops, so the stack's end is checked before each such call
    /// instead, with room for the native frames until the next. Wine's own
    /// minimum (ntdll's `min_kernel_stack`, 8 KiB): a user-mode callback
    /// whose stack comes within it is refused by Wine first
    /// (`KeUserModeCallback`'s STATUS_STACK_OVERFLOW, which the program goes
    /// on from), so only a guest call Wine does not bound ends the process.
    pub const NATIVE_CALL_ROOM: u64 = 8 << 10;

    /// Whether a guest call from native Wine with the stack at `sp` finds
    /// its thread's stack spent: less than [`NATIVE_CALL_ROOM`] above the
    /// stack's reserved bottom (the TEB's `DeallocationStack`, the guard
    /// pages included). `teb` is the thread's TEB (the FS base for 32-bit
    /// code, the GS base for 64-bit), in the layout of `class32`; `read`
    /// reads a guest word of that width. A stack the TEB does not describe
    /// (no TEB yet, or `sp` outside it) is never spent.
    pub fn native_call_stack_spent(class32: bool, teb: u64, sp: u64, read: impl Fn(u64) -> Option<u64>) -> bool {
        if teb == 0 {
            return false;
        }
        // NT_TIB's StackBase, and DeallocationStack
        let (base, bottom) = if class32 { (teb + 4, teb + 0xe0c) } else { (teb + 8, teb + 0x1478) };
        let (Some(base), Some(bottom)) = (read(base), read(bottom)) else { return false };
        bottom != 0 && sp > bottom && sp <= base && sp - bottom < NATIVE_CALL_ROOM
    }

    /// Whether `pc` is one of WoW64's stops, which no code jumps to: reached
    /// with no call-out of the thread waiting there, it is a stray arrival.
    pub const fn wow64_stop(pc: u64) -> bool {
        pc == WOW64_STOP_SYSCALL || pc == WOW64_STOP_UNIX || pc == WOW64_RETURN_MARK
    }
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

/// The stubs of a native Wine DLL's PE skeleton (winebuild's
/// `output_wasm_skeleton`), where gex binds the DLL's native exports.
/// 32-bit Wine's are `ud2; ud2`. 64-bit Wine's are 16 bytes a hooking
/// library can patch: `jmp [rip+0]`, the address after it (the stub's
/// alias, [`skel::ALIAS`] bytes on) and the alias's `int3; int3`. A hook's
/// trampoline copies the whole `jmp`, so the original it calls lands on the
/// alias, which gex binds to the same native export once the stub is
/// rewritten.
pub mod skel {
    /// The alias's offset from its stub.
    pub const ALIAS: u64 = 14;

    /// Memory a skeleton is read from: a slice addressed from 0 (a native
    /// host's memory, a test's), or the guest's [`crate::GuestMem`].
    pub trait Bytes {
        fn bytes_at(&self, a: u64, n: usize) -> Option<&[u8]>;
    }

    impl Bytes for [u8] {
        fn bytes_at(&self, a: u64, n: usize) -> Option<&[u8]> {
            let a = usize::try_from(a).ok()?;
            self.get(a..a.checked_add(n)?)
        }
    }

    impl Bytes for crate::GuestMem<'_> {
        fn bytes_at(&self, a: u64, n: usize) -> Option<&[u8]> {
            self.read(a, n).ok()
        }
    }

    fn at<const N: usize>(m: &(impl Bytes + ?Sized), a: u64) -> Option<[u8; N]> {
        m.bytes_at(a, N)?.try_into().ok()
    }

    /// Whether the bytes at `a` are still an export's stub.
    pub fn stub(m: &(impl Bytes + ?Sized), a: u64) -> bool {
        match at::<4>(m, a) {
            Some([0x0f, 0x0b, 0x0f, 0x0b]) => true,
            Some([0xff, 0x25, 0, 0]) => at::<2>(m, a + 4) == Some([0, 0]) && alias_target(m, a) == Some(a + ALIAS),
            _ => false,
        }
    }

    /// The address the 64-bit stub at `a` jumps to (its alias, unless the
    /// operand was rewritten).
    pub fn alias_target(m: &(impl Bytes + ?Sized), a: u64) -> Option<u64> {
        at::<8>(m, a + 6).map(u64::from_le_bytes)
    }

    /// Whether `a` is a 64-bit stub's alias: its `int3; int3`, [`ALIAS`]
    /// bytes into a 16-byte stub. Not by the stub's operand: a 14-byte hook
    /// (`jmp [rip+0]` and the detour's address) rewrote it, and the hook's
    /// trampoline, which copied the original 14 bytes, still lands here.
    pub fn alias(m: &(impl Bytes + ?Sized), a: u64) -> bool {
        a % 16 == ALIAS && at::<2>(m, a) == Some([0xcc, 0xcc])
    }

    /// Whether the bytes at `a` are still the skeleton's: a stub or an alias.
    pub fn intact(m: &(impl Bytes + ?Sized), a: u64) -> bool {
        stub(m, a) || alias(m, a)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn stub64(m: &mut [u8], a: u64) {
            let i = a as usize;
            m[i..i + 6].copy_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
            m[i + 6..i + 14].copy_from_slice(&(a + ALIAS).to_le_bytes());
            m[i + 14..i + 16].copy_from_slice(&[0xcc, 0xcc]);
        }

        #[test]
        fn stubs_and_aliases() {
            let mut m = vec![0u8; 64];
            m[0..4].copy_from_slice(&[0x0f, 0x0b, 0x0f, 0x0b]);
            stub64(&mut m, 16);
            stub64(&mut m, 32);
            assert!(stub(&m[..], 0) && intact(&m[..], 0));
            assert!(stub(&m[..], 16) && !alias(&m[..], 16));
            assert!(alias(&m[..], 30) && !stub(&m[..], 30) && intact(&m[..], 30));
            assert_eq!(alias_target(&m[..], 32), Some(46));
            // a 5-byte hook at 16: the stub is gone, its operand and alias stay
            m[16..21].copy_from_slice(&[0xe9, 1, 2, 3, 4]);
            assert!(!intact(&m[..], 16) && alias(&m[..], 30) && alias_target(&m[..], 16) == Some(30));
            // a 14-byte one at 32 over the operand: the stub is gone, its alias stays
            m[32..38].copy_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
            m[38..46].copy_from_slice(&0x1234u64.to_le_bytes());
            assert!(!stub(&m[..], 32) && !intact(&m[..], 32) && alias(&m[..], 46) && intact(&m[..], 46));
            assert!(!intact(&m[..], 60) && !intact(&m[..], 1000));
        }
    }
}
