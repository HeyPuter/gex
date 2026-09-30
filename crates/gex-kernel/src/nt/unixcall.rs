//! `__wine_unix_call` and unixlibs (docs/nt-layer.md §6).
//!
//! ntdll's exported `__wine_unix_call_dispatcher` points at a trampoline that
//! issues the gex-private syscall [`super::gex_nr::UNIX_CALL`] with
//! `(handle, code, args)`. A handle is `(dll id << 16) | 1`; id 0 is ntdll
//! itself (its `__wine_unixlib_handle`), and the others are handed out by
//! `NtQueryVirtualMemory(MemoryWineLoadUnixLib)` to DLLs gex has a Unix side
//! for. Nothing has one yet (the per-DLL redirects are later work), so every
//! other DLL gets `STATUS_DLL_NOT_FOUND`: those listed in [`DEGRADES`] quietly,
//! because Wine's PE side handles that answer, the rest loudly.

use super::{st, Abi, Call, Status, R};
use std::collections::HashMap;
use std::sync::Mutex;

/// ntdll's own unixlib handle.
pub const NTDLL_HANDLE: u64 = 1;

/// DLLs whose PE side copes with having no Unix side (the census's list for
/// Noita: networking and crypto report "nothing available").
pub const DEGRADES: &[&str] = &[
    "ws2_32.dll",
    "secur32.dll",
    "crypt32.dll",
    "bcrypt.dll",
    "dnsapi.dll",
    "kerberos.dll",
    "msv1_0.dll",
    "netapi32.dll",
];

#[derive(Default, Debug)]
pub struct Registry {
    /// Handle → DLL name, for the DLLs given a handle.
    pub libs: Mutex<HashMap<u64, String>>,
}

/// ntdll's unix functions (`enum ntdll_unix_funcs`, dlls/ntdll/unixlib.h).
mod ntdll_fn {
    pub const LOAD_SO_DLL: u32 = 0;
    pub const UNWIND_BUILTIN_DLL: u32 = 1;
    pub const WINE_DBG_WRITE: u32 = 2;
    pub const WINE_SERVER_CALL: u32 = 3;
    pub const WINE_SERVER_FD_TO_HANDLE: u32 = 4;
    pub const WINE_SERVER_HANDLE_TO_FD: u32 = 5;
    pub const WINE_SPAWNVP: u32 = 6;
    pub const SYSTEM_TIME_PRECISE: u32 = 7;
    pub const NAMES: [&str; 8] = [
        "load_so_dll",
        "unwind_builtin_dll",
        "wine_dbg_write",
        "wine_server_call",
        "wine_server_fd_to_handle",
        "wine_server_handle_to_fd",
        "wine_spawnvp",
        "system_time_precise",
    ];
}

/// `NtQueryVirtualMemory(MemoryWineLoadUnixLib[ByName])` (classes 1000 and
/// 1002).
pub fn load_unix_lib(c: &mut Call<'_>, class: u32, addr: u64, buf: u64, len: u64) -> R {
    if class != 1000 {
        let name = c.g.ustr(addr)?.map(|n| super::wstr(&n)).unwrap_or_default();
        return Err(c.unimplemented(&format!("MemoryWineLoadUnixLibByName ({name})")));
    }
    let ps = 8; // unixlib_handle_t is a UINT64 in both ABIs
    if len != ps {
        return Err(Status(st::INFO_LENGTH_MISMATCH));
    }
    let _ = buf;
    let name = {
        let stl = c.p.st.lock().unwrap();
        stl.images.get(&addr).map(|v| v.path.rsplit('/').next().unwrap_or(&v.path).to_ascii_lowercase())
    };
    let Some(name) = name else { return Err(Status(st::INVALID_HANDLE)) };
    if DEGRADES.contains(&name.as_str()) {
        return Err(Status(st::DLL_NOT_FOUND));
    }
    Err(c.unimplemented(&format!("the Unix side of {name} (MemoryWineLoadUnixLib)")))
}

/// The gex-private `UNIX_CALL` syscall: `(handle, code, args)`.
pub fn dispatch(c: &mut Call<'_>) -> R {
    // i386: the handle is a 64-bit stdcall argument, two stack slots.
    let (handle, code, args) = match c.p.abi {
        Abi::P64 => (c.a(0), c.a32(1), c.a(2)),
        Abi::P32 => (c.a(0) | c.a(1) << 32, c.a32(2), c.a(3)),
    };
    if handle == NTDLL_HANDLE {
        return ntdll(c, code, args);
    }
    let lib = c.p.unix.libs.lock().unwrap().get(&handle).cloned();
    match lib {
        Some(name) => Err(c.unimplemented(&format!("{name} unix call {code}"))),
        None => Err(c.unimplemented(&format!("unix call {code} on unknown handle {handle:#x}"))),
    }
}

fn ntdll(c: &mut Call<'_>, code: u32, args: u64) -> R {
    use ntdll_fn::*;
    let p64 = c.p.abi == Abi::P64;
    match code {
        // No `.so` builtins exist: the PE loader falls back to its own
        // search, and a DLL that only exists as a `.so` is missing.
        LOAD_SO_DLL => Err(Status(st::DLL_NOT_FOUND)),
        // No DWARF-only builtin code exists; every unwind goes through
        // `.pdata`.
        UNWIND_BUILTIN_DLL => Err(Status(st::UNSUCCESSFUL)),
        // `{ const char *str; unsigned int len; }`.
        WINE_DBG_WRITE => {
            let s = c.g.ptr(args)?;
            let n = c.g.u32(args + if p64 { 8 } else { 4 })? as usize;
            let bytes = c.g.bytes(s, n)?;
            use std::io::Write;
            let _ = std::io::stderr().write_all(&bytes);
            Ok(n as u32)
        }
        // `LONGLONG *`: the precise system time.
        SYSTEM_TIME_PRECISE => {
            let t = c.p.clocks.filetime();
            c.g.w64(args, t)?;
            Ok(st::SUCCESS)
        }
        WINE_SERVER_CALL | WINE_SERVER_FD_TO_HANDLE | WINE_SERVER_HANDLE_TO_FD | WINE_SPAWNVP => {
            Err(c.unimplemented(&format!("ntdll unix call {} (there is no wineserver)", NAMES[code as usize])))
        }
        _ => Err(c.unimplemented(&format!("ntdll unix call {code}"))),
    }
}
