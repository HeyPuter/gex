//! Threads, processes, register contexts, exceptions and APCs
//! (docs/nt-layer.md §1.3, §3.5).

use super::object::{self, Obj};
use super::vm::{self, page_prot, Kind};
use super::{off, st, sync, Abi, Call, Outcome, Process, Status, Thread, G, R};
use gex_abi::ctx;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// A queued user APC: `func(arg1, arg2, arg3)`.
#[derive(Clone, Copy, Debug)]
pub struct Apc {
    pub func: u64,
    pub args: [u64; 3],
}

// ---------------------------------------------------------------------------
// The x86-64 CONTEXT (winnt.h). Registers are in hardware order from Rax, the
// same order as gex's context, so register `i` is at `RAX + 8 * i`.
// ---------------------------------------------------------------------------

pub mod amd64 {
    pub const SIZE: u64 = 0x4d0;
    pub const P1HOME: u64 = 0x00;
    pub const CONTEXT_FLAGS: u64 = 0x30;
    pub const MXCSR: u64 = 0x34;
    pub const SEG_CS: u64 = 0x38;
    pub const SEG_DS: u64 = 0x3a;
    pub const SEG_ES: u64 = 0x3c;
    pub const SEG_FS: u64 = 0x3e;
    pub const SEG_GS: u64 = 0x40;
    pub const SEG_SS: u64 = 0x42;
    pub const EFLAGS: u64 = 0x44;
    pub const RAX: u64 = 0x78;
    pub const RSP: u64 = 0x98;
    pub const RIP: u64 = 0xf8;
    pub const FLTSAVE: u64 = 0x100;
    /// `CONTEXT_EX` follows the context in dispatcher frames.
    pub const CONTEXT_EX: u64 = 0x4d0;

    pub const CONTEXT_AMD64: u32 = 0x0010_0000;
    pub const CONTROL: u32 = CONTEXT_AMD64 | 0x1;
    pub const INTEGER: u32 = CONTEXT_AMD64 | 0x2;
    pub const SEGMENTS: u32 = CONTEXT_AMD64 | 0x4;
    pub const FLOATING_POINT: u32 = CONTEXT_AMD64 | 0x8;
    pub const FULL: u32 = CONTROL | INTEGER | FLOATING_POINT;
    pub const ALL: u32 = FULL | SEGMENTS | CONTEXT_AMD64 | 0x10;

    /// Wine's selectors (`cs64_sel` etc. on Linux).
    pub const CS: u16 = 0x33;
    pub const DS: u16 = 0x2b;
    pub const FS: u16 = 0x53;
}

/// gex flag bytes → EFLAGS.
fn read_eflags(g: &G<'_>, cx: u64) -> R<u32> {
    let b = |o: usize| g.u8(cx + o as u64).map(|v| (v & 1) as u32);
    Ok(b(ctx::CF)? | 0x2 | b(ctx::PF)? << 2 | b(ctx::AF)? << 4 | b(ctx::ZF)? << 6 | b(ctx::NF)? << 7
        | 0x200 | b(ctx::DF)? << 10 | b(ctx::VF)? << 11)
}

fn write_eflags(g: &mut G<'_>, cx: u64, f: u32) -> R<()> {
    for (o, bit) in [(ctx::CF, 0), (ctx::PF, 2), (ctx::AF, 4), (ctx::ZF, 6), (ctx::NF, 7), (ctx::DF, 10), (ctx::VF, 11)] {
        g.w8(cx + o as u64, ((f >> bit) & 1) as u8)?;
    }
    Ok(())
}

/// An f64 as the x87's 80-bit extended format.
fn f64_to_f80(v: f64) -> [u8; 10] {
    let bits = v.to_bits();
    let sign = (bits >> 63) as u16;
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1 << 52) - 1);
    let (e80, m80): (u16, u64) = if exp == 0 && frac == 0 {
        (0, 0)
    } else if exp == 0x7ff {
        (0x7fff, (1 << 63) | (frac << 11))
    } else if exp == 0 {
        // Denormal double: normalise.
        let shift = frac.leading_zeros() - 11;
        let m = frac << (shift + 11);
        ((1 - 1023 + 16383 - shift as i32) as u16, m)
    } else {
        ((exp - 1023 + 16383) as u16, (1 << 63) | (frac << 11))
    };
    let mut out = [0u8; 10];
    out[..8].copy_from_slice(&m80.to_le_bytes());
    out[8..].copy_from_slice(&((sign << 15) | e80).to_le_bytes());
    out
}

fn f80_to_f64(b: &[u8]) -> f64 {
    let m = u64::from_le_bytes(b[..8].try_into().unwrap());
    let se = u16::from_le_bytes([b[8], b[9]]);
    let sign = if se & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = (se & 0x7fff) as i32;
    if e == 0 && m == 0 {
        return 0.0 * sign;
    }
    if e == 0x7fff {
        return if m << 1 == 0 { sign * f64::INFINITY } else { f64::NAN };
    }
    sign * (m as f64) * 2f64.powi(e - 16383 - 63)
}

/// x87 register `phys` (physical, not stack-relative) of the gex context at
/// `cx`, in the 80-bit format a CONTEXT holds. gex keeps x87 registers as
/// f64 today; this pair is the one place that knows (S14 will make them
/// 80-bit).
fn x87_read_f80(g: &G<'_>, cx: u64, phys: u64) -> R<[u8; 10]> {
    Ok(f64_to_f80(f64::from_bits(g.u64(cx + ctx::X87 as u64 + 8 * phys)?)))
}

fn x87_write_f80(g: &mut G<'_>, cx: u64, phys: u64, v: &[u8]) -> R<()> {
    g.w64(cx + ctx::X87 as u64 + 8 * phys, f80_to_f64(v).to_bits())
}

/// The gex register state of the thread at `cx` as an x86-64 CONTEXT at `at`
/// (`CONTEXT_FULL | CONTEXT_SEGMENTS`). `rip` is where it resumes.
pub fn save_context64(g: &mut G<'_>, cx: u64, at: u64, rip: u64) -> R<()> {
    use amd64::*;
    g.zero(at, SIZE as usize)?;
    g.w32(at + CONTEXT_FLAGS, FULL | SEGMENTS)?;
    let mxcsr = g.u32(cx + ctx::MXCSR as u64)?;
    g.w32(at + MXCSR, mxcsr)?;
    g.w16(at + SEG_CS, CS)?;
    for o in [SEG_DS, SEG_ES, SEG_GS, SEG_SS] {
        g.w16(at + o, DS)?;
    }
    g.w16(at + SEG_FS, FS)?;
    let fl = read_eflags(g, cx)?;
    g.w32(at + EFLAGS, fl)?;
    for i in 0..16u8 {
        let v = g.u64(cx + ctx::x(i) as u64)?;
        g.w64(at + RAX + 8 * i as u64, v)?;
    }
    g.w64(at + RIP, rip)?;
    // FltSave (XSAVE_FORMAT): x87 as 80-bit, stack-relative; SSE registers.
    let f = at + FLTSAVE;
    let cw = g.u16(cx + ctx::X87_CW as u64)?;
    let top = g.u32(cx + ctx::X87_TOP as u64)? & 7;
    let sw = g.u16(cx + ctx::X87_SW as u64)? & !(7 << 11) | (top as u16) << 11;
    let tags = g.u16(cx + ctx::X87_TAGS as u64)? as u8;
    g.w16(f, cw)?;
    g.w16(f + 2, sw)?;
    g.w8(f + 4, tags)?;
    g.w32(f + 0x18, mxcsr)?;
    g.w32(f + 0x1c, 0xffff)?;
    for i in 0..8u64 {
        let phys = (top as u64 + i) & 7;
        let v = x87_read_f80(g, cx, phys)?;
        g.write(f + 0x20 + 16 * i, &v)?;
    }
    for i in 0..16u8 {
        let v = g.bytes(cx + ctx::v(i) as u64, 16)?;
        g.write(f + 0xa0 + 16 * i as u64, &v)?;
    }
    Ok(())
}

/// Load an x86-64 CONTEXT at `at` into the gex context at `cx`, honouring its
/// flags. Returns the new rip (or `None` without `CONTEXT_CONTROL`).
pub fn load_context64(g: &mut G<'_>, cx: u64, at: u64) -> R<Option<u64>> {
    use amd64::*;
    let flags = g.u32(at + CONTEXT_FLAGS)?;
    if flags & INTEGER == INTEGER {
        for i in 0..16u8 {
            if i == 4 {
                continue;
            }
            let v = g.u64(at + RAX + 8 * i as u64)?;
            g.w64(cx + ctx::x(i) as u64, v)?;
        }
    }
    if flags & FLOATING_POINT == FLOATING_POINT {
        let f = at + FLTSAVE;
        let mxcsr = g.u32(at + MXCSR)?;
        g.w32(cx + ctx::MXCSR as u64, mxcsr)?;
        let cw = g.u16(f)?;
        let sw = g.u16(f + 2)?;
        let tags = g.u8(f + 4)?;
        let top = ((sw >> 11) & 7) as u64;
        g.w16(cx + ctx::X87_CW as u64, cw)?;
        g.w16(cx + ctx::X87_SW as u64, sw & !(7 << 11))?;
        g.w32(cx + ctx::X87_TOP as u64, top as u32)?;
        g.w16(cx + ctx::X87_TAGS as u64, tags as u16)?;
        for i in 0..8u64 {
            let b = g.bytes(f + 0x20 + 16 * i, 10)?;
            let phys = (top + i) & 7;
            x87_write_f80(g, cx, phys, &b)?;
        }
        for i in 0..16u8 {
            let v = g.bytes(f + 0xa0 + 16 * i as u64, 16)?;
            g.write(cx + ctx::v(i) as u64, &v)?;
        }
    }
    if flags & CONTROL == CONTROL {
        let rsp = g.u64(at + RSP)?;
        g.w64(cx + ctx::x(4) as u64, rsp)?;
        let fl = g.u32(at + EFLAGS)?;
        write_eflags(g, cx, fl)?;
        let rip = g.u64(at + RIP)?;
        g.w64(cx + ctx::PC as u64, rip)?;
        return Ok(Some(rip));
    }
    Ok(None)
}

/// Initialise `CONTEXT_EX` after a context without extended state.
fn init_context_ex(g: &mut G<'_>, context: u64) -> R<()> {
    let x = context + amd64::CONTEXT_EX;
    let neg = (-(amd64::SIZE as i32)) as u32;
    g.w32(x, neg)?; // All.Offset
    g.w32(x + 4, amd64::SIZE as u32 + 24)?; // All.Length
    g.w32(x + 8, neg)?; // Legacy.Offset
    g.w32(x + 12, amd64::SIZE as u32)?; // Legacy.Length
    g.w32(x + 16, 0)?; // XState.Offset
    g.w32(x + 20, 25) // XState.Length
}

fn need_p64(c: &Call<'_>, what: &str) -> R<()> {
    if c.p.abi != Abi::P64 {
        return Err(c.unimplemented(&format!("{what} for a 32-bit process (N3)")));
    }
    Ok(())
}

/// `NtContinue(context, alertable)`.
pub fn nt_continue(c: &mut Call<'_>) -> R {
    need_p64(c, "NtContinue")?;
    let (at, alertable) = (c.a(0), c.a32(1) & 0xff != 0);
    continue_to(c, at, alertable)
}

/// `NtContinueEx(context, KCONTINUE_ARGUMENT * or BOOLEAN)`.
pub fn nt_continue_ex(c: &mut Call<'_>) -> R {
    need_p64(c, "NtContinueEx")?;
    let (at, arg) = (c.a(0), c.a(1));
    // A pointer to KCONTINUE_ARGUMENT {type, flags, reserved}, or a small
    // BOOLEAN, as Wine accepts.
    let alertable = if arg > 0xff {
        let flags = c.g.u32(arg + 4)?;
        flags & 0x1 != 0 // KCONTINUE_FLAG_TEST_ALERT
    } else {
        arg != 0
    };
    continue_to(c, at, alertable)
}

fn continue_to(c: &mut Call<'_>, at: u64, alertable: bool) -> R {
    let cx = c.t.ctx_addr;
    let rip = load_context64(&mut c.g, cx, at)?;
    let rip = match rip {
        Some(r) => r,
        None => c.g.u64(cx + ctx::PC as u64)?,
    };
    if alertable && !c.t.apcs.lock().unwrap().is_empty() {
        // Deliver on top of the context just loaded.
        c.outcome = Some(Outcome::Resume(rip));
        deliver_apc_at(c, rip, st::SUCCESS, false)?;
        return Ok(st::SUCCESS);
    }
    c.outcome = Some(Outcome::Resume(rip));
    Ok(st::SUCCESS)
}

/// Dispatch an exception into the guest: `KiUserExceptionDispatcher` with
/// the context and record on the stack (Wine's `exc_stack_layout`).
pub fn dispatch_exception64(c: &mut Call<'_>, rec: &[u8], context: u64) -> R<()> {
    let cx = c.t.ctx_addr;
    let rsp = c.g.u64(context + amd64::RSP)?;
    let stack = (rsp - 0x5c0) & !63;
    let ctx_bytes = c.g.bytes(context, amd64::SIZE as usize)?;
    c.g.write(stack, &ctx_bytes)?;
    init_context_ex(&mut c.g, stack)?;
    c.g.write(stack + 0x4f0, rec)?;
    // EXCEPTION_BREAKPOINT reports the int3's own address.
    let code = u32::from_le_bytes(rec[..4].try_into().unwrap());
    if code == st::BREAKPOINT {
        let rip = c.g.u64(stack + amd64::RIP)?;
        c.g.w64(stack + amd64::RIP, rip.wrapping_sub(1))?;
    }
    let rip = c.g.u64(stack + amd64::RIP)?;
    c.g.w64(stack + 0x590, rip)?; // machine_frame.rip
    c.g.w64(stack + 0x590 + 24, rsp)?; // machine_frame.rsp
    // Load the context as the thread's state (the dispatcher reads it back
    // from the stack), then point rsp at the frame.
    load_context64(&mut c.g, cx, context)?;
    c.g.w64(cx + ctx::x(4) as u64, stack)?;
    let entry = c.p.ntdll.lock().unwrap().ki_user_exception_dispatcher;
    c.g.w64(cx + ctx::PC as u64, entry)?;
    c.outcome = Some(Outcome::Resume(entry));
    Ok(())
}

/// `NtRaiseException(record, context, first_chance)`.
pub fn nt_raise_exception(c: &mut Call<'_>) -> R {
    need_p64(c, "NtRaiseException")?;
    let (rec_p, context, first) = (c.a(0), c.a(1), c.a32(2) & 0xff != 0);
    let rec = c.g.bytes(rec_p, 0x98)?;
    if first {
        dispatch_exception64(c, &rec, context)?;
        return Ok(st::SUCCESS);
    }
    // Second chance: nothing handled it. Wine terminates the process.
    let code = u32::from_le_bytes(rec[..4].try_into().unwrap());
    let addr = u64::from_le_bytes(rec[16..24].try_into().unwrap());
    eprintln!("gex-nt: unhandled exception {code:#010x} at {addr:#x}; ending the process");
    terminate_process(c, code);
    Ok(st::SUCCESS)
}

/// The register state after the current syscall returns with `status`,
/// as a CONTEXT at `at`.
fn save_after_syscall(c: &mut Call<'_>, at: u64, status: u32) -> R<u64> {
    let cx = c.t.ctx_addr;
    let rip = c.g.u64(cx + ctx::PC as u64)?;
    save_context64(&mut c.g, cx, at, rip)?;
    c.g.w64(at + amd64::RAX, status as u64)?;
    Ok(rip)
}

/// Deliver one queued user APC: `KiUserApcDispatcher` with Wine's
/// `apc_stack_layout`. The interrupted state is the syscall's return with
/// `STATUS_USER_APC`.
pub fn deliver_apc(c: &mut Call<'_>) -> R<()> {
    need_p64(c, "APC delivery")?;
    let cx = c.t.ctx_addr;
    let rip = c.g.u64(cx + ctx::PC as u64)?;
    deliver_apc_at(c, rip, st::USER_APC, true)
}

fn deliver_apc_at(c: &mut Call<'_>, rip: u64, status: u32, after_syscall: bool) -> R<()> {
    let Some(apc) = c.t.apcs.lock().unwrap().pop_front() else { return Ok(()) };
    let cx = c.t.ctx_addr;
    let rsp = c.g.u64(cx + ctx::x(4) as u64)?;
    let stack = (rsp & !15) - 0x560;
    if after_syscall {
        save_after_syscall(c, stack, status)?;
    } else {
        save_context64(&mut c.g, cx, stack, rip)?;
    }
    init_context_ex(&mut c.g, stack)?;
    // continue_arg: KCONTINUE_RESUME, TEST_ALERT | DELIVER_APC.
    c.g.zero(stack + 0x4f0, 0x18)?;
    c.g.w32(stack + 0x4f4, 0x1 | 0x2)?;
    c.g.w64(stack + amd64::P1HOME, apc.args[0])?;
    c.g.w64(stack + amd64::P1HOME + 8, apc.args[1])?;
    c.g.w64(stack + amd64::P1HOME + 16, apc.args[2])?;
    c.g.w64(stack + amd64::P1HOME + 24, apc.func)?;
    let saved_rip = c.g.u64(stack + amd64::RIP)?;
    c.g.w64(stack + 0x530, saved_rip)?;
    c.g.w64(stack + 0x530 + 24, rsp)?;
    c.g.w64(cx + ctx::x(4) as u64, stack)?;
    let entry = c.p.ntdll.lock().unwrap().ki_user_apc_dispatcher;
    c.g.w64(cx + ctx::PC as u64, entry)?;
    c.outcome = Some(Outcome::Resume(entry));
    Ok(())
}

/// `NtTestAlert()`: run queued APCs now.
pub fn nt_test_alert(c: &mut Call<'_>) -> R {
    if c.t.apcs.lock().unwrap().is_empty() {
        return Ok(st::SUCCESS);
    }
    deliver_apc(c)?;
    Ok(st::USER_APC)
}

/// `NtQueueApcThread(thread, func, arg1, arg2, arg3)`.
pub fn nt_queue_apc_thread(c: &mut Call<'_>) -> R {
    let h = c.a32(0);
    let apc = Apc { func: c.a(1), args: [c.a(2), c.a(3), c.a(4)] };
    let Obj::Thread(t) = object::resolve(c, h)? else {
        return Err(Status(st::OBJECT_TYPE_MISMATCH));
    };
    if t.exit_code.lock().unwrap().is_some() {
        return Err(Status(st::UNSUCCESSFUL));
    }
    t.apcs.lock().unwrap().push_back(apc);
    sync::wake_thread(c, &t);
    Ok(st::SUCCESS)
}

/// `NtCallbackReturn(result, len, status)`: end the innermost user-mode
/// callback. The nested loop's driver reads the result (docs/nt-layer.md
/// §5).
pub fn nt_callback_return(c: &mut Call<'_>) -> R {
    if c.t.callback_depth.load(Ordering::Acquire) == 0 {
        return Err(Status(st::NO_CALLBACK_ACTIVE));
    }
    c.outcome = Some(Outcome::CallbackReturn);
    Ok(st::SUCCESS)
}

// ---------------------------------------------------------------------------
// Creating threads
// ---------------------------------------------------------------------------

/// The Windows thread id for gex thread `gex_tid`: a multiple of 4, as on
/// Windows, and never equal to a process id ([`nt_pid`]).
pub fn nt_tid(gex_tid: i32) -> u32 {
    0x100 + 4 * gex_tid as u32
}

/// The Windows process id for gex process `gex_pid`.
pub fn nt_pid(gex_pid: i32) -> u32 {
    0x20 + 4 * gex_pid as u32
}

/// A new thread's TEB and stack, set up as Wine's `init_teb` and
/// `init_thread_stack` do. Returns the thread (not yet registered).
/// `slot` is `(context slot, gex tid)` for a thread gex already numbered
/// (the main thread); `None` allocates both.
pub fn create_thread_state(
    p: &Process,
    k: &mut crate::Kernel,
    g: &mut G<'_>,
    stack_reserve: u64,
    slot: Option<(u32, i32)>,
) -> R<Arc<Thread>> {
    let abi = p.abi;
    let (slot, gex_tid) = match slot {
        Some(s) => s,
        None => {
            let t = k.next_tid;
            k.next_tid += 1;
            let s = k.next_ctx_slot;
            k.next_ctx_slot += 1;
            (s, t)
        }
    };
    let ctx_addr = gex_abi::ctx_addr(slot);
    if ctx_addr + ctx::SIZE as u64 >= gex_abi::EXCLUSIVE_SCRATCH {
        return Err(Status(st::NO_MEMORY));
    }
    let word = p.alloc_wait_word().ok_or(Status(st::NO_MEMORY))?;
    let tid = nt_tid(gex_tid);

    // TEB: three pages for PE32+ (TEB64, then TEB32 at +0x2000, as Wine
    // lays them out); two for PE32.
    let teb_size = if abi == Abi::P64 { 0x3000 } else { 0x2000 };
    let stack_reserve = stack_reserve.max(0x10_0000);
    let stack_size = (stack_reserve + vm::GRANULE - 1) & !(vm::GRANULE - 1);
    let (teb, stack_lo) = {
        let mut stl = p.st.lock().unwrap();
        let teb = stl.vm.find_free(vm::GRANULE, true, vm::ALLOC_HIGH).ok_or(Status(st::NO_MEMORY))?;
        stl.vm.reserve(teb, teb_size, page_prot::READWRITE, true, Kind::Private, "[teb]");
        let lo = stl.vm.find_free(stack_size, false, vm::ALLOC_HIGH).ok_or(Status(st::NO_MEMORY))?;
        stl.vm.reserve(lo, stack_size, page_prot::READWRITE, true, Kind::Private, "[stack]");
        let _ = stl.vm.mirror(&mut k.mem, teb, teb + teb_size);
        let _ = stl.vm.mirror(&mut k.mem, lo, lo + stack_size);
        (teb, lo)
    };
    g.zero(teb, teb_size as usize)?;
    g.zero(stack_lo, stack_size as usize)?;
    let stack_hi = stack_lo + stack_size;
    let peb = p.peb.load(Ordering::Relaxed);
    macro_rules! t {
        ($f:ident) => {
            teb + off!(abi, teb::$f)
        };
    }
    g.wptr(t!(TIB_EXCEPTIONLIST), if abi == Abi::P32 { 0xffff_ffff } else { 0 })?;
    g.wptr(t!(TIB_STACKBASE), stack_hi)?;
    g.wptr(t!(TIB_STACKLIMIT), stack_lo)?;
    g.wptr(t!(TIB_FIBERDATA), 0x1e00)?;
    g.wptr(t!(TIB_SELF), teb)?;
    g.wptr(t!(CLIENTID_UNIQUEPROCESS), p.pid as u64)?;
    g.wptr(t!(CLIENTID_UNIQUETHREAD), tid as u64)?;
    g.wptr(t!(REALCLIENTID), p.pid as u64)?;
    g.wptr(t!(REALCLIENTID) + g.psize(), tid as u64)?;
    g.wptr(t!(PEB), peb)?;
    g.wptr(t!(ACTIVATIONCONTEXTSTACKPOINTER), t!(ACTIVATIONCONTEXTSTACK))?;
    let flc = t!(ACTIVATIONCONTEXTSTACK_FRAMELISTCACHE);
    g.wptr(flc, flc)?;
    g.wptr(flc + g.psize(), flc)?;
    g.w16(t!(STATICUNICODESTRING), 0)?;
    g.w16(t!(STATICUNICODESTRING) + 2, 261 * 2)?;
    g.wptr(t!(STATICUNICODESTRING) + off!(abi, unicode_string::BUFFER), t!(STATICUNICODEBUFFER))?;
    g.wptr(t!(DEALLOCATIONSTACK), stack_lo)?;
    let tl = t!(TLSLINKS);
    g.wptr(tl, tl)?;
    g.wptr(tl + g.psize(), tl)?;
    g.w32(t!(CURRENTLOCALE), 0x409)?;
    if abi == Abi::P64 {
        // The TEB32 Wine keeps beside every TEB64.
        let t32 = teb + 0x2000;
        use super::layout::x86::teb as t32o;
        g.w32(t32 + t32o::TIB_EXCEPTIONLIST, 0xffff_ffff)?;
        g.w32(t32 + t32o::TIB_SELF, t32 as u32)?;
        g.w32(t32 + t32o::TIB_FIBERDATA, 0x1e00)?;
        g.w32(t32 + t32o::PEB, (peb + 0x1000) as u32)?;
        g.w32(t32 + t32o::GDIBATCHCOUNT, teb as u32)?;
        g.w32(t32 + t32o::WOWTEBOFFSET, (-0x2000i32) as u32)?;
    }
    let sync_id = p.new_sync_id();
    p.sync.lock().unwrap().states.insert(sync_id, sync::SState::Done(false));
    Ok(Arc::new(Thread {
        tid,
        gex_tid,
        ctx_addr,
        teb,
        stack: (stack_lo, stack_hi),
        word,
        alerted: AtomicBool::new(false),
        apcs: Mutex::new(Default::default()),
        exit_code: Mutex::new(None),
        sync_id,
        suspend: AtomicU32::new(0),
        callback_depth: AtomicU32::new(0),
    }))
}

/// Set up a thread's gex registers to enter `LdrInitializeThunk` with a
/// CONTEXT that starts `RtlUserThreadStart(entry, arg)` (Wine's
/// `init_syscall_frame`).
pub fn init_start_frame(p: &Process, g: &mut G<'_>, t: &Thread, entry: u64, arg: u64) -> R<()> {
    let n = p.ntdll.lock().unwrap().clone();
    let cx = t.ctx_addr;
    g.zero(cx, ctx::SIZE)?;
    match p.abi {
        Abi::P64 => {
            use amd64::*;
            let rsp0 = t.stack.1 - 0x28;
            let context = ((rsp0 & !15) - SIZE) & !63;
            g.zero(context, SIZE as usize)?;
            g.w32(context + CONTEXT_FLAGS, FULL)?;
            g.w64(context + RAX + 8, entry)?; // Rcx
            g.w64(context + RAX + 16, arg)?; // Rdx
            g.w64(context + RSP, rsp0)?;
            g.w64(context + RIP, n.rtl_user_thread_start)?;
            g.w16(context + SEG_CS, CS)?;
            for o in [SEG_DS, SEG_ES, SEG_GS, SEG_SS] {
                g.w16(context + o, DS)?;
            }
            g.w16(context + SEG_FS, FS)?;
            g.w32(context + EFLAGS, 0x200)?;
            g.w16(context + FLTSAVE, 0x27f)?;
            g.w32(context + FLTSAVE + 0x18, 0x1f80)?;
            g.w32(context + MXCSR, 0x1f80)?;
            g.w64(cx + ctx::x(1) as u64, context)?; // rcx
            g.w64(cx + ctx::x(4) as u64, context - 8)?; // rsp
            g.w64(cx + ctx::PC as u64, n.ldr_initialize_thunk)?;
            g.w64(cx + ctx::GS_BASE as u64, t.teb)?;
        }
        Abi::P32 => return Err(p.unimplemented("starting a 32-bit thread (N3)")),
    }
    g.w64(cx + ctx::TID as u64, t.gex_tid as u64)?;
    g.w32(cx + ctx::SAFEPOINT_COUNTER as u64, 1024)?;
    g.w16(cx + ctx::X87_CW as u64, 0x27f)?;
    g.w32(cx + ctx::MXCSR as u64, 0x1f80)?;
    Ok(())
}

/// Write the PS_ATTRIBUTE outputs a thread creation asks for.
fn write_attr_list(c: &mut Call<'_>, list: u64, t: &Thread) -> R<()> {
    if list == 0 {
        return Ok(());
    }
    let ps = c.g.psize();
    let total = c.g.ptr(list)?;
    let attr_size = 4 * ps;
    let n = total.saturating_sub(ps) / attr_size;
    for i in 0..n {
        let a = list + ps + i * attr_size;
        let kind = c.g.ptr(a)? as u32;
        let size = c.g.ptr(a + ps)?;
        let val = c.g.ptr(a + 2 * ps)?;
        match kind {
            // PS_ATTRIBUTE_CLIENT_ID
            0x10003 => {
                if size >= 2 * ps {
                    c.g.wptr(val, c.p.pid as u64)?;
                    c.g.wptr(val + ps, t.tid as u64)?;
                }
            }
            // PS_ATTRIBUTE_TEB_ADDRESS
            0x10004 => {
                if size >= ps {
                    c.g.wptr(val, t.teb)?;
                }
            }
            _ => return Err(c.unimplemented(&format!("NtCreateThreadEx attribute {kind:#x}"))),
        }
    }
    Ok(())
}

/// `NtCreateThreadEx(*handle, access, attr, process, start, param, flags,
/// zero_bits, stack_commit, stack_reserve, attr_list)`.
pub fn nt_create_thread_ex(c: &mut Call<'_>) -> R {
    let (hp, access, process, start, param, flags) = (c.a(0), c.a32(1), c.a32(3), c.a(4), c.a(5), c.a32(6));
    let (reserve, list) = (c.a(9), c.a(10));
    if !matches!(object::resolve(c, process)?, Obj::Process) {
        return Err(c.unimplemented("NtCreateThreadEx in another process"));
    }
    const SUSPENDED: u32 = 0x1;
    const SKIP_THREAD_ATTACH: u32 = 0x2;
    const HIDE_FROM_DEBUGGER: u32 = 0x4;
    const SKIP_LOADER_INIT: u32 = 0x20;
    const BYPASS_PROCESS_FREEZE: u32 = 0x40;
    if flags & !(SUSPENDED | SKIP_THREAD_ATTACH | HIDE_FROM_DEBUGGER | SKIP_LOADER_INIT | BYPASS_PROCESS_FREEZE) != 0 {
        return Err(c.unimplemented(&format!("NtCreateThreadEx flags {flags:#x}")));
    }
    let t = {
        let mut k = c.kernel.lock().unwrap_or_else(|e| e.into_inner());
        let t = create_thread_state(c.p, &mut k, &mut c.g, reserve, None)?;
        k.live_threads += 1;
        t
    };
    if flags & (SKIP_THREAD_ATTACH | SKIP_LOADER_INIT) != 0 {
        let abi = c.p.abi;
        let mut f = 0u16;
        if flags & SKIP_THREAD_ATTACH != 0 {
            f |= 1 << 3;
        }
        if flags & SKIP_LOADER_INIT != 0 {
            f |= 1 << 14;
        }
        c.g.w16(t.teb + off!(abi, teb::SAMETEBFLAGS), f)?;
    }
    init_start_frame(c.p, &mut c.g, &t, start, param)?;
    c.p.add_thread(Arc::clone(&t));
    write_attr_list(c, list, &t)?;
    let h = c.p.st.lock().unwrap().handles.insert(Obj::Thread(Arc::clone(&t)), access);
    c.g.wptr(hp, h as u64)?;
    if flags & SUSPENDED != 0 {
        t.suspend.store(1, Ordering::Release);
    } else {
        c.outcome = Some(Outcome::SpawnThread { ctx_addr: t.ctx_addr, gex_tid: t.gex_tid });
    }
    Ok(st::SUCCESS)
}

/// `NtResumeThread(handle, *prev_count)`. A thread created suspended is
/// started when its count reaches zero; suspending a running thread is not
/// supported.
pub fn nt_resume_thread(c: &mut Call<'_>) -> R {
    let (h, prev_p) = (c.a32(0), c.a(1));
    let Obj::Thread(t) = object::resolve(c, h)? else {
        return Err(Status(st::OBJECT_TYPE_MISMATCH));
    };
    let prev = t
        .suspend
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| Some(n.saturating_sub(1)))
        .unwrap();
    if prev == 1 {
        c.outcome = Some(Outcome::SpawnThread { ctx_addr: t.ctx_addr, gex_tid: t.gex_tid });
    }
    c.g.out32(prev_p, prev)?;
    Ok(st::SUCCESS)
}

/// End the calling thread with `code`.
pub fn exit_thread(c: &mut Call<'_>, code: u32) {
    let t = Arc::clone(&c.t);
    *t.exit_code.lock().unwrap() = Some(code);
    sync::abandon_mutants(c, t.tid);
    sync::signal_done(c, t.sync_id);
    c.p.remove_thread(&t);
    {
        let mut k = c.kernel();
        k.live_threads = k.live_threads.saturating_sub(1);
    }
    c.outcome = Some(Outcome::ThreadExit);
}

/// `NtTerminateThread(handle, status)`.
pub fn nt_terminate_thread(c: &mut Call<'_>) -> R {
    let (h, code) = (c.a32(0), c.a32(1));
    let target = if h == 0 { Obj::Thread(Arc::clone(&c.t)) } else { object::resolve(c, h)? };
    let Obj::Thread(t) = target else { return Err(Status(st::OBJECT_TYPE_MISMATCH)) };
    if !Arc::ptr_eq(&t, &c.t) {
        if t.suspend.load(Ordering::Acquire) > 0 {
            // Never started: just retire it.
            *t.exit_code.lock().unwrap() = Some(code);
            sync::signal_done(c, t.sync_id);
            c.p.remove_thread(&t);
            return Ok(st::SUCCESS);
        }
        return Err(c.unimplemented("NtTerminateThread on another running thread"));
    }
    if c.p.threads.read().unwrap().by_tid.len() <= 1 {
        // The last thread: that is the process ending.
        terminate_process(c, code);
        return Ok(st::SUCCESS);
    }
    exit_thread(c, code);
    Ok(st::SUCCESS)
}

/// End the process with `code`.
pub fn terminate_process(c: &mut Call<'_>, code: u32) {
    c.p.exit_code.store(code, Ordering::Release);
    c.p.terminating.store(true, Ordering::Release);
    wake_everyone(c);
    c.outcome = Some(Outcome::Exit(code));
}

fn wake_everyone(c: &mut Call<'_>) {
    let ts: Vec<Arc<Thread>> = c.p.threads.read().unwrap().by_tid.values().cloned().collect();
    for t in ts {
        if !Arc::ptr_eq(&t, &c.t) {
            sync::wake_thread(c, &t);
        }
    }
}

/// `NtTerminateProcess(handle, status)`. Handle 0 means "every thread but
/// this one", which `RtlExitUserProcess` does before detaching DLLs.
pub fn nt_terminate_process(c: &mut Call<'_>) -> R {
    let (h, code) = (c.a32(0), c.a32(1));
    if h == 0 {
        c.p.exit_code.store(code, Ordering::Release);
        let others: Vec<Arc<Thread>> = c
            .p
            .threads
            .read()
            .unwrap()
            .by_tid
            .values()
            .filter(|t| !Arc::ptr_eq(t, &c.t))
            .cloned()
            .collect();
        for t in others {
            // Threads that never started just go; running ones stop at
            // their next syscall (see `syscall`).
            if t.suspend.load(Ordering::Acquire) > 0 {
                c.p.remove_thread(&t);
            } else {
                t.apcs.lock().unwrap().clear();
                c.p.kill_list.lock().unwrap().push(t.tid);
                sync::wake_thread(c, &t);
            }
        }
        return Ok(st::SUCCESS);
    }
    match object::resolve(c, h)? {
        Obj::Process => {
            terminate_process(c, code);
            Ok(st::SUCCESS)
        }
        _ => Err(Status(st::OBJECT_TYPE_MISMATCH)),
    }
}

/// `NtGetNextThread(process, thread, access, attributes, flags, *handle)`.
pub fn nt_get_next_thread(c: &mut Call<'_>) -> R {
    let (process, h, access, hp) = (c.a32(0), c.a32(1), c.a32(2), c.a(5));
    if !matches!(object::resolve(c, process)?, Obj::Process) {
        return Err(c.unimplemented("NtGetNextThread in another process"));
    }
    let after = if h == 0 {
        None
    } else {
        match object::resolve(c, h)? {
            Obj::Thread(t) => Some(t.tid),
            _ => return Err(Status(st::OBJECT_TYPE_MISMATCH)),
        }
    };
    let mut tids: Vec<u32> = c.p.threads.read().unwrap().by_tid.keys().copied().collect();
    tids.sort_unstable();
    let next = tids.into_iter().find(|&t| after.is_none_or(|a| t > a));
    let Some(tid) = next else { return Err(Status(st::NO_MORE_ENTRIES)) };
    let t = c.p.thread_by_tid(tid).ok_or(Status(st::NO_MORE_ENTRIES))?;
    let nh = c.p.st.lock().unwrap().handles.insert(Obj::Thread(t), access);
    c.g.wptr(hp, nh as u64)?;
    Ok(st::SUCCESS)
}

fn get_thread(c: &Call<'_>, h: u32) -> R<Arc<Thread>> {
    match object::resolve(c, h)? {
        Obj::Thread(t) => Ok(t),
        _ => Err(Status(st::OBJECT_TYPE_MISMATCH)),
    }
}

/// `NtQueryInformationThread(handle, class, buffer, len, *ret_len)`.
pub fn nt_query_information_thread(c: &mut Call<'_>) -> R {
    let (h, class, buf, len, ret) = (c.a32(0), c.a32(1), c.a(2), c.a32(3) as u64, c.a(4));
    let t = get_thread(c, h)?;
    let abi = c.p.abi;
    let ps = c.g.psize();
    let n = match class {
        // ThreadBasicInformation
        0 => {
            let size = off!(abi, thread_basic_information::SIZEOF);
            if len < size {
                return Err(Status(st::INFO_LENGTH_MISMATCH));
            }
            c.g.zero(buf, size as usize)?;
            let code = t.exit_code.lock().unwrap().unwrap_or(st::PENDING);
            c.g.w32(buf + off!(abi, thread_basic_information::EXITSTATUS), code)?;
            c.g.wptr(buf + off!(abi, thread_basic_information::TEBBASEADDRESS), t.teb)?;
            c.g.wptr(buf + off!(abi, thread_basic_information::CLIENTID), c.p.pid as u64)?;
            c.g.wptr(buf + off!(abi, thread_basic_information::CLIENTID) + ps, t.tid as u64)?;
            c.g.wptr(buf + off!(abi, thread_basic_information::AFFINITYMASK), 1)?;
            c.g.w32(buf + off!(abi, thread_basic_information::PRIORITY), 8)?;
            c.g.w32(buf + off!(abi, thread_basic_information::BASEPRIORITY), 8)?;
            size
        }
        // ThreadTimes: creation, exit, kernel, user.
        1 => {
            if len < 32 {
                return Err(Status(st::INFO_LENGTH_MISMATCH));
            }
            c.g.zero(buf, 32)?;
            32
        }
        // ThreadAmILastThread
        12 => {
            if len < 4 {
                return Err(Status(st::INFO_LENGTH_MISMATCH));
            }
            let last = c.p.threads.read().unwrap().by_tid.len() <= 1;
            c.g.w32(buf, last as u32)?;
            4
        }
        // ThreadIsTerminated
        20 => {
            if len < 4 {
                return Err(Status(st::INFO_LENGTH_MISMATCH));
            }
            c.g.w32(buf, t.exit_code.lock().unwrap().is_some() as u32)?;
            4
        }
        // ThreadHideFromDebugger
        17 => {
            if len < 1 {
                return Err(Status(st::INFO_LENGTH_MISMATCH));
            }
            c.g.w8(buf, 0)?;
            1
        }
        // ThreadSuspendCount
        35 => {
            if len < 4 {
                return Err(Status(st::INFO_LENGTH_MISMATCH));
            }
            c.g.w32(buf, t.suspend.load(Ordering::Acquire))?;
            4
        }
        _ => return Err(c.unimplemented(&format!("NtQueryInformationThread class {class}"))),
    };
    c.g.out32(ret, n as u32)?;
    Ok(st::SUCCESS)
}

/// `NtSetInformationThread(handle, class, buffer, len)`.
pub fn nt_set_information_thread(c: &mut Call<'_>) -> R {
    let (h, class, _buf, _len) = (c.a32(0), c.a32(1), c.a(2), c.a32(3));
    get_thread(c, h)?;
    match class {
        // Scheduling hints gex's host threads don't have: priorities,
        // affinity, ideal processor, priority boost, power throttling, and
        // hiding from a debugger that isn't there. Accepting them is what a
        // scheduler without them would do.
        2 | 3 | 4 | 13 | 14 | 17 | 31 | 49 => Ok(st::SUCCESS),
        // ThreadZeroTlsCell: `TlsFree` clears the slot in every thread
        // (Wine's `virtual_clear_tls_index`).
        10 => {
            if !Arc::ptr_eq(&get_thread(c, h)?, &c.t) {
                return Err(c.unimplemented("ThreadZeroTlsCell on another thread"));
            }
            if _len != 4 {
                return Err(Status(st::INVALID_PARAMETER));
            }
            let index = c.g.u32(_buf)? as u64;
            let abi = c.p.abi;
            let ps = c.g.psize();
            let tebs: Vec<u64> = c.p.threads.read().unwrap().by_tid.values().map(|t| t.teb).collect();
            if index >= 64 + 1024 {
                return Err(Status(st::INVALID_PARAMETER));
            }
            for teb in tebs {
                if index < 64 {
                    c.g.wptr(teb + off!(abi, teb::TLSSLOTS) + index * ps, 0)?;
                } else {
                    let exp = c.g.ptr(teb + off!(abi, teb::TLSEXPANSIONSLOTS))?;
                    if exp != 0 {
                        c.g.wptr(exp + (index - 64) * ps, 0)?;
                    }
                }
            }
            Ok(st::SUCCESS)
        }
        // ThreadQuerySetWin32StartAddress, ThreadDescription, ThreadWineNativeThreadName:
        // names and labels for debuggers.
        9 | 38 | 1100 => Ok(st::SUCCESS),
        _ => Err(c.unimplemented(&format!("NtSetInformationThread class {class}"))),
    }
}

/// `NtOpenThread(*handle, access, attr, CLIENT_ID *)`.
pub fn nt_open_thread(c: &mut Call<'_>) -> R {
    let (hp, access, cid) = (c.a(0), c.a32(1), c.a(3));
    let tid = c.g.ptr(cid + c.g.psize())? as u32;
    let t = c.p.thread_by_tid(tid).ok_or(Status(st::INVALID_CID))?;
    let h = c.p.st.lock().unwrap().handles.insert(Obj::Thread(t), access);
    c.g.wptr(hp, h as u64)?;
    Ok(st::SUCCESS)
}

/// `NtOpenProcess(*handle, access, attr, CLIENT_ID *)`: only this process.
pub fn nt_open_process(c: &mut Call<'_>) -> R {
    let (hp, access, cid) = (c.a(0), c.a32(1), c.a(3));
    let pid = c.g.ptr(cid)? as u32;
    if pid != c.p.pid {
        return Err(Status(st::INVALID_CID));
    }
    let h = c.p.st.lock().unwrap().handles.insert(Obj::Process, access);
    c.g.wptr(hp, h as u64)?;
    Ok(st::SUCCESS)
}

/// `NtGetContextThread(handle, context)`, for the calling thread.
pub fn nt_get_context_thread(c: &mut Call<'_>) -> R {
    need_p64(c, "NtGetContextThread")?;
    let (h, at) = (c.a32(0), c.a(1));
    let t = get_thread(c, h)?;
    if !Arc::ptr_eq(&t, &c.t) {
        return Err(c.unimplemented("NtGetContextThread on another thread"));
    }
    let flags = c.g.u32(at + amd64::CONTEXT_FLAGS)?;
    let tmp = {
        // Capture into a scratch copy, then copy the parts asked for.
        let cx = c.t.ctx_addr;
        let rip = c.g.u64(cx + ctx::PC as u64)?;
        let mut tmp = vec![0u8; amd64::SIZE as usize];
        let rsp = c.g.u64(cx + ctx::x(4) as u64)?;
        let scratch = (rsp - 0x1000 - amd64::SIZE) & !63;
        save_context64(&mut c.g, cx, scratch, rip)?;
        c.g.w64(scratch + amd64::RAX, st::SUCCESS as u64)?;
        tmp.copy_from_slice(&c.g.bytes(scratch, amd64::SIZE as usize)?);
        tmp
    };
    let copy = |c: &mut Call<'_>, from: u64, to: u64| c.g.write(at + from, &tmp[from as usize..to as usize]);
    if flags & amd64::CONTROL == amd64::CONTROL {
        copy(c, amd64::SEG_CS, amd64::SEG_CS + 2)?;
        copy(c, amd64::SEG_SS, amd64::EFLAGS + 4)?;
        copy(c, amd64::RSP, amd64::RSP + 8)?;
        copy(c, amd64::RIP, amd64::RIP + 8)?;
    }
    if flags & amd64::INTEGER == amd64::INTEGER {
        copy(c, amd64::RAX, amd64::RSP)?;
        copy(c, amd64::RSP + 8, amd64::RIP)?;
    }
    if flags & amd64::SEGMENTS == amd64::SEGMENTS {
        copy(c, amd64::SEG_DS, amd64::SEG_SS)?;
    }
    if flags & amd64::FLOATING_POINT == amd64::FLOATING_POINT {
        copy(c, amd64::MXCSR, amd64::MXCSR + 4)?;
        copy(c, amd64::FLTSAVE, amd64::FLTSAVE + 512)?;
    }
    Ok(st::SUCCESS)
}

/// `NtSetContextThread(handle, context)`, for the calling thread.
pub fn nt_set_context_thread(c: &mut Call<'_>) -> R {
    need_p64(c, "NtSetContextThread")?;
    let (h, at) = (c.a32(0), c.a(1));
    let t = get_thread(c, h)?;
    if !Arc::ptr_eq(&t, &c.t) {
        return Err(c.unimplemented("NtSetContextThread on another thread"));
    }
    let cx = c.t.ctx_addr;
    if let Some(rip) = load_context64(&mut c.g, cx, at)? {
        c.outcome = Some(Outcome::Resume(rip));
    }
    Ok(st::SUCCESS)
}

/// `NtQueryInformationProcess(handle, class, buffer, len, *ret_len)`.
pub fn nt_query_information_process(c: &mut Call<'_>) -> R {
    let (h, class, buf, len, ret) = (c.a32(0), c.a32(1), c.a(2), c.a32(3) as u64, c.a(4));
    if !matches!(object::resolve(c, h)?, Obj::Process) {
        return Err(Status(st::OBJECT_TYPE_MISMATCH));
    }
    let abi = c.p.abi;
    let ps = c.g.psize();
    let need = |n: u64| if len < n { Err(Status(st::INFO_LENGTH_MISMATCH)) } else { Ok(()) };
    let n: u64 = match class {
        // ProcessBasicInformation
        0 => {
            let size = off!(abi, process_basic_information::SIZEOF);
            need(size)?;
            c.g.zero(buf, size as usize)?;
            c.g.w32(buf + off!(abi, process_basic_information::EXITSTATUS), st::PENDING)?;
            c.g.wptr(buf + off!(abi, process_basic_information::PEBBASEADDRESS), c.p.peb.load(Ordering::Relaxed))?;
            c.g.wptr(buf + off!(abi, process_basic_information::AFFINITYMASK), 1)?;
            c.g.w32(buf + off!(abi, process_basic_information::BASEPRIORITY), 8)?;
            c.g.wptr(buf + off!(abi, process_basic_information::UNIQUEPROCESSID), c.p.pid as u64)?;
            c.g.wptr(buf + off!(abi, process_basic_information::INHERITEDFROMUNIQUEPROCESSID), 0)?;
            size
        }
        // ProcessDebugPort, ProcessDebugObjectHandle: no debugger.
        7 => {
            need(ps)?;
            c.g.wptr(buf, 0)?;
            ps
        }
        30 => return Err(Status(0xc000_0353)), // STATUS_PORT_NOT_SET
        // ProcessDebugFlags: 1 = no debug inherit, i.e. not debugged.
        31 => {
            need(4)?;
            c.g.w32(buf, 1)?;
            4
        }
        // ProcessDefaultHardErrorMode
        12 => {
            need(4)?;
            c.g.w32(buf, 1)?;
            4
        }
        // ProcessWow64Information: gex presents a native Windows of the
        // program's bitness (docs/nt-layer.md §1.3).
        26 => {
            need(ps)?;
            c.g.wptr(buf, 0)?;
            ps
        }
        // ProcessImageInformation
        37 => {
            let size = off!(abi, section_image_information::SIZEOF);
            need(size)?;
            let img = {
                let stl = c.p.st.lock().unwrap();
                let base = c.g.ptr(c.p.peb.load(Ordering::Relaxed) + off!(abi, peb::IMAGEBASEADDRESS))?;
                stl.images.get(&base).map(|v| Arc::clone(&v.pe))
            };
            let img = img.ok_or(Status(st::UNSUCCESSFUL))?;
            super::section::write_image_info(&mut c.g, buf, &img)?;
            size
        }
        // ProcessExecuteFlags: DEP permanently on.
        34 => {
            need(4)?;
            c.g.w32(buf, 0x2 | 0x8)?;
            4
        }
        // ProcessCookie
        36 => {
            need(4)?;
            c.g.w32(buf, 0x6765_7821)?;
            4
        }
        // ProcessImageFileName, ProcessImageFileNameWin32: a UNICODE_STRING
        // then the name.
        27 | 43 => {
            let path = if class == 43 {
                super::to_w(&c.p.cfg.image_path)
            } else {
                super::to_w(&format!("\\??\\{}", c.p.cfg.image_path))
            };
            let us = c.g.us_size();
            let size = us + path.len() as u64 * 2 + 2;
            c.g.out32(ret, size as u32)?;
            if len < size {
                return Err(Status(st::INFO_LENGTH_MISMATCH));
            }
            let b = path.len() as u16 * 2;
            c.g.w_ustr_hdr(buf, b, b + 2, buf + us)?;
            let mut z = path;
            z.push(0);
            c.g.write_wchars(buf + us, &z)?;
            return Ok(st::SUCCESS);
        }
        // ProcessSessionInformation
        24 => {
            need(4)?;
            c.g.w32(buf, 1)?;
            4
        }
        // ProcessHandleCount
        20 => {
            need(4)?;
            c.g.w32(buf, 32)?;
            4
        }
        // ProcessPriorityClass: PROCESS_PRIORITY_CLASS {Foreground, class}.
        18 => {
            need(2)?;
            c.g.w8(buf, 0)?;
            c.g.w8(buf + 1, 2)?; // PROCESS_PRIOCLASS_NORMAL
            2
        }
        _ => return Err(c.unimplemented(&format!("NtQueryInformationProcess class {class}"))),
    };
    c.g.out32(ret, n as u32)?;
    Ok(st::SUCCESS)
}

/// `NtSetInformationProcess(handle, class, buffer, len)`.
pub fn nt_set_information_process(c: &mut Call<'_>) -> R {
    let (h, class) = (c.a32(0), c.a32(1));
    if !matches!(object::resolve(c, h)?, Obj::Process) {
        return Err(Status(st::OBJECT_TYPE_MISMATCH));
    }
    match class {
        // Error mode, affinity, priority class, execute flags (DEP is
        // always on), power throttling: recorded by nobody, as a scheduler
        // and MMU without these features would.
        12 | 18 | 21 | 34 | 77 => Ok(st::SUCCESS),
        _ => Err(c.unimplemented(&format!("NtSetInformationProcess class {class}"))),
    }
}

#[allow(dead_code)]
fn _unused(_: &Abi) {}
