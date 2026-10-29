//! The functions translated code imports (`gex.*`, `gex_abi::imports`). Plain static functions: their function-table
//! indices are the same on every Worker, so the JS library passes `wasmTable.get(index)` straight to each module,
//! and a call from translated code to one is wasm to wasm.

use gex_abi::ctx;
use std::sync::atomic::{AtomicU64, Ordering};

/// Back edges between safepoint calls.
pub const SAFEPOINT_INTERVAL: u32 = 1024;

/// A host function the guest calls (`gex_embed_thunk`): `(ctx, user)`, reading its arguments from the context and
/// writing its results back.
pub type HostFn = extern "C" fn(ctx: u32, user: u32);

/// Host functions by id, `fn | user << 32`.
static HOSTFNS: [AtomicU64; crate::layout::THUNKS as usize] = [const { AtomicU64::new(0) }; crate::layout::THUNKS as usize];

pub fn set_host_fn(id: u32, f: HostFn, user: u32) {
    HOSTFNS[id as usize].store(f as *const () as usize as u64 | (user as u64) << 32, Ordering::Release);
}

pub extern "C" fn syscall(c: i32, _pc: i64) {
    crate::sys::syscall(c as u32);
}

pub extern "C" fn dispatch_miss(_c: i32, pc: i64) -> i32 {
    crate::jit::miss(pc as u64)
}

pub extern "C" fn illegal(c: i32, pc: i64, raw: i32) {
    crate::host::fatal(&format!(
        "illegal instruction {raw:#010x} at {:#x} (sp {:#x}, x30 {:#x})",
        pc,
        crate::cpu::x(c as u32, 31),
        crate::cpu::x(c as u32, 30)
    ));
}

pub extern "C" fn breakpoint(c: i32, pc: i64, imm: i32) {
    match imm {
        gex_abi::imports::BRK_TRACE | gex_abi::imports::BRK_TRAP_PC => {
            crate::host::log(&format!("gex-embed: at {pc:#x} x0 {:#x} sp {:#x}\n", crate::cpu::x(c as u32, 0), crate::cpu::x(c as u32, 31)));
        }
        _ => crate::host::fatal(&format!("brk #{imm} at {pc:#x}")),
    }
}

pub extern "C" fn sysreg_read(_c: i32, _pc: i64, reg: i32) -> i64 {
    match reg {
        // CNTVCT_EL0, at the frequency the lifter gives CNTFRQ: nanoseconds.
        -3 => crate::host::now_ns() as i64,
        // MIDR_EL1: a Cortex-A72, as the other hosts claim.
        -7 => 0x410f_d083,
        _ => 0,
    }
}

pub extern "C" fn sysreg_write(c: i32, pc: i64, reg: i32, val: i64) {
    if reg == gex_xlate::intrinsic::REPLACE_SYSREG {
        let e = HOSTFNS.get(val as usize).map_or(0, |h| h.load(Ordering::Acquire));
        if e == 0 {
            crate::host::fatal(&format!("host function #{val} at {pc:#x} is not bound"));
        }
        // SAFETY: stored from a `HostFn` by `set_host_fn`.
        let f: HostFn = unsafe { core::mem::transmute::<usize, HostFn>(e as u32 as usize) };
        f(c as u32, (e >> 32) as u32);
    }
}

pub extern "C" fn cache_op(_c: i32, _pc: i64, _op: i32, _addr: i64) {}

pub extern "C" fn safepoint(c: i32, _pc: i64) {
    crate::cpu::set_u32(c as u32, ctx::SAFEPOINT_COUNTER, SAFEPOINT_INTERVAL);
}

pub extern "C" fn fma(_c: i32, a: i64, b: i64, cc: i64, double: i32) -> i64 {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        if double != 0 {
            crate::host::fma(f64::from_bits(a as u64), f64::from_bits(b as u64), f64::from_bits(cc as u64)).to_bits() as i64
        } else {
            crate::host::fmaf(f32::from_bits(a as u32), f32::from_bits(b as u32), f32::from_bits(cc as u32)).to_bits() as i64
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (a, b, cc, double);
        unimplemented!()
    }
}

pub extern "C" fn x87(_c: i32, _op: i32, _a: i64, _ae: i32, _b: i64, _be: i32, _cw: i32) -> i64 {
    crate::host::fatal("an x87 operation in an AArch64 guest")
}

/// The table indices of the imports, in the descriptor's order (`js/gex_embed.js`'s `IMPORTS`).
pub fn table() -> [u32; 10] {
    [
        syscall as *const () as usize as u32,
        dispatch_miss as *const () as usize as u32,
        illegal as *const () as usize as u32,
        breakpoint as *const () as usize as u32,
        sysreg_read as *const () as usize as u32,
        sysreg_write as *const () as usize as u32,
        cache_op as *const () as usize as u32,
        safepoint as *const () as usize as u32,
        fma as *const () as usize as u32,
        x87 as *const () as usize as u32,
    ]
}
