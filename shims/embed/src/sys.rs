//! Guest system calls. What is about memory, threads, signals and the process is here; everything else (files,
//! time, futexes, ids) is the host's libc, through `c/gex_embed_sys.c`, which knows emscripten's structs and errno.

use crate::cpu;
use gex_abi::ctx;

const ENOSYS: i64 = 38;
const EINVAL: i64 = 22;
const ENOMEM: i64 = 12;

/// The guest's uid and gid: an app's.
pub const UID: u64 = 10000;

extern "C" {
    fn gex_embed_sys(nr: i64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64;
}

/// Whether to say each call (`gex_embed_set_trace`).
pub static TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn syscall(c: u32) {
    let nr = cpu::get_u64(c, ctx::SYSCALL_NR) as i64;
    let a: [u64; 6] = core::array::from_fn(|i| cpu::get_u64(c, ctx::SYSCALL_ARGS[i]));
    let r: i64 = match nr {
        56 => openat(&a),
        57 => {
            crate::overlay::closed(a[0] as i32);
            host(nr, &a)
        }
        78 => readlinkat(&a),
        93 | 94 => exit(nr, a[0] as i32),
        96 => crate::thread::current().tid as i64,
        129..=131 => kill(nr, &a),
        132 => sigaltstack(a[1]),
        134 => {
            // rt_sigaction: recorded nowhere; gex delivers no signals hosted.
            zero(a[2], 32);
            0
        }
        135 => {
            zero(a[2], 8);
            0
        }
        167 => prctl(&a),
        172 => crate::thread::PID as i64,
        178 => crate::thread::current().tid as i64,
        214 => -ENOMEM,
        215 => crate::vm::munmap(a[0], a[1]),
        216 => -ENOMEM,
        220 | 435 => {
            crate::host::log("gex-embed: clone is not supported yet\n");
            -ENOSYS
        }
        222 => crate::vm::mmap(a[0], a[1], a[2] as u32, a[3], a[4] as i32, a[5]),
        226 => crate::vm::mprotect(a[0], a[1], a[2] as u32),
        233 => crate::vm::madvise(a[0], a[1], a[2]),
        _ => host(nr, &a),
    };
    if TRACE.load(std::sync::atomic::Ordering::Relaxed) {
        crate::host::log(&format!("gex-embed: [{}] syscall {nr}({:#x}, {:#x}, {:#x}, {:#x}) = {r}\n", crate::thread::current().tid, a[0], a[1], a[2], a[3]));
    }
    cpu::set_u64(c, ctx::SYSCALL_RET, r as u64);
}

fn host(nr: i64, a: &[u64; 6]) -> i64 {
    // SAFETY: the C side's, with the guest's arguments.
    let r = unsafe { gex_embed_sys(nr, a[0], a[1], a[2], a[3], a[4], a[5]) };
    if r == -ENOSYS {
        crate::host::log(&format!("gex-embed: syscall {nr} not implemented\n"));
    }
    r
}

/// A NUL-terminated string in guest memory.
fn cstr(p: u64) -> String {
    if p == 0 {
        return String::new();
    }
    // SAFETY: a string the guest passed.
    unsafe { std::ffi::CStr::from_ptr(p as usize as *const core::ffi::c_char) }.to_string_lossy().into_owned()
}

fn zero(p: u64, n: usize) {
    if p != 0 {
        // SAFETY: the guest's out-parameter.
        unsafe { (p as usize as *mut u8).write_bytes(0, n) };
    }
}

fn openat(a: &[u64; 6]) -> i64 {
    let r = host(56, a);
    if r >= 0 {
        crate::overlay::opened(r as i32, cstr(a[1]));
    }
    r
}

fn readlinkat(a: &[u64; 6]) -> i64 {
    if cstr(a[1]) == "/proc/self/exe" {
        let exe = crate::elf::EXE.get().map_or("/system/bin/app_process64", |s| s.as_str());
        let n = exe.len().min(a[3] as usize);
        // SAFETY: the guest's buffer of `a[3]` bytes.
        unsafe { core::ptr::copy_nonoverlapping(exe.as_ptr(), a[2] as usize as *mut u8, n) };
        return n as i64;
    }
    host(78, a)
}

fn exit(nr: i64, code: i32) -> i64 {
    if nr == 93 {
        crate::host::fatal(&format!("a guest thread exited ({code}), and threads aren't supported yet"));
    }
    crate::host::log(&format!("gex-embed: the guest exited ({code})\n"));
    #[cfg(target_arch = "wasm32")]
    unsafe {
        crate::host::exit(code)
    }
    #[cfg(not(target_arch = "wasm32"))]
    std::process::exit(code)
}

fn kill(nr: i64, a: &[u64; 6]) -> i64 {
    let sig = if nr == 131 { a[2] } else { a[1] };
    if sig == 0 {
        return 0;
    }
    let t = crate::thread::current();
    crate::host::fatal(&format!(
        "the guest raised signal {sig} (pc {:#x}, x30 {:#x})",
        cpu::get_u64(t.ctx, ctx::PC),
        cpu::x(t.ctx, 30)
    ));
}

fn sigaltstack(old: u64) -> i64 {
    if old != 0 {
        zero(old, 24);
        // SS_DISABLE
        // SAFETY: the guest's stack_t.
        unsafe { ((old + 8) as usize as *mut u32).write_unaligned(2) };
    }
    0
}

fn prctl(a: &[u64; 6]) -> i64 {
    match a[0] {
        // PR_SET_NAME, PR_SET_DUMPABLE, PR_SET_VMA
        15 | 4 | 0x5356_4d41 => 0,
        // PR_GET_NAME
        16 => {
            let name = b"guest\0";
            // SAFETY: the guest's 16-byte buffer.
            unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), a[1] as usize as *mut u8, name.len()) };
            0
        }
        // PR_SET/GET_TAGGED_ADDR_CTRL and the rest: no. (A tagged pointer would land on gex's bounds guard.)
        _ => -EINVAL,
    }
}
