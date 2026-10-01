//! Diagnostics for comparing two runs syscall by syscall (the function tier's
//! lockstep check, `crates/gex-cgen/tools/lockstep`). Both are off unless
//! their variable is set, and change nothing then.
//!
//! * `GEX_SYSCALL_LOG=<path>`: every syscall appends an entry line with the
//!   thread, pc, number, arguments and the architectural registers, and a
//!   result line with the result register when the kernel returns.
//! * `GEX_FAKE_CLOCK=1`: the sources of run-to-run difference a guest sees
//!   are answered deterministically: `clock_gettime`, `gettimeofday`, `time`,
//!   `getrandom`, and `CNTVCT_EL0`/`rdtsc`. (`AT_RANDOM` is fixed already.)

use gex_abi::ctx;
use gex_kernel::GuestMem;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

static LOG: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

fn log() -> Option<&'static Mutex<std::fs::File>> {
    LOG.get_or_init(|| {
        let path = std::env::var_os("GEX_SYSCALL_LOG")?;
        match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => Some(Mutex::new(f)),
            Err(e) => panic!("GEX_SYSCALL_LOG: cannot open {path:?}: {e}"),
        }
    })
    .as_ref()
}

pub fn fake_clock() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("GEX_FAKE_CLOCK").is_some())
}

fn rd(gm: &GuestMem<'_>, a: u64) -> u64 {
    gm.read_u64(a).unwrap_or(0xdead_dead_dead_dead)
}

/// The entry line: `E tid pc nr a0..a5 | x0..x30 sp nzcv pa vhash`.
pub fn log_entry(gm: &GuestMem<'_>, ctx_addr: u64, tid: i32, pc: u64) {
    let Some(f) = log() else { return };
    let mut s = format!("E {tid} {pc:x} {:x}", rd(gm, ctx_addr + ctx::SYSCALL_NR as u64));
    for a in ctx::SYSCALL_ARGS {
        s += &format!(" {:x}", rd(gm, ctx_addr + a as u64));
    }
    s += " |";
    for n in 0..31u8 {
        s += &format!(" {:x}", rd(gm, ctx_addr + ctx::x(n) as u64));
    }
    s += &format!(" sp={:x}", rd(gm, ctx_addr + ctx::SP as u64));
    let flags = rd(gm, ctx_addr + ctx::NZCV as u64);
    // AArch64 has four flag bytes; x86-64 six (and DF) in the doubleword.
    let flags = if gex_abi::X86 { flags & 0x00ff_ffff_ffff_ffff } else { flags & 0xffff_ffff };
    s += &format!(" f={flags:x}");
    // The vector file as one FNV-1a hash: 512 bytes per line is too much.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    if let Ok(v) = gm.read(ctx_addr + ctx::V as u64, 32 * 16) {
        for b in v {
            h = (h ^ *b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
    s += &format!(" v={h:x}\n");
    let _ = f.lock().unwrap_or_else(|e| e.into_inner()).write_all(s.as_bytes());
}

/// The result line: `R tid result`.
pub fn log_result(gm: &GuestMem<'_>, ctx_addr: u64, tid: i32) {
    let Some(f) = log() else { return };
    let s = format!("R {tid} {:x}\n", rd(gm, ctx_addr + ctx::SYSCALL_RET as u64));
    let _ = f.lock().unwrap_or_else(|e| e.into_inner()).write_all(s.as_bytes());
}

/// The fake clock, in nanoseconds: starts at a fixed date and advances a
/// microsecond per reading, so time still moves forward.
fn tick() -> u64 {
    static T: AtomicU64 = AtomicU64::new(1_700_000_000_000_000_000);
    T.fetch_add(1_000, Ordering::Relaxed)
}

/// `CNTVCT_EL0` / `rdtsc` under the fake clock.
pub fn counter() -> Option<i64> {
    fake_clock().then(|| tick() as i64)
}

/// Answer a nondeterministic syscall here, when the fake clock is on.
/// Returns true when it did (the result register is written).
pub fn fake_syscall(gm: &mut GuestMem<'_>, ctx_addr: u64) -> bool {
    if !fake_clock() {
        return false;
    }
    let nr = rd(gm, ctx_addr + ctx::SYSCALL_NR as u64);
    let arg = |gm: &GuestMem<'_>, i: usize| rd(gm, ctx_addr + ctx::SYSCALL_ARGS[i] as u64);
    let (clock_gettime, gettimeofday, time, getrandom) = if gex_abi::X86 {
        (228, 96, Some(201), 318)
    } else {
        (113, 169, None, 278)
    };
    let ret: u64 = if nr == clock_gettime {
        let buf = arg(gm, 1);
        let t = tick();
        if buf != 0 {
            let _ = gm.write_u64(buf, t / 1_000_000_000);
            let _ = gm.write_u64(buf + 8, t % 1_000_000_000);
        }
        0
    } else if nr == gettimeofday {
        let buf = arg(gm, 0);
        let t = tick();
        if buf != 0 {
            let _ = gm.write_u64(buf, t / 1_000_000_000);
            let _ = gm.write_u64(buf + 8, (t % 1_000_000_000) / 1_000);
        }
        0
    } else if Some(nr) == time {
        let secs = tick() / 1_000_000_000;
        let buf = arg(gm, 0);
        if buf != 0 {
            let _ = gm.write_u64(buf, secs);
        }
        secs
    } else if nr == getrandom {
        // A fixed xorshift stream, the same in every run.
        static SEED: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);
        let (buf, len) = (arg(gm, 0), arg(gm, 1).min(1 << 20));
        let mut bytes = Vec::with_capacity(len as usize);
        while bytes.len() < len as usize {
            let mut x = SEED.load(Ordering::Relaxed);
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            SEED.store(x, Ordering::Relaxed);
            bytes.extend_from_slice(&x.to_le_bytes());
        }
        bytes.truncate(len as usize);
        let _ = gm.write(buf, &bytes);
        len
    } else {
        return false;
    };
    let _ = gm.write_u64(ctx_addr + ctx::SYSCALL_RET as u64, ret);
    true
}
