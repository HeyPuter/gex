//! Guest system calls, passed to the host's libc. (The start of it: what a test guest needs.)

use crate::cpu;
use gex_abi::ctx;

const ENOSYS: i64 = 38;

pub fn syscall(c: u32) {
    let nr = cpu::get_u64(c, ctx::SYSCALL_NR);
    let a = |i: usize| cpu::get_u64(c, ctx::SYSCALL_ARGS[i]);
    let r: i64 = match nr {
        // write
        64 => host_write(a(0) as i32, a(1), a(2)),
        // exit, exit_group
        93 | 94 => exit(a(0) as i32),
        // getpid, gettid
        172 | 178 => (unsafe { crate::host::gettid() }) as i64,
        // clock_gettime
        113 => clock_gettime(a(0) as i32, a(1)),
        _ => {
            crate::host::log(&format!("gex-embed: syscall {nr} not implemented\n"));
            -ENOSYS
        }
    };
    cpu::set_u64(c, ctx::SYSCALL_RET, r as u64);
}

#[cfg(target_arch = "wasm32")]
fn host_write(fd: i32, p: u64, n: u64) -> i64 {
    // SAFETY: the guest's buffer, in the shared memory.
    let r = unsafe { crate::host::write(fd, p as usize as *const u8, n as usize) };
    if r < 0 { -5 } else { r as i64 }
}

#[cfg(target_arch = "wasm32")]
fn exit(code: i32) -> i64 {
    unsafe { crate::host::exit(code) }
}

#[cfg(target_arch = "wasm32")]
fn clock_gettime(clk: i32, ts: u64) -> i64 {
    let mut t = crate::host::Timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a local, then the guest's 16-byte `timespec`.
    unsafe {
        if crate::host::clock_gettime(clk, &mut t) != 0 {
            return -22;
        }
        let g = ts as usize as *mut i64;
        g.write_unaligned(t.tv_sec);
        g.add(1).write_unaligned(t.tv_nsec as i64);
    }
    0
}

#[cfg(not(target_arch = "wasm32"))]
fn host_write(_: i32, _: u64, _: u64) -> i64 {
    -ENOSYS
}
#[cfg(not(target_arch = "wasm32"))]
fn exit(code: i32) -> i64 {
    std::process::exit(code)
}
#[cfg(not(target_arch = "wasm32"))]
fn clock_gettime(_: i32, _: u64) -> i64 {
    -ENOSYS
}
