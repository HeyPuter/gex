//! Diagnostics for comparing two runs syscall by syscall (the function tier's
//! lockstep check, `gexdev/tests/lockstep/lockstep`). Both are off unless
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

/// The entry line: `E tid pc nr a0..a5 | x0..x30 sp nzcv pa [cs] vhash` (cs: x86 only).
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
    // The code selector: a WoW64 thread's mode (0 is the process's own).
    if gex_abi::X86 {
        s += &format!(" cs={:x}", gm.read_u32(ctx_addr + ctx::CS as u64).unwrap_or(0));
    }
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

/// `GEX_TRACE_INSNS`: one line per traced instruction, to GEX_TRACE_FILE
/// (default stderr): `pc eax ecx edx ebx esp ebp esi edi flags`, the flags
/// as x86 EFLAGS bits (CF PF AF ZF SF DF OF). i386 width when the code is
/// 32-bit (the process's class, or a WoW64 thread's `ctx::CS`).
pub fn log_trace(gm: &GuestMem<'_>, ctx_addr: u64, pc: u64) {
    static OUT: std::sync::OnceLock<Mutex<Box<dyn Write + Send>>> = std::sync::OnceLock::new();
    let out = OUT.get_or_init(|| {
        let w: Box<dyn Write + Send> = match std::env::var("GEX_TRACE_FILE") {
            Ok(p) => Box::new(std::io::LineWriter::new(std::fs::File::create(p).expect("GEX_TRACE_FILE"))),
            Err(_) => Box::new(std::io::stderr()),
        };
        Mutex::new(w)
    });
    let f = |o: usize| gm.read(ctx_addr + ctx::NZCV as u64 + o as u64, 1).map(|b| b[0] as u64 & 1).unwrap_or(0);
    // N Z C V P A D bytes -> SF ZF CF OF PF AF DF bits.
    let eflags = f(2) | f(4) << 2 | f(5) << 4 | f(1) << 6 | f(0) << 7 | f(6) << 10 | f(3) << 11;
    let cs = gm.read_u32(ctx_addr + ctx::CS as u64).unwrap_or(0);
    let compat = ctx::cs_compat(cs, COMPAT.load(Ordering::Relaxed));
    let mut s = format!("{pc:x}");
    for n in 0..8u8 {
        let v = rd(gm, ctx_addr + ctx::x(n) as u64);
        s += &format!(" {:x}", if compat { v & 0xffff_ffff } else { v });
    }
    s += &format!(" {eflags:x}\n");
    let _ = out.lock().unwrap_or_else(|e| e.into_inner()).write_all(s.as_bytes());
}


/// The result line: `R tid result`.
pub fn log_result(gm: &GuestMem<'_>, ctx_addr: u64, tid: i32) {
    let Some(f) = log() else { return };
    let s = format!("R {tid} {:x}\n", rd(gm, ctx_addr + ctx::SYSCALL_RET as u64));
    let _ = f.lock().unwrap_or_else(|e| e.into_inner()).write_all(s.as_bytes());
}

/// Whether the process is an i386 program (compatibility mode): its
/// syscall numbers and structure layouts are the 32-bit ABI's.
static COMPAT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_compat(on: bool) {
    COMPAT.store(on, Ordering::Relaxed);
}

/// The i386 ABI's clock and random syscalls (`fake_syscall`).
fn fake_syscall32(gm: &mut GuestMem<'_>, ctx_addr: u64, nr: u64) -> bool {
    let arg = |gm: &GuestMem<'_>, i: usize| rd(gm, ctx_addr + ctx::SYSCALL_ARGS[i] as u64) & 0xffff_ffff;
    let w32 = |gm: &mut GuestMem<'_>, a: u64, v: u64| {
        let _ = gm.write(a, &(v as u32).to_le_bytes());
    };
    let ret: u64 = match nr {
        // clock_gettime64: struct __kernel_timespec, two 64-bit fields.
        403 => {
            let (buf, t) = (arg(gm, 1), tick());
            if buf != 0 {
                let _ = gm.write_u64(buf, t / 1_000_000_000);
                let _ = gm.write_u64(buf + 8, t % 1_000_000_000);
            }
            0
        }
        // clock_gettime: the 32-bit timespec.
        265 => {
            let (buf, t) = (arg(gm, 1), tick());
            if buf != 0 {
                w32(gm, buf, t / 1_000_000_000);
                w32(gm, buf + 4, t % 1_000_000_000);
            }
            0
        }
        // gettimeofday: the 32-bit timeval.
        78 => {
            let (buf, t) = (arg(gm, 0), tick());
            if buf != 0 {
                w32(gm, buf, t / 1_000_000_000);
                w32(gm, buf + 4, (t % 1_000_000_000) / 1_000);
            }
            0
        }
        // time.
        13 => {
            let (buf, secs) = (arg(gm, 0), tick() / 1_000_000_000);
            if buf != 0 {
                w32(gm, buf, secs);
            }
            secs
        }
        // getrandom: the same fixed stream as the 64-bit ABI's.
        355 => {
            let (buf, len) = (arg(gm, 0), arg(gm, 1).min(1 << 20));
            let bytes = random_bytes(len as usize);
            let _ = gm.write(buf, &bytes);
            len
        }
        _ => return false,
    };
    let _ = gm.write_u64(ctx_addr + ctx::SYSCALL_RET as u64, ret);
    true
}

/// `len` bytes of a fixed xorshift stream, the same in every run.
fn random_bytes(len: usize) -> Vec<u8> {
    static SEED: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        let mut x = SEED.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        SEED.store(x, Ordering::Relaxed);
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    bytes.truncate(len);
    bytes
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
    if COMPAT.load(Ordering::Relaxed) {
        return fake_syscall32(gm, ctx_addr, nr);
    }
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
