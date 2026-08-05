//! Signals (PLAN §8).
//!
//! Delivery works because of the tail-call design: every host call flushes
//! the guest's registers to the thread context first, so at any syscall or
//! safepoint the architectural state is complete and a handler frame can be
//! built from it exactly. Resuming is then just re-entering translated code
//! at a different PC — there is no wasm stack to unwind, because guest call
//! depth never became wasm call depth.
//!
//! Returning from a handler happens one of two ways. A libc that sets
//! `SA_RESTORER` returns to its own trampoline, which issues `rt_sigreturn`
//! like any other syscall. Otherwise the return address is
//! [`SIGRETURN_TRAMPOLINE`], a reserved address with no translation: branching
//! to it lands in the dispatch-miss handler, which recognises it and restores
//! the frame. That avoids having to synthesise and translate a code page just
//! to hold two instructions.

use crate::errno::Errno;
use crate::GuestMem;
use gex_abi::ctx;

/// Signal numbers.
pub mod sig {
    pub const SIGHUP: u32 = 1;
    pub const SIGINT: u32 = 2;
    pub const SIGQUIT: u32 = 3;
    pub const SIGILL: u32 = 4;
    pub const SIGTRAP: u32 = 5;
    pub const SIGABRT: u32 = 6;
    pub const SIGBUS: u32 = 7;
    pub const SIGFPE: u32 = 8;
    pub const SIGKILL: u32 = 9;
    pub const SIGUSR1: u32 = 10;
    pub const SIGSEGV: u32 = 11;
    pub const SIGUSR2: u32 = 12;
    pub const SIGPIPE: u32 = 13;
    pub const SIGALRM: u32 = 14;
    pub const SIGTERM: u32 = 15;
    pub const SIGCHLD: u32 = 17;
    pub const SIGCONT: u32 = 18;
    pub const SIGSTOP: u32 = 19;
    pub const SIGTSTP: u32 = 20;
    pub const SIGWINCH: u32 = 28;
    pub const SIGSYS: u32 = 31;
    /// Signals 1..=64 exist; 0 is "no signal".
    pub const MAX: u32 = 64;
}

/// `sa_flags` bits.
pub mod sa {
    pub const NOCLDSTOP: u64 = 0x0000_0001;
    pub const NOCLDWAIT: u64 = 0x0000_0002;
    pub const SIGINFO: u64 = 0x0000_0004;
    pub const RESTORER: u64 = 0x0400_0000;
    pub const ONSTACK: u64 = 0x0800_0000;
    pub const RESTART: u64 = 0x1000_0000;
    pub const NODEFER: u64 = 0x4000_0000;
    pub const RESETHAND: u64 = 0x8000_0000;
}

/// `SIG_DFL`.
pub const SIG_DFL: u64 = 0;
/// `SIG_IGN`.
pub const SIG_IGN: u64 = 1;

/// Return address installed when the guest supplies no `sa_restorer`.
///
/// Inside the runtime-reserved region, four-byte aligned, and deliberately
/// without a translation so that returning to it reaches the dispatch-miss
/// handler.
pub const SIGRETURN_TRAMPOLINE: u64 = gex_abi::RESERVED_END - 16;

/// What the kernel should do when a signal has no handler.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum DefaultAction {
    /// Terminate the process.
    Terminate,
    /// Terminate and note that a core would have been written.
    Core,
    /// Discard the signal.
    Ignore,
    /// Stop the process.
    Stop,
    /// Continue a stopped process.
    Continue,
}

/// The default disposition of a signal.
pub fn default_action(signal: u32) -> DefaultAction {
    match signal {
        sig::SIGCHLD | sig::SIGCONT | sig::SIGWINCH | 29 => DefaultAction::Ignore,
        sig::SIGSTOP | sig::SIGTSTP | 21 | 22 => DefaultAction::Stop,
        sig::SIGQUIT | sig::SIGILL | sig::SIGTRAP | sig::SIGABRT | sig::SIGBUS | sig::SIGFPE
        | sig::SIGSEGV | sig::SIGSYS => DefaultAction::Core,
        _ => DefaultAction::Terminate,
    }
}

/// One installed disposition.
#[derive(Copy, Clone, Debug, Default)]
pub struct SigAction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

/// An alternate signal stack.
#[derive(Copy, Clone, Debug, Default)]
pub struct SigAltStack {
    pub sp: u64,
    pub flags: u32,
    pub size: u64,
}

/// What the caller should do after the kernel considered a pending signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Nothing to deliver; carry on.
    None,
    /// A handler frame is set up; resume translated code at this address.
    Resume(u64),
    /// No handler, and the default disposition ends the process.
    Terminate { signal: u32, core: bool },
}

/// Per-process signal state.
#[derive(Debug)]
pub struct Signals {
    actions: [SigAction; sig::MAX as usize + 1],
    /// Signals raised but not yet delivered.
    pending: u64,
    /// Signals the guest has blocked.
    pub blocked: u64,
    pub alt_stack: SigAltStack,
    /// Set while a handler runs, so `rt_sigreturn` knows a frame exists.
    pub in_handler: bool,
}

impl Default for Signals {
    fn default() -> Self {
        Self::new()
    }
}

/// Bit for a signal in a `sigset_t`. Signal 1 is bit 0.
fn bit(signal: u32) -> u64 {
    if signal == 0 || signal > 64 {
        0
    } else {
        1u64 << (signal - 1)
    }
}

impl Signals {
    pub fn new() -> Signals {
        Signals {
            actions: [SigAction::default(); sig::MAX as usize + 1],
            pending: 0,
            blocked: 0,
            alt_stack: SigAltStack::default(),
            in_handler: false,
        }
    }

    pub fn action(&self, signal: u32) -> SigAction {
        self.actions[(signal as usize).min(sig::MAX as usize)]
    }

    pub fn set_action(&mut self, signal: u32, action: SigAction) -> Result<(), Errno> {
        // SIGKILL and SIGSTOP cannot be caught or ignored.
        if signal == 0 || signal > sig::MAX || signal == sig::SIGKILL || signal == sig::SIGSTOP {
            return Err(Errno::EINVAL);
        }
        self.actions[signal as usize] = action;
        Ok(())
    }

    /// Mark a signal pending.
    pub fn raise(&mut self, signal: u32) {
        if signal > 0 && signal <= sig::MAX {
            self.pending |= bit(signal);
        }
    }

    pub fn has_pending(&self) -> bool {
        self.pending & !self.blocked != 0
    }

    /// The lowest-numbered deliverable signal, if any.
    fn next_deliverable(&self) -> Option<u32> {
        let ready = self.pending & !self.blocked;
        if ready == 0 {
            return None;
        }
        Some(ready.trailing_zeros() + 1)
    }
}

/// Byte offsets inside the aarch64 signal frame the kernel builds.
///
/// The layout is `struct rt_sigframe`: a `siginfo_t` followed by a
/// `ucontext`, whose `uc_mcontext` is a `sigcontext`. Every offset here is
/// fixed ABI that libc unwinders and `longjmp` implementations read directly.
pub mod frame {
    /// `siginfo_t` occupies the first 128 bytes.
    pub const SIGINFO: usize = 0;
    pub const SIGINFO_SIZE: usize = 128;
    /// The `ucontext` follows it.
    pub const UCONTEXT: usize = SIGINFO_SIZE;
    /// Fields within the ucontext.
    pub const UC_FLAGS: usize = 0;
    pub const UC_LINK: usize = 8;
    pub const UC_STACK: usize = 16;
    pub const UC_SIGMASK: usize = 40;
    /// `uc_mcontext`, after the 128-byte signal mask.
    pub const UC_MCONTEXT: usize = 176;
    /// Fields within the `sigcontext`.
    pub const MC_FAULT_ADDRESS: usize = 0;
    pub const MC_REGS: usize = 8;
    pub const MC_SP: usize = 8 + 31 * 8;
    pub const MC_PC: usize = MC_SP + 8;
    pub const MC_PSTATE: usize = MC_PC + 8;
    /// `__reserved[4096]`, where the FP state record lives.
    pub const MC_RESERVED: usize = MC_PSTATE + 8;
    pub const SIGCONTEXT_SIZE: usize = MC_RESERVED + 4096;
    pub const UCONTEXT_SIZE: usize = UC_MCONTEXT + SIGCONTEXT_SIZE;
    pub const FRAME_SIZE: usize = UCONTEXT + UCONTEXT_SIZE;
}

/// Build a handler frame for `signal` and point the context at the handler.
///
/// Returns the address to resume at. The caller has already established that
/// a handler is installed.
pub fn setup_frame(
    signals: &mut Signals,
    m: &mut GuestMem<'_>,
    ctx_addr: u64,
    signal: u32,
    fault_address: u64,
) -> Result<u64, Errno> {
    let action = signals.action(signal);
    let sp = m.read_u64(ctx_addr + ctx::SP as u64)?;

    // An `SA_ONSTACK` handler runs on the alternate stack, but only if one is
    // installed and we are not already on it.
    let base = if action.flags & sa::ONSTACK != 0 && signals.alt_stack.size > 0 {
        let alt = &signals.alt_stack;
        let top = alt.sp + alt.size;
        if sp > alt.sp && sp <= top {
            sp
        } else {
            top
        }
    } else {
        sp
    };

    // The frame sits below the current stack pointer, 16-byte aligned.
    let frame_addr = (base - frame::FRAME_SIZE as u64) & !0xf;
    m.zero(frame_addr, frame::FRAME_SIZE)?;

    // siginfo: si_signo, si_errno, si_code, then the fault address for the
    // synchronous signals, which is what a crash handler reads.
    m.write_u32(frame_addr, signal)?;
    m.write_u32(frame_addr + 8, fault_code(signal))?;
    if matches!(signal, sig::SIGSEGV | sig::SIGBUS | sig::SIGILL | sig::SIGFPE) {
        m.write_u64(frame_addr + 16, fault_address)?;
    }

    let uc = frame_addr + frame::UCONTEXT as u64;
    m.write_u64(uc + frame::UC_FLAGS as u64, 0)?;
    m.write_u64(uc + frame::UC_LINK as u64, 0)?;
    // uc_stack describes the stack the handler is running on.
    m.write_u64(uc + frame::UC_STACK as u64, signals.alt_stack.sp)?;
    m.write_u32(uc + frame::UC_STACK as u64 + 8, signals.alt_stack.flags)?;
    m.write_u64(uc + frame::UC_STACK as u64 + 16, signals.alt_stack.size)?;
    // The mask to restore on return is the one in force right now.
    m.write_u64(uc + frame::UC_SIGMASK as u64, signals.blocked)?;

    // The machine context: every general register, sp, pc and pstate, taken
    // from the thread context, which a host call has already made complete.
    let mc = uc + frame::UC_MCONTEXT as u64;
    m.write_u64(mc + frame::MC_FAULT_ADDRESS as u64, fault_address)?;
    for i in 0..31u8 {
        let v = m.read_u64(ctx_addr + ctx::x(i) as u64)?;
        m.write_u64(mc + frame::MC_REGS as u64 + i as u64 * 8, v)?;
    }
    m.write_u64(mc + frame::MC_SP as u64, sp)?;
    let pc = m.read_u64(ctx_addr + ctx::PC as u64)?;
    m.write_u64(mc + frame::MC_PC as u64, pc)?;
    m.write_u64(mc + frame::MC_PSTATE as u64, read_pstate(m, ctx_addr)?)?;

    // Block this signal for the duration of the handler unless asked not to,
    // plus whatever the action's mask names.
    signals.blocked |= action.mask;
    if action.flags & sa::NODEFER == 0 {
        signals.blocked |= bit(signal);
    }
    signals.pending &= !bit(signal);
    signals.in_handler = true;

    // A one-shot handler reverts before it runs, as `SA_RESETHAND` requires.
    if action.flags & sa::RESETHAND != 0 {
        let _ = signals.set_action(signal, SigAction::default());
    }

    // Hand the handler its arguments and a way back.
    m.write_u64(ctx_addr + ctx::SP as u64, frame_addr)?;
    m.write_u64(ctx_addr + ctx::x(0) as u64, signal as u64)?;
    m.write_u64(ctx_addr + ctx::x(1) as u64, frame_addr)?;
    m.write_u64(ctx_addr + ctx::x(2) as u64, uc)?;
    let ret = if action.flags & sa::RESTORER != 0 && action.restorer != 0 {
        action.restorer
    } else {
        SIGRETURN_TRAMPOLINE
    };
    m.write_u64(ctx_addr + ctx::x(30) as u64, ret)?;
    m.write_u64(ctx_addr + ctx::PC as u64, action.handler)?;
    Ok(action.handler)
}

/// Restore the context a handler frame captured. Returns the resume address.
pub fn sigreturn(
    signals: &mut Signals,
    m: &mut GuestMem<'_>,
    ctx_addr: u64,
) -> Result<u64, Errno> {
    // The frame is at the stack pointer the handler was entered with, which
    // it is required to have preserved.
    let frame_addr = m.read_u64(ctx_addr + ctx::SP as u64)?;
    let uc = frame_addr + frame::UCONTEXT as u64;
    let mc = uc + frame::UC_MCONTEXT as u64;

    for i in 0..31u8 {
        let v = m.read_u64(mc + frame::MC_REGS as u64 + i as u64 * 8)?;
        m.write_u64(ctx_addr + ctx::x(i) as u64, v)?;
    }
    let sp = m.read_u64(mc + frame::MC_SP as u64)?;
    let pc = m.read_u64(mc + frame::MC_PC as u64)?;
    let pstate = m.read_u64(mc + frame::MC_PSTATE as u64)?;
    m.write_u64(ctx_addr + ctx::SP as u64, sp)?;
    m.write_u64(ctx_addr + ctx::PC as u64, pc)?;
    write_pstate(m, ctx_addr, pstate)?;

    signals.blocked = m.read_u64(uc + frame::UC_SIGMASK as u64)?;
    signals.in_handler = false;
    Ok(pc)
}

/// Consider the highest-priority pending signal.
pub fn dispatch(
    signals: &mut Signals,
    m: &mut GuestMem<'_>,
    ctx_addr: u64,
) -> Result<Delivery, Errno> {
    let Some(signal) = signals.next_deliverable() else {
        return Ok(Delivery::None);
    };
    let action = signals.action(signal);
    if action.handler == SIG_IGN {
        signals.pending &= !bit(signal);
        return Ok(Delivery::None);
    }
    if action.handler == SIG_DFL {
        signals.pending &= !bit(signal);
        return Ok(match default_action(signal) {
            DefaultAction::Ignore => Delivery::None,
            // Stopping needs a scheduler to resume from; until then it ends
            // the process, which is at least visible rather than a silent
            // hang.
            DefaultAction::Core | DefaultAction::Stop => Delivery::Terminate {
                signal,
                core: true,
            },
            _ => Delivery::Terminate {
                signal,
                core: false,
            },
        });
    }
    let resume = setup_frame(signals, m, ctx_addr, signal, 0)?;
    Ok(Delivery::Resume(resume))
}

/// The `si_code` a synchronous fault reports.
fn fault_code(signal: u32) -> u32 {
    match signal {
        // SEGV_MAPERR: address not mapped to an object.
        sig::SIGSEGV => 1,
        // ILL_ILLOPC: illegal opcode.
        sig::SIGILL => 1,
        // BUS_ADRALN: invalid address alignment.
        sig::SIGBUS => 1,
        // FPE_INTDIV: integer divide by zero.
        sig::SIGFPE => 1,
        // TRAP_BRKPT: process breakpoint.
        sig::SIGTRAP => 1,
        // SI_KERNEL for everything the kernel raised itself.
        _ => 0x80,
    }
}

/// Pack the four condition flags into a PSTATE word.
fn read_pstate(m: &GuestMem<'_>, ctx_addr: u64) -> Result<u64, Errno> {
    let n = m.read_u32(ctx_addr + ctx::NF as u64)? as u64;
    let z = m.read_u32(ctx_addr + ctx::ZF as u64)? as u64;
    let c = m.read_u32(ctx_addr + ctx::CF as u64)? as u64;
    let v = m.read_u32(ctx_addr + ctx::VF as u64)? as u64;
    // EL0t with interrupts enabled, plus NZCV in their architectural places.
    Ok((n << 31) | (z << 30) | (c << 29) | (v << 28))
}

fn write_pstate(m: &mut GuestMem<'_>, ctx_addr: u64, pstate: u64) -> Result<(), Errno> {
    m.write_u32(ctx_addr + ctx::NF as u64, ((pstate >> 31) & 1) as u32)?;
    m.write_u32(ctx_addr + ctx::ZF as u64, ((pstate >> 30) & 1) as u32)?;
    m.write_u32(ctx_addr + ctx::CF as u64, ((pstate >> 29) & 1) as u32)?;
    m.write_u32(ctx_addr + ctx::VF as u64, ((pstate >> 28) & 1) as u32)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTX: u64 = 0x1000;

    fn setup() -> (Vec<u8>, Signals) {
        let mut buf = vec![0u8; 0x80000];
        {
            let mut m = GuestMem::new(&mut buf);
            // A plausible stack pointer with room below it for a frame.
            m.write_u64(CTX + ctx::SP as u64, 0x40000).unwrap();
            m.write_u64(CTX + ctx::PC as u64, 0x400100).unwrap();
            for i in 0..31u8 {
                m.write_u64(CTX + ctx::x(i) as u64, 0x1000 + i as u64).unwrap();
            }
        }
        (buf, Signals::new())
    }

    #[test]
    fn kill_and_stop_cannot_be_caught() {
        let mut s = Signals::new();
        assert_eq!(
            s.set_action(sig::SIGKILL, SigAction::default()).err(),
            Some(Errno::EINVAL)
        );
        assert_eq!(
            s.set_action(sig::SIGSTOP, SigAction::default()).err(),
            Some(Errno::EINVAL)
        );
        assert!(s.set_action(sig::SIGUSR1, SigAction::default()).is_ok());
    }

    #[test]
    fn blocked_signals_stay_pending() {
        let mut s = Signals::new();
        s.blocked = bit(sig::SIGUSR1);
        s.raise(sig::SIGUSR1);
        assert!(!s.has_pending());
        s.blocked = 0;
        assert!(s.has_pending());
    }

    #[test]
    fn an_unhandled_fatal_signal_terminates() {
        let (mut buf, mut s) = setup();
        let mut m = GuestMem::new(&mut buf);
        s.raise(sig::SIGTERM);
        assert_eq!(
            dispatch(&mut s, &mut m, CTX).unwrap(),
            Delivery::Terminate {
                signal: sig::SIGTERM,
                core: false
            }
        );
    }

    #[test]
    fn an_ignored_signal_is_discarded() {
        let (mut buf, mut s) = setup();
        let mut m = GuestMem::new(&mut buf);
        s.set_action(
            sig::SIGUSR1,
            SigAction {
                handler: SIG_IGN,
                ..Default::default()
            },
        )
        .unwrap();
        s.raise(sig::SIGUSR1);
        assert_eq!(dispatch(&mut s, &mut m, CTX).unwrap(), Delivery::None);
        assert!(!s.has_pending());
    }

    #[test]
    fn sigchld_defaults_to_ignored() {
        let (mut buf, mut s) = setup();
        let mut m = GuestMem::new(&mut buf);
        s.raise(sig::SIGCHLD);
        assert_eq!(dispatch(&mut s, &mut m, CTX).unwrap(), Delivery::None);
    }

    #[test]
    fn a_handler_frame_round_trips_the_context() {
        let (mut buf, mut s) = setup();
        let mut m = GuestMem::new(&mut buf);
        s.set_action(
            sig::SIGUSR1,
            SigAction {
                handler: 0x400500,
                ..Default::default()
            },
        )
        .unwrap();
        s.raise(sig::SIGUSR1);

        let d = dispatch(&mut s, &mut m, CTX).unwrap();
        assert_eq!(d, Delivery::Resume(0x400500));
        // The handler sees its arguments and a return path.
        assert_eq!(
            m.read_u64(CTX + ctx::x(0) as u64).unwrap(),
            sig::SIGUSR1 as u64
        );
        assert_eq!(
            m.read_u64(CTX + ctx::x(30) as u64).unwrap(),
            SIGRETURN_TRAMPOLINE
        );
        assert!(s.in_handler);
        // The signal is blocked while its own handler runs.
        assert_ne!(s.blocked & bit(sig::SIGUSR1), 0);
        // The stack moved below the original.
        let handler_sp = m.read_u64(CTX + ctx::SP as u64).unwrap();
        assert!(handler_sp < 0x40000);
        assert_eq!(handler_sp % 16, 0);

        // Returning restores everything the frame captured.
        let resume = sigreturn(&mut s, &mut m, CTX).unwrap();
        assert_eq!(resume, 0x400100);
        assert_eq!(m.read_u64(CTX + ctx::SP as u64).unwrap(), 0x40000);
        assert_eq!(m.read_u64(CTX + ctx::x(5) as u64).unwrap(), 0x1005);
        assert_eq!(s.blocked, 0);
        assert!(!s.in_handler);
    }

    #[test]
    fn flags_are_preserved_across_a_handler() {
        let (mut buf, mut s) = setup();
        let mut m = GuestMem::new(&mut buf);
        m.write_u32(CTX + ctx::NF as u64, 1).unwrap();
        m.write_u32(CTX + ctx::CF as u64, 1).unwrap();
        s.set_action(
            sig::SIGUSR1,
            SigAction {
                handler: 0x400500,
                ..Default::default()
            },
        )
        .unwrap();
        s.raise(sig::SIGUSR1);
        dispatch(&mut s, &mut m, CTX).unwrap();
        // The handler runs with whatever flags it likes.
        m.write_u32(CTX + ctx::NF as u64, 0).unwrap();
        m.write_u32(CTX + ctx::CF as u64, 0).unwrap();
        m.write_u32(CTX + ctx::ZF as u64, 1).unwrap();
        sigreturn(&mut s, &mut m, CTX).unwrap();
        assert_eq!(m.read_u32(CTX + ctx::NF as u64).unwrap(), 1);
        assert_eq!(m.read_u32(CTX + ctx::CF as u64).unwrap(), 1);
        assert_eq!(m.read_u32(CTX + ctx::ZF as u64).unwrap(), 0);
    }

    #[test]
    fn resethand_reverts_before_the_handler_runs() {
        let (mut buf, mut s) = setup();
        let mut m = GuestMem::new(&mut buf);
        s.set_action(
            sig::SIGUSR1,
            SigAction {
                handler: 0x400500,
                flags: sa::RESETHAND,
                ..Default::default()
            },
        )
        .unwrap();
        s.raise(sig::SIGUSR1);
        dispatch(&mut s, &mut m, CTX).unwrap();
        assert_eq!(s.action(sig::SIGUSR1).handler, SIG_DFL);
    }

    #[test]
    fn an_alternate_stack_is_used_when_requested() {
        let (mut buf, mut s) = setup();
        let mut m = GuestMem::new(&mut buf);
        s.alt_stack = SigAltStack {
            sp: 0x60000,
            flags: 0,
            size: 0x8000,
        };
        s.set_action(
            sig::SIGSEGV,
            SigAction {
                handler: 0x400600,
                flags: sa::ONSTACK,
                ..Default::default()
            },
        )
        .unwrap();
        s.raise(sig::SIGSEGV);
        dispatch(&mut s, &mut m, CTX).unwrap();
        let sp = m.read_u64(CTX + ctx::SP as u64).unwrap();
        assert!(
            sp > 0x60000 && sp <= 0x68000,
            "handler should run on the alternate stack, got {sp:#x}"
        );
    }

    #[test]
    fn the_lowest_numbered_signal_is_delivered_first() {
        let (mut buf, mut s) = setup();
        let mut m = GuestMem::new(&mut buf);
        for n in [sig::SIGUSR2, sig::SIGUSR1] {
            s.set_action(
                n,
                SigAction {
                    handler: 0x400000 + n as u64,
                    ..Default::default()
                },
            )
            .unwrap();
            s.raise(n);
        }
        assert_eq!(
            dispatch(&mut s, &mut m, CTX).unwrap(),
            Delivery::Resume(0x400000 + sig::SIGUSR1 as u64)
        );
    }
}
