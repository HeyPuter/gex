//! Signals (PLAN §8).
//!
//! Delivery works because of the tail-call design: every host call flushes the
//! guest's registers to the thread context first, so at any syscall or safepoint
//! the architectural state is complete and a handler frame can be built from it.
//! Resuming is just re-entering translated code at a different PC -- there is no
//! wasm stack to unwind.
//!
//! Returning from a handler: a libc that sets `SA_RESTORER` returns to its own
//! trampoline, which issues `rt_sigreturn`. Otherwise the return address is
//! [`SIGRETURN_TRAMPOLINE`], a reserved address with no translation, so
//! branching to it lands in the dispatch-miss handler, which restores the frame.

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
    pub const SIGVTALRM: u32 = 26;
    pub const SIGPROF: u32 = 27;
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

/// Return address installed when the guest supplies no `sa_restorer`. In the
/// reserved region and deliberately without a translation, so returning to it
/// reaches the dispatch-miss handler.
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
        sig::SIGQUIT
        | sig::SIGILL
        | sig::SIGTRAP
        | sig::SIGABRT
        | sig::SIGBUS
        | sig::SIGFPE
        | sig::SIGSEGV
        | sig::SIGSYS => DefaultAction::Core,
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
#[derive(Debug, Clone)]
pub struct Signals {
    actions: [SigAction; sig::MAX as usize + 1],
    /// Signals raised but not yet delivered.
    pending: u64,
    /// Who sent each pending signal, when a process did (`kill`, `tgkill`):
    /// what its siginfo reports instead of `SI_KERNEL`. `None` for one the
    /// kernel raised itself.
    sources: [Option<Source>; sig::MAX as usize + 1],
    /// Signals the guest has blocked.
    pub blocked: u64,
    pub alt_stack: SigAltStack,
    /// Set while a handler runs, so `rt_sigreturn` knows a frame exists.
    pub in_handler: bool,
    /// The i386 frame most recently set up was the non-rt `sigframe` (a
    /// handler without `SA_SIGINFO`), whose `sigreturn` reads a different
    /// layout. Decided where the frame is written, because the return may
    /// arrive through the host trampoline as well as through `int 0x80`.
    pub legacy_frame: bool,
    /// The mask a signal frame should record instead of the current one.
    ///
    /// `rt_sigsuspend` runs with a caller-chosen mask and must restore the old
    /// one *after* the handler returns; the frame carries the mask across. So
    /// suspend leaves the old mask here, `setup_frame` writes it into the frame,
    /// and `sigreturn` restores it -- without it the woken signal stays blocked
    /// and pending forever (`timeout` spun 2.2 million times).
    pub suspend_restore: Option<u64>,
}

impl Default for Signals {
    fn default() -> Self {
        Self::new()
    }
}

impl Signals {
    /// The signal state a forked child starts with.
    ///
    /// Dispositions and the blocked mask are inherited; the pending set is
    /// not, and neither is a handler frame -- the child begins between
    /// instructions, not inside a handler.
    pub fn fork(&self) -> Signals {
        Signals {
            actions: self.actions,
            pending: 0,
            sources: [None; sig::MAX as usize + 1],
            blocked: self.blocked,
            suspend_restore: None,
            alt_stack: self.alt_stack,
            in_handler: false,
            legacy_frame: false,
        }
    }

    /// The signal state after `execve`.
    ///
    /// A handler is an address in the old image, so every caught signal reverts
    /// to default -- otherwise the new program inherits the address of a handler
    /// that no longer exists (`timeout 3 sleep 30` jumped into dash's old
    /// handler). Ignored stays ignored, the blocked mask and pending signals
    /// carry over, per POSIX; the alternate stack does not -- it too pointed into
    /// the old image.
    pub fn exec(&mut self) {
        for action in self.actions.iter_mut() {
            if action.handler != SIG_DFL && action.handler != SIG_IGN {
                *action = SigAction::default();
            }
        }
        self.alt_stack = SigAltStack::default();
        self.in_handler = false;
        self.suspend_restore = None;
    }
}

/// Bit for a signal in a `sigset_t`. Signal 1 is bit 0.
/// A process that sent a signal, for its siginfo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Source {
    /// `si_code`: [`SI_USER`] or [`SI_TKILL`].
    pub code: i32,
    pub pid: i32,
    pub uid: u32,
}

/// `si_code` for a signal sent by `kill`.
pub const SI_USER: i32 = 0;
/// `si_code` for a signal sent by `tkill` or `tgkill`.
pub const SI_TKILL: i32 = -6;

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
            sources: [None; sig::MAX as usize + 1],
            blocked: 0,
            suspend_restore: None,
            alt_stack: SigAltStack::default(),
            in_handler: false,
            legacy_frame: false,
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

    /// Mark a signal pending, as the kernel raising it itself (`SI_KERNEL`).
    pub fn raise(&mut self, signal: u32) {
        if signal > 0 && signal <= sig::MAX {
            self.pending |= bit(signal);
            self.sources[signal as usize] = None;
        }
    }

    /// Mark a signal pending, as sent by a process: `kill` (`SI_USER`) or
    /// `tkill`/`tgkill` (`SI_TKILL`). A standard signal does not queue, so the
    /// last sender is the one reported.
    pub fn raise_from(&mut self, signal: u32, source: Source) {
        if signal > 0 && signal <= sig::MAX {
            self.pending |= bit(signal);
            self.sources[signal as usize] = Some(source);
        }
    }

    /// Forget who sent `signal`: a synchronous fault reports itself, whatever
    /// a process may also have sent.
    pub fn forget_source(&mut self, signal: u32) {
        if signal > 0 && signal <= sig::MAX {
            self.sources[signal as usize] = None;
        }
    }

    /// Take who sent `signal`, as its delivery reports it.
    pub fn take_source(&mut self, signal: u32) -> Option<Source> {
        self.sources.get_mut(signal as usize).and_then(Option::take)
    }

    /// Take the lowest-numbered pending signal in `set`, blocked or not.
    ///
    /// `sigwaitinfo` is used precisely on signals the caller has blocked, so
    /// this deliberately ignores the mask [`Signals::next_deliverable`] respects.
    pub fn take_pending_in(&mut self, set: u64) -> Option<u32> {
        let ready = self.pending & set;
        if ready == 0 {
            return None;
        }
        let signal = ready.trailing_zeros() + 1;
        self.pending &= !bit(signal);
        Some(signal)
    }

    /// Everything raised, deliverable or not. For diagnostics only.
    pub fn pending_raw(&self) -> u64 {
        self.pending
    }

    pub fn has_pending(&self) -> bool {
        self.pending & !self.blocked != 0
    }

    /// Whether a pending signal would actually *do* something -- a handler, or a
    /// non-`Ignore` default. `has_pending` only asks whether an unblocked bit is
    /// set, which is wrong for ending a wait: `SIGCHLD` is ignored by default and
    /// arrives on every child exit, and would otherwise interrupt a shell's wait
    /// for nothing.
    pub fn would_deliver(&self) -> bool {
        let mut ready = self.pending & !self.blocked;
        while ready != 0 {
            let signal = ready.trailing_zeros() + 1;
            ready &= ready - 1;
            let action = self.action(signal);
            if action.handler == SIG_IGN {
                continue;
            }
            if action.handler == SIG_DFL && default_action(signal) == DefaultAction::Ignore {
                continue;
            }
            return true;
        }
        false
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

/// Byte offsets inside the x86-64 `struct rt_sigframe`.
///
/// The return address the handler `ret`s through comes first, then the
/// `ucontext` (whose `uc_mcontext` is a `struct sigcontext` of 23 words in
/// the architecture's fixed order), then the `siginfo`. The FP state is a
/// separate 512-byte `fxsave` area above the frame that `uc_mcontext.fpstate`
/// points to. Fixed ABI: libc's unwinders and `swapcontext` read it.
pub mod xframe {
    pub const PRETCODE: usize = 0;
    pub const UCONTEXT: usize = 8;
    pub const UC_FLAGS: usize = 0;
    pub const UC_LINK: usize = 8;
    pub const UC_STACK: usize = 16;
    pub const UC_MCONTEXT: usize = 40;
    /// `sigcontext` words: r8..r15, rdi, rsi, rbp, rbx, rdx, rax, rcx, rsp,
    /// rip, eflags, then cs/gs/fs/ss packed, err, trapno, oldmask, cr2,
    /// fpstate pointer, and reserved space.
    pub const MC_R8: usize = 0;
    pub const MC_RDI: usize = 64;
    pub const MC_RSI: usize = 72;
    pub const MC_RBP: usize = 80;
    pub const MC_RBX: usize = 88;
    pub const MC_RDX: usize = 96;
    pub const MC_RAX: usize = 104;
    pub const MC_RCX: usize = 112;
    pub const MC_RSP: usize = 120;
    pub const MC_RIP: usize = 128;
    pub const MC_EFLAGS: usize = 136;
    pub const MC_CS: usize = 144;
    pub const MC_ERR: usize = 152;
    pub const MC_TRAPNO: usize = 160;
    pub const MC_OLDMASK: usize = 168;
    pub const MC_CR2: usize = 176;
    pub const MC_FPSTATE: usize = 184;
    pub const SIGCONTEXT_SIZE: usize = 256;
    pub const UC_SIGMASK: usize = UC_MCONTEXT + SIGCONTEXT_SIZE;
    pub const UCONTEXT_SIZE: usize = UC_SIGMASK + 8;
    pub const SIGINFO: usize = UCONTEXT + UCONTEXT_SIZE;
    pub const SIGINFO_SIZE: usize = 128;
    pub const FRAME_SIZE: usize = SIGINFO + SIGINFO_SIZE;
    /// The `fxsave` image.
    pub const FPSTATE_SIZE: usize = 512;
    /// The System V red zone below `rsp` a signal frame must not touch.
    pub const RED_ZONE: u64 = 128;
    /// `struct sigcontext` register order as gex GPR numbers.
    pub const GPR_ORDER: [u8; 16] = [8, 9, 10, 11, 12, 13, 14, 15, 7, 6, 5, 3, 2, 0, 1, 4];
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
    compat: bool,
) -> Result<u64, Errno> {
    if gex_abi::PPC {
        return setup_frame_ppc(signals, m, ctx_addr, signal, fault_address);
    }
    if gex_abi::X86 && compat {
        return setup_frame_i386(signals, m, ctx_addr, signal, fault_address);
    }
    if gex_abi::X86 {
        return setup_frame_x86(signals, m, ctx_addr, signal, fault_address);
    }
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
    write_siginfo(signals, m, frame_addr, signal, fault_address, true)?;

    let uc = frame_addr + frame::UCONTEXT as u64;
    m.write_u64(uc + frame::UC_FLAGS as u64, 0)?;
    m.write_u64(uc + frame::UC_LINK as u64, 0)?;
    // uc_stack describes the stack the handler is running on.
    m.write_u64(uc + frame::UC_STACK as u64, signals.alt_stack.sp)?;
    m.write_u32(uc + frame::UC_STACK as u64 + 8, signals.alt_stack.flags)?;
    m.write_u64(uc + frame::UC_STACK as u64 + 16, signals.alt_stack.size)?;
    // The mask to restore on return is the one in force right now.
    m.write_u64(
        uc + frame::UC_SIGMASK as u64,
        signals.suspend_restore.take().unwrap_or(signals.blocked),
    )?;

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

/// The x86-64 frame builder. See [`xframe`] for the layout.
fn setup_frame_x86(
    signals: &mut Signals,
    m: &mut GuestMem<'_>,
    ctx_addr: u64,
    signal: u32,
    fault_address: u64,
) -> Result<u64, Errno> {
    let action = signals.action(signal);
    let sp = m.read_u64(ctx_addr + ctx::SP as u64)?;

    let on_alt = action.flags & sa::ONSTACK != 0 && signals.alt_stack.size > 0;
    let base = if on_alt {
        let alt = &signals.alt_stack;
        let top = alt.sp + alt.size;
        if sp > alt.sp && sp <= top {
            sp
        } else {
            top
        }
    } else {
        // Leave the red zone below the interrupted code's stack pointer.
        sp - xframe::RED_ZONE
    };

    // The FP state above the frame, 64-byte aligned; the frame below it so
    // that `rsp + 8` is 16-byte aligned on handler entry, as after a `call`.
    let fpstate = (base - xframe::FPSTATE_SIZE as u64) & !63;
    let frame_addr = ((fpstate - xframe::FRAME_SIZE as u64) & !0xf) - 8;
    m.zero(frame_addr, xframe::FRAME_SIZE)?;
    m.zero(fpstate, xframe::FPSTATE_SIZE)?;

    // The handler's return address: the libc restorer (glibc always sets one
    // on x86-64), else the untranslated trampoline the host recognises.
    let ret = if action.flags & sa::RESTORER != 0 && action.restorer != 0 {
        action.restorer
    } else {
        SIGRETURN_TRAMPOLINE
    };
    m.write_u64(frame_addr + xframe::PRETCODE as u64, ret)?;

    let info = frame_addr + xframe::SIGINFO as u64;
    write_siginfo(signals, m, info, signal, fault_address, true)?;

    let uc = frame_addr + xframe::UCONTEXT as u64;
    m.write_u64(uc + xframe::UC_FLAGS as u64, 0)?;
    m.write_u64(uc + xframe::UC_LINK as u64, 0)?;
    m.write_u64(uc + xframe::UC_STACK as u64, signals.alt_stack.sp)?;
    m.write_u32(uc + xframe::UC_STACK as u64 + 8, signals.alt_stack.flags)?;
    m.write_u64(uc + xframe::UC_STACK as u64 + 16, signals.alt_stack.size)?;
    m.write_u64(
        uc + xframe::UC_SIGMASK as u64,
        signals.suspend_restore.take().unwrap_or(signals.blocked),
    )?;

    let mc = uc + xframe::UC_MCONTEXT as u64;
    for (i, reg) in xframe::GPR_ORDER.iter().enumerate() {
        let v = m.read_u64(ctx_addr + ctx::x(*reg) as u64)?;
        m.write_u64(mc + i as u64 * 8, v)?;
    }
    let pc = m.read_u64(ctx_addr + ctx::PC as u64)?;
    m.write_u64(mc + xframe::MC_RIP as u64, pc)?;
    m.write_u64(mc + xframe::MC_EFLAGS as u64, read_rflags(m, ctx_addr)?)?;
    // cs = __USER_CS (0x33), the other selectors zero.
    m.write_u64(mc + xframe::MC_CS as u64, 0x33)?;
    m.write_u64(mc + xframe::MC_TRAPNO as u64, if signal == sig::SIGSEGV { 14 } else { 0 })?;
    m.write_u64(mc + xframe::MC_OLDMASK as u64, signals.blocked)?;
    m.write_u64(mc + xframe::MC_CR2 as u64, fault_address)?;
    m.write_u64(mc + xframe::MC_FPSTATE as u64, fpstate)?;

    // The fxsave image: control words, MXCSR, and the sixteen xmm registers.
    m.write_u32(fpstate, m.read_u32(ctx_addr + ctx::X87_CW as u64)? & 0xffff)?;
    m.write_u32(fpstate + 24, m.read_u32(ctx_addr + ctx::MXCSR as u64)?)?;
    m.write_u32(fpstate + 28, 0xffff)?;
    for n in 0..16u8 {
        for half in 0..2u64 {
            let v = m.read_u64(ctx_addr + ctx::v(n) as u64 + half * 8)?;
            m.write_u64(fpstate + 160 + n as u64 * 16 + half * 8, v)?;
        }
    }

    signals.blocked |= action.mask;
    if action.flags & sa::NODEFER == 0 {
        signals.blocked |= bit(signal);
    }
    signals.pending &= !bit(signal);
    signals.in_handler = true;
    if action.flags & sa::RESETHAND != 0 {
        let _ = signals.set_action(signal, SigAction::default());
    }

    // rsp = frame, rdi = signo, rsi = &info, rdx = &uc, rax = 0.
    m.write_u64(ctx_addr + ctx::SP as u64, frame_addr)?;
    m.write_u64(ctx_addr + ctx::x(7) as u64, signal as u64)?;
    m.write_u64(ctx_addr + ctx::x(6) as u64, info)?;
    m.write_u64(ctx_addr + ctx::x(2) as u64, uc)?;
    m.write_u64(ctx_addr + ctx::x(0) as u64, 0)?;
    m.write_u64(ctx_addr + ctx::PC as u64, action.handler)?;
    Ok(action.handler)
}

/// The x86-64 `rt_sigreturn`. The handler's `ret` popped the return address,
/// so the frame starts one word below the stack pointer.
fn sigreturn_x86(signals: &mut Signals, m: &mut GuestMem<'_>, ctx_addr: u64) -> Result<u64, Errno> {
    let frame_addr = m.read_u64(ctx_addr + ctx::SP as u64)? - 8;
    let uc = frame_addr + xframe::UCONTEXT as u64;
    let mc = uc + xframe::UC_MCONTEXT as u64;
    for (i, reg) in xframe::GPR_ORDER.iter().enumerate() {
        let v = m.read_u64(mc + i as u64 * 8)?;
        m.write_u64(ctx_addr + ctx::x(*reg) as u64, v)?;
    }
    let pc = m.read_u64(mc + xframe::MC_RIP as u64)?;
    m.write_u64(ctx_addr + ctx::PC as u64, pc)?;
    write_rflags(m, ctx_addr, m.read_u64(mc + xframe::MC_EFLAGS as u64)?)?;
    let fpstate = m.read_u64(mc + xframe::MC_FPSTATE as u64)?;
    if fpstate != 0 {
        if let Ok(mxcsr) = m.read_u32(fpstate + 24) {
            m.write_u32(ctx_addr + ctx::MXCSR as u64, mxcsr)?;
            for n in 0..16u8 {
                for half in 0..2u64 {
                    let v = m.read_u64(fpstate + 160 + n as u64 * 16 + half * 8)?;
                    m.write_u64(ctx_addr + ctx::v(n) as u64 + half * 8, v)?;
                }
            }
        }
    }
    signals.blocked = m.read_u64(uc + xframe::UC_SIGMASK as u64)?;
    signals.in_handler = false;
    Ok(pc)
}

/// The i386 signal frames (`arch/x86/ia32/ia32_signal.c`). Every field is a
/// 32-bit word; the frame sits on the 32-bit stack and the handler is entered
/// as if called, with `esp` at the return address.
///
/// `struct sigcontext_ia32` (88 bytes):
///   gs fs es ds (selector words) | edi esi ebp esp ebx edx ecx eax | trapno
///   err | eip cs eflags esp_at_signal ss | fpstate oldmask cr2
/// `struct sigframe_ia32`: pretcode sig sigcontext extramask retcode
/// `struct rt_sigframe_ia32`: pretcode sig pinfo puc siginfo(128) ucontext
///   where ucontext is uc_flags uc_link uc_stack(12) uc_mcontext(88)
///   uc_sigmask(8).
mod iframe {
    pub const SC_GS: u64 = 0;
    pub const SC_FS: u64 = 4;
    pub const SC_ES: u64 = 8;
    pub const SC_DS: u64 = 12;
    /// edi esi ebp esp ebx edx ecx eax, as context register numbers.
    pub const GPR_ORDER: [u8; 8] = [7, 6, 5, 4, 3, 2, 1, 0];
    pub const SC_GPRS: u64 = 16;
    pub const SC_TRAPNO: u64 = 48;
    pub const SC_ERR: u64 = 52;
    pub const SC_EIP: u64 = 56;
    pub const SC_CS: u64 = 60;
    pub const SC_EFLAGS: u64 = 64;
    pub const SC_ESP_AT_SIGNAL: u64 = 68;
    pub const SC_SS: u64 = 72;
    pub const SC_FPSTATE: u64 = 76;
    pub const SC_OLDMASK: u64 = 80;
    pub const SC_CR2: u64 = 84;
    pub const SC_SIZE: u64 = 88;

    // sigframe_ia32
    pub const SF_SIG: u64 = 4;
    pub const SF_SC: u64 = 8;
    pub const SF_EXTRAMASK: u64 = SF_SC + SC_SIZE; // 96
    pub const SF_SIZE: u64 = 112;

    // rt_sigframe_ia32
    pub const RT_SIG: u64 = 4;
    pub const RT_PINFO: u64 = 8;
    pub const RT_PUC: u64 = 12;
    pub const RT_INFO: u64 = 16;
    pub const RT_UC: u64 = 144;
    pub const UC_STACK: u64 = 8;
    pub const UC_MCONTEXT: u64 = 20;
    pub const UC_SIGMASK: u64 = UC_MCONTEXT + SC_SIZE; // 108
    pub const RT_SIZE: u64 = 272;

    /// `struct _fpstate_ia32` with an fxsave image: the legacy words first,
    /// `magic` at 110, then the fxsave environment (mxcsr at 136) and the
    /// xmm registers at 272.
    pub const FPSTATE_SIZE: usize = 624;
    pub const FP_MXCSR: u64 = 136;
    pub const FP_XMM: u64 = 272;

    pub const USER32_CS: u32 = 0x23;
    pub const USER_DS: u32 = 0x2b;
    /// The TLS entry glibc's i386 port loads into %gs: GDT entry 6, RPL 3.
    pub const GS_TLS: u32 = 0x33;
}

fn setup_frame_i386(
    signals: &mut Signals,
    m: &mut GuestMem<'_>,
    ctx_addr: u64,
    signal: u32,
    fault_address: u64,
) -> Result<u64, Errno> {
    let action = signals.action(signal);
    let sp = m.read_u64(ctx_addr + ctx::SP as u64)? & 0xffff_ffff;
    let rt = action.flags & sa::SIGINFO != 0;
    signals.legacy_frame = !rt;

    let on_alt = action.flags & sa::ONSTACK != 0 && signals.alt_stack.size > 0;
    let base = if on_alt {
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

    // The FP image above the frame, then the frame such that `esp + 4` is
    // 16-byte aligned on handler entry, as after a `call`.
    let fpstate = (base - iframe::FPSTATE_SIZE as u64) & !63;
    let size = if rt { iframe::RT_SIZE } else { iframe::SF_SIZE };
    let frame_addr = ((fpstate - size) & !0xf) - 4;
    m.zero(frame_addr, size as usize)?;
    m.zero(fpstate, iframe::FPSTATE_SIZE)?;

    let ret = if action.flags & sa::RESTORER != 0 && action.restorer != 0 {
        action.restorer
    } else {
        SIGRETURN_TRAMPOLINE
    };
    m.write_u32(frame_addr, ret as u32)?;

    let sc = if rt {
        m.write_u32(frame_addr + iframe::RT_SIG, signal)?;
        let info = frame_addr + iframe::RT_INFO;
        let uc = frame_addr + iframe::RT_UC;
        m.write_u32(frame_addr + iframe::RT_PINFO, info as u32)?;
        m.write_u32(frame_addr + iframe::RT_PUC, uc as u32)?;
        write_siginfo(signals, m, info, signal, fault_address, false)?;
        m.write_u32(uc, 0)?;
        m.write_u32(uc + 4, 0)?;
        m.write_u32(uc + iframe::UC_STACK, signals.alt_stack.sp as u32)?;
        m.write_u32(uc + iframe::UC_STACK + 4, signals.alt_stack.flags)?;
        m.write_u32(uc + iframe::UC_STACK + 8, signals.alt_stack.size as u32)?;
        m.write_u64(
            uc + iframe::UC_SIGMASK,
            signals.suspend_restore.take().unwrap_or(signals.blocked),
        )?;
        uc + iframe::UC_MCONTEXT
    } else {
        m.write_u32(frame_addr + iframe::SF_SIG, signal)?;
        let mask = signals.suspend_restore.take().unwrap_or(signals.blocked);
        m.write_u32(frame_addr + iframe::SF_EXTRAMASK, (mask >> 32) as u32)?;
        frame_addr + iframe::SF_SC
    };

    m.write_u32(sc + iframe::SC_GS, iframe::GS_TLS)?;
    m.write_u32(sc + iframe::SC_FS, 0)?;
    m.write_u32(sc + iframe::SC_ES, iframe::USER_DS)?;
    m.write_u32(sc + iframe::SC_DS, iframe::USER_DS)?;
    for (i, reg) in iframe::GPR_ORDER.iter().enumerate() {
        let v = m.read_u64(ctx_addr + ctx::x(*reg) as u64)?;
        m.write_u32(sc + iframe::SC_GPRS + i as u64 * 4, v as u32)?;
    }
    let pc = m.read_u64(ctx_addr + ctx::PC as u64)?;
    m.write_u32(sc + iframe::SC_TRAPNO, if signal == sig::SIGSEGV { 14 } else { 0 })?;
    m.write_u32(sc + iframe::SC_ERR, 0)?;
    m.write_u32(sc + iframe::SC_EIP, pc as u32)?;
    m.write_u32(sc + iframe::SC_CS, iframe::USER32_CS)?;
    m.write_u32(sc + iframe::SC_EFLAGS, read_rflags(m, ctx_addr)? as u32)?;
    m.write_u32(sc + iframe::SC_ESP_AT_SIGNAL, sp as u32)?;
    m.write_u32(sc + iframe::SC_SS, iframe::USER_DS)?;
    m.write_u32(sc + iframe::SC_FPSTATE, fpstate as u32)?;
    m.write_u32(sc + iframe::SC_OLDMASK, signals.blocked as u32)?;
    m.write_u32(sc + iframe::SC_CR2, fault_address as u32)?;

    // The FP image: x87 control word, MXCSR and the eight xmm registers.
    m.write_u32(fpstate, m.read_u32(ctx_addr + ctx::X87_CW as u64)? & 0xffff)?;
    m.write_u32(fpstate + iframe::FP_MXCSR, m.read_u32(ctx_addr + ctx::MXCSR as u64)?)?;
    for n in 0..8u8 {
        for half in 0..2u64 {
            let v = m.read_u64(ctx_addr + ctx::v(n) as u64 + half * 8)?;
            m.write_u64(fpstate + iframe::FP_XMM + n as u64 * 16 + half * 8, v)?;
        }
    }

    signals.blocked |= action.mask;
    if action.flags & sa::NODEFER == 0 {
        signals.blocked |= bit(signal);
    }
    signals.pending &= !bit(signal);
    signals.in_handler = true;
    if action.flags & sa::RESETHAND != 0 {
        let _ = signals.set_action(signal, SigAction::default());
    }

    if std::env::var_os("GEX_SIGTRACE").is_some() {
        eprintln!(
            "[sig] i386 frame for {signal}: rt={rt} frame=0x{frame_addr:x} sc=0x{sc:x} fpstate=0x{fpstate:x} \
             handler=0x{:x} restorer=0x{:x} flags=0x{:x} interrupted pc=0x{pc:x} esp=0x{sp:x}",
            action.handler, action.restorer, action.flags
        );
    }
    // esp = frame; eax = signo; for an rt frame edx = &info and ecx = &uc.
    m.write_u64(ctx_addr + ctx::SP as u64, frame_addr)?;
    m.write_u64(ctx_addr + ctx::x(0) as u64, signal as u64)?;
    if rt {
        m.write_u64(ctx_addr + ctx::x(2) as u64, frame_addr + iframe::RT_INFO)?;
        m.write_u64(ctx_addr + ctx::x(1) as u64, frame_addr + iframe::RT_UC)?;
    } else {
        m.write_u64(ctx_addr + ctx::x(2) as u64, 0)?;
        m.write_u64(ctx_addr + ctx::x(1) as u64, 0)?;
    }
    m.write_u64(ctx_addr + ctx::PC as u64, action.handler)?;
    Ok(action.handler)
}

/// The i386 `sigreturn` (legacy: the restorer popped the return address and
/// `sig`, so the frame is two words below `esp`) and `rt_sigreturn` (one
/// word below).
fn sigreturn_i386(
    signals: &mut Signals,
    m: &mut GuestMem<'_>,
    ctx_addr: u64,
    legacy: bool,
) -> Result<u64, Errno> {
    let esp = m.read_u64(ctx_addr + ctx::SP as u64)? & 0xffff_ffff;
    let (sc, mask) = if legacy {
        let frame_addr = esp - 8;
        let sc = frame_addr + iframe::SF_SC;
        let lo = m.read_u32(sc + iframe::SC_OLDMASK)? as u64;
        let hi = m.read_u32(frame_addr + iframe::SF_EXTRAMASK)? as u64;
        (sc, lo | (hi << 32))
    } else {
        let frame_addr = esp - 4;
        let uc = frame_addr + iframe::RT_UC;
        (uc + iframe::UC_MCONTEXT, m.read_u64(uc + iframe::UC_SIGMASK)?)
    };
    if std::env::var_os("GEX_SIGTRACE").is_some() {
        let words: Vec<String> = (0..8)
            .map(|i| format!("{:08x}", m.read_u32(esp - 8 + i * 4).unwrap_or(0)))
            .collect();
        eprintln!(
            "[sig] i386 sigreturn legacy={legacy} esp=0x{esp:x} sc=0x{sc:x} eip=0x{:x} words@esp-8: {}",
            m.read_u32(sc + iframe::SC_EIP).unwrap_or(0),
            words.join(" ")
        );
    }
    for (i, reg) in iframe::GPR_ORDER.iter().enumerate() {
        let v = m.read_u32(sc + iframe::SC_GPRS + i as u64 * 4)? as u64;
        m.write_u64(ctx_addr + ctx::x(*reg) as u64, v)?;
    }
    let pc = m.read_u32(sc + iframe::SC_EIP)? as u64;
    m.write_u64(ctx_addr + ctx::PC as u64, pc)?;
    write_rflags(m, ctx_addr, m.read_u32(sc + iframe::SC_EFLAGS)? as u64)?;
    let fpstate = m.read_u32(sc + iframe::SC_FPSTATE)? as u64;
    if fpstate != 0 {
        if let Ok(mxcsr) = m.read_u32(fpstate + iframe::FP_MXCSR) {
            m.write_u32(ctx_addr + ctx::MXCSR as u64, mxcsr)?;
            for n in 0..8u8 {
                for half in 0..2u64 {
                    let v = m.read_u64(fpstate + iframe::FP_XMM + n as u64 * 16 + half * 8)?;
                    m.write_u64(ctx_addr + ctx::v(n) as u64 + half * 8, v)?;
                }
            }
        }
    }
    signals.blocked = mask;
    signals.in_handler = false;
    Ok(pc)
}

/// RFLAGS from the context's flag bytes: CF, PF, AF, ZF, SF, DF, OF in their
/// architectural bits, plus the always-set bit 1 and IF.
fn read_rflags(m: &GuestMem<'_>, ctx_addr: u64) -> Result<u64, Errno> {
    let f = |off: usize| -> Result<u64, Errno> {
        Ok(m.read(ctx_addr + off as u64, 1)?[0] as u64 & 1)
    };
    Ok((f(ctx::CF)?)
        | (1 << 1)
        | (f(ctx::PF)? << 2)
        | (f(ctx::AF)? << 4)
        | (f(ctx::ZF)? << 6)
        | (f(ctx::NF)? << 7)
        | (1 << 9)
        | (f(ctx::DF)? << 10)
        | (f(ctx::VF)? << 11)
        | (m.read_u32(ctx_addr + ctx::EFLAGS_SYS as u64)? & EFLAGS_AC_ID) as u64)
}

/// EFLAGS.AC (bit 18) and ID (bit 21): the system bits user code can toggle,
/// kept in `ctx::EFLAGS_SYS`, so a handler sees, and `sigreturn` restores, what
/// the interrupted code's last `popf` set.
const EFLAGS_AC_ID: u32 = 0x24_0000;

fn write_rflags(m: &mut GuestMem<'_>, ctx_addr: u64, v: u64) -> Result<(), Errno> {
    for (off, bit) in [
        (ctx::CF, 0),
        (ctx::PF, 2),
        (ctx::AF, 4),
        (ctx::ZF, 6),
        (ctx::NF, 7),
        (ctx::DF, 10),
        (ctx::VF, 11),
    ] {
        m.write(ctx_addr + off as u64, &[((v >> bit) & 1) as u8])?;
    }
    m.write_u32(ctx_addr + ctx::EFLAGS_SYS as u64, v as u32 & EFLAGS_AC_ID)?;
    Ok(())
}

/// Restore the context a handler frame captured. Returns the resume address.
/// `compat` selects the i386 frames, and `legacy` the non-rt `sigreturn`
/// form of those (a handler installed without `SA_SIGINFO`).
pub fn sigreturn(
    signals: &mut Signals,
    m: &mut GuestMem<'_>,
    ctx_addr: u64,
    compat: bool,
    legacy: bool,
) -> Result<u64, Errno> {
    if gex_abi::PPC {
        return sigreturn_ppc(signals, m, ctx_addr);
    }
    if gex_abi::X86 && compat {
        // The frame's own kind decides; the entry number is a cross-check.
        let _ = legacy;
        let kind = signals.legacy_frame;
        return sigreturn_i386(signals, m, ctx_addr, kind);
    }
    if gex_abi::X86 {
        return sigreturn_x86(signals, m, ctx_addr);
    }
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
    compat: bool,
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
            // The three a terminal raises, with no handler and no scheduler to
            // resume from: doing nothing matches a shell with no job control (a
            // `^Z` there does nothing on real hardware too), and is far better
            // than killing a backgrounded reader. `SIGSTOP` is excluded -- it
            // can't be handled, so ending the process at least does something.
            DefaultAction::Stop
                if matches!(signal, sig::SIGTSTP | 21 | 22) =>
            {
                Delivery::None
            }
            DefaultAction::Core | DefaultAction::Stop => Delivery::Terminate { signal, core: true },
            _ => Delivery::Terminate {
                signal,
                core: false,
            },
        });
    }
    let resume = setup_frame(signals, m, ctx_addr, signal, 0, compat)?;
    Ok(Delivery::Resume(resume))
}

/// The `siginfo_t` head at `info`: signo, code, and then either the sender
/// (`si_pid`, `si_uid`) of a signal a process sent or, for the synchronous
/// faults, the fault address, which is what a crash handler reads. `wide` is
/// the 64-bit layout, where the union after the three ints is 8-aligned (at
/// 16 rather than 12). Takes the signal's [`Source`].
fn write_siginfo(
    signals: &mut Signals,
    m: &mut GuestMem<'_>,
    info: u64,
    signal: u32,
    fault_address: u64,
    wide: bool,
) -> Result<(), Errno> {
    let fields = info + if wide { 16 } else { 12 };
    m.write_u32(info, signal)?;
    if let Some(src) = signals.take_source(signal) {
        m.write_u32(info + 8, src.code as u32)?;
        m.write_u32(fields, src.pid as u32)?;
        m.write_u32(fields + 4, src.uid)?;
        return Ok(());
    }
    m.write_u32(info + 8, fault_code(signal))?;
    if matches!(signal, sig::SIGSEGV | sig::SIGBUS | sig::SIGILL | sig::SIGFPE) {
        if wide {
            m.write_u64(fields, fault_address)?;
        } else {
            m.write_u32(fields, fault_address as u32)?;
        }
    }
    Ok(())
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
    let (n, z, c, v) = ctx::unpack_nzcv(m.read_u32(ctx_addr + ctx::NZCV as u64)?);
    // EL0t with interrupts enabled, plus NZCV in their architectural places.
    Ok(((n as u64) << 31) | ((z as u64) << 30) | ((c as u64) << 29) | ((v as u64) << 28))
}

fn write_pstate(m: &mut GuestMem<'_>, ctx_addr: u64, pstate: u64) -> Result<(), Errno> {
    m.write_u32(
        ctx_addr + ctx::NZCV as u64,
        ctx::pack_nzcv(
            (pstate >> 31) as u32,
            (pstate >> 30) as u32,
            (pstate >> 29) as u32,
            (pstate >> 28) as u32,
        ),
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// PowerPC (32-bit): `arch/powerpc/kernel/signal_32.c`'s rt_sigframe
// ---------------------------------------------------------------------------

/// Offsets of the ppc32 `rt_sigframe`, relative to the frame (docs/ppc.md §7).
mod pframe {
    /// `struct siginfo`, then `struct ucontext`.
    pub const INFO: u64 = 0;
    pub const UC: u64 = 128;
    pub const UC_STACK: u64 = 8;
    /// `uc_regs`: points at `uc_mcontext`.
    pub const UC_REGS: u64 = 48;
    pub const UC_SIGMASK: u64 = 52;
    pub const UC_MCONTEXT: u64 = 192;
    /// Within `mcontext`: 48 gregs, then 33 doubles (f0–f31, FPSCR).
    pub const MC_FREGS: u64 = 48 * 4;
    /// The whole frame: siginfo, ucontext (with the AltiVec area gex never
    /// fills) and the 224-byte ABI gap, rounded to 16.
    pub const SIZE: u64 = 128 + 192 + 1024 + 224;
    /// The caller frame the kernel leaves below it: `__SIGNAL_FRAMESIZE` + 16.
    pub const CALLER: u64 = 64 + 16;
    /// `mc_gregs` indices beyond the GPRs.
    pub const PT_NIP: u64 = 32;
    pub const PT_MSR: u64 = 33;
    pub const PT_ORIG_R3: u64 = 34;
    pub const PT_CTR: u64 = 35;
    pub const PT_LNK: u64 = 36;
    pub const PT_XER: u64 = 37;
    pub const PT_CCR: u64 = 38;
    pub const PT_TRAP: u64 = 40;
    pub const PT_DAR: u64 = 41;
    /// A user-mode MSR: EE, PR, FP, ME, IR, DR, RI.
    pub const USER_MSR: u32 = 0xd032;
}

/// The CR, from its 32 context bytes.
fn read_cr_ppc(m: &GuestMem<'_>, ctx_addr: u64) -> Result<u32, Errno> {
    let b = m.read(ctx_addr + ctx::PPC_CR as u64, 32)?;
    Ok(b.iter().enumerate().fold(0u32, |acc, (i, v)| acc | (((*v & 1) as u32) << (31 - i))))
}

fn write_cr_ppc(m: &mut GuestMem<'_>, ctx_addr: u64, cr: u32) -> Result<(), Errno> {
    let b: Vec<u8> = (0..32).map(|i| ((cr >> (31 - i)) & 1) as u8).collect();
    m.write(ctx_addr + ctx::PPC_CR as u64, &b)
}

fn read_xer_ppc(m: &GuestMem<'_>, ctx_addr: u64) -> Result<u32, Errno> {
    let b = m.read(ctx_addr + ctx::PPC_SO as u64, 4)?;
    Ok(((b[0] as u32 & 1) << 31) | ((b[1] as u32 & 1) << 30) | ((b[2] as u32 & 1) << 29) | (b[3] as u32 & 0x7f))
}

fn write_xer_ppc(m: &mut GuestMem<'_>, ctx_addr: u64, x: u32) -> Result<(), Errno> {
    let b = [(x >> 31) as u8 & 1, (x >> 30) as u8 & 1, (x >> 29) as u8 & 1, (x & 0x7f) as u8];
    m.write(ctx_addr + ctx::PPC_SO as u64, &b)
}

fn setup_frame_ppc(
    signals: &mut Signals,
    m: &mut GuestMem<'_>,
    ctx_addr: u64,
    signal: u32,
    fault_address: u64,
) -> Result<u64, Errno> {
    use pframe::*;
    let action = signals.action(signal);
    let sp = m.read_u64(ctx_addr + ctx::SP as u64)? & 0xffff_ffff;
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
    let frame = (base - SIZE) & !0xf;
    let newsp = frame - CALLER;
    m.zero(newsp, (SIZE + CALLER) as usize)?;

    let info = frame + INFO;
    write_siginfo(signals, m, info, signal, fault_address, false)?;
    let uc = frame + UC;
    m.write_u32(uc + UC_STACK, signals.alt_stack.sp as u32)?;
    m.write_u32(uc + UC_STACK + 4, signals.alt_stack.flags)?;
    m.write_u32(uc + UC_STACK + 8, signals.alt_stack.size as u32)?;
    let mc = uc + UC_MCONTEXT;
    m.write_u32(uc + UC_REGS, mc as u32)?;
    m.write_bits64(uc + UC_SIGMASK, signals.suspend_restore.take().unwrap_or(signals.blocked))?;

    let g = |i: u64| mc + i * 4;
    for n in 0..32u8 {
        let v = m.read_u64(ctx_addr + ctx::x(n) as u64)? as u32;
        m.write_u32(g(n as u64), v)?;
    }
    let pc = m.read_u64(ctx_addr + ctx::PC as u64)?;
    m.write_u32(g(PT_NIP), pc as u32)?;
    m.write_u32(g(PT_MSR), USER_MSR)?;
    m.write_u32(g(PT_ORIG_R3), m.read_u32(ctx_addr + ctx::x(3) as u64)?)?;
    m.write_u32(g(PT_CTR), m.read_u32(ctx_addr + ctx::PPC_CTR as u64)?)?;
    m.write_u32(g(PT_LNK), m.read_u32(ctx_addr + ctx::PPC_LR as u64)?)?;
    m.write_u32(g(PT_XER), read_xer_ppc(m, ctx_addr)?)?;
    m.write_u32(g(PT_CCR), read_cr_ppc(m, ctx_addr)?)?;
    // 0x300 is a data storage interrupt, 0x700 a program check.
    let trap = match signal {
        sig::SIGSEGV | sig::SIGBUS => 0x300,
        sig::SIGILL | sig::SIGTRAP | sig::SIGFPE => 0x700,
        _ => 0,
    };
    m.write_u32(g(PT_TRAP), trap)?;
    m.write_u32(g(PT_DAR), fault_address as u32)?;
    for n in 0..32u8 {
        let v = m.read_u64(ctx_addr + ctx::f(n) as u64)?;
        m.write_u64(mc + MC_FREGS + n as u64 * 8, v)?;
    }
    let fpscr = m.read_u32(ctx_addr + ctx::PPC_FPSCR as u64)? as u64;
    m.write_u64(mc + MC_FREGS + 32 * 8, fpscr)?;
    // The back chain of the handler's caller frame.
    m.write_u32(newsp, sp as u32)?;

    signals.blocked |= action.mask;
    if action.flags & sa::NODEFER == 0 {
        signals.blocked |= bit(signal);
    }
    signals.pending &= !bit(signal);
    signals.in_handler = true;
    if action.flags & sa::RESETHAND != 0 {
        let _ = signals.set_action(signal, SigAction::default());
    }
    if std::env::var_os("GEX_SIGTRACE").is_some() {
        eprintln!(
            "[sig] ppc frame for {signal}: frame=0x{frame:x} sp=0x{newsp:x} handler=0x{:x} interrupted pc=0x{pc:x}",
            action.handler
        );
    }
    // r1 = the caller frame, r3 = signo, r4 = &info, r5 = &uc, r6 = the
    // frame; the handler returns through LR to the host's trampoline.
    for (n, v) in [(1u8, newsp), (3, signal as u64), (4, info), (5, uc), (6, frame)] {
        m.write_u64(ctx_addr + ctx::x(n) as u64, v & 0xffff_ffff)?;
    }
    m.write_u32(ctx_addr + ctx::PPC_LR as u64, SIGRETURN_TRAMPOLINE as u32)?;
    m.write_u64(ctx_addr + ctx::PC as u64, action.handler)?;
    Ok(action.handler)
}

/// `rt_sigreturn`: the frame is above the handler's r1 by the caller frame
/// the kernel left, and its `uc_regs` names the registers to restore (a
/// handler may point it elsewhere, as `swapcontext`-style code does).
fn sigreturn_ppc(signals: &mut Signals, m: &mut GuestMem<'_>, ctx_addr: u64) -> Result<u64, Errno> {
    use pframe::*;
    let sp = m.read_u64(ctx_addr + ctx::SP as u64)? & 0xffff_ffff;
    let frame = sp + CALLER;
    let uc = frame + UC;
    let mask = m.read_bits64(uc + UC_SIGMASK)?;
    let mc = m.read_u32(uc + UC_REGS)? as u64;
    let g = |i: u64| mc + i * 4;
    for n in 0..32u8 {
        let v = m.read_u32(g(n as u64))? as u64;
        m.write_u64(ctx_addr + ctx::x(n) as u64, v)?;
    }
    let pc = m.read_u32(g(PT_NIP))? as u64;
    m.write_u64(ctx_addr + ctx::PC as u64, pc)?;
    m.write_u32(ctx_addr + ctx::PPC_CTR as u64, m.read_u32(g(PT_CTR))?)?;
    m.write_u32(ctx_addr + ctx::PPC_LR as u64, m.read_u32(g(PT_LNK))?)?;
    write_xer_ppc(m, ctx_addr, m.read_u32(g(PT_XER))?)?;
    write_cr_ppc(m, ctx_addr, m.read_u32(g(PT_CCR))?)?;
    for n in 0..32u8 {
        let v = m.read_u64(mc + MC_FREGS + n as u64 * 8)?;
        m.write_u64(ctx_addr + ctx::f(n) as u64, v)?;
    }
    let fpscr = m.read_u64(mc + MC_FREGS + 32 * 8)? as u32;
    m.write_u32(ctx_addr + ctx::PPC_FPSCR as u64, fpscr)?;
    signals.blocked = mask;
    signals.in_handler = false;
    Ok(pc)
}

#[cfg(all(test, not(any(feature = "x86", feature = "ppc"))))]
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
                m.write_u64(CTX + ctx::x(i) as u64, 0x1000 + i as u64)
                    .unwrap();
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
            dispatch(&mut s, &mut m, CTX, false).unwrap(),
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
        assert_eq!(dispatch(&mut s, &mut m, CTX, false).unwrap(), Delivery::None);
        assert!(!s.has_pending());
    }

    #[test]
    fn sigchld_defaults_to_ignored() {
        let (mut buf, mut s) = setup();
        let mut m = GuestMem::new(&mut buf);
        s.raise(sig::SIGCHLD);
        assert_eq!(dispatch(&mut s, &mut m, CTX, false).unwrap(), Delivery::None);
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

        let d = dispatch(&mut s, &mut m, CTX, false).unwrap();
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
        let resume = sigreturn(&mut s, &mut m, CTX, false, false).unwrap();
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
        m.write_u32(CTX + ctx::NZCV as u64, ctx::pack_nzcv(1, 0, 1, 0))
            .unwrap();
        s.set_action(
            sig::SIGUSR1,
            SigAction {
                handler: 0x400500,
                ..Default::default()
            },
        )
        .unwrap();
        s.raise(sig::SIGUSR1);
        dispatch(&mut s, &mut m, CTX, false).unwrap();
        // The handler runs with whatever flags it likes.
        m.write_u32(CTX + ctx::NZCV as u64, ctx::pack_nzcv(0, 1, 0, 0))
            .unwrap();
        sigreturn(&mut s, &mut m, CTX, false, false).unwrap();
        let (n, z, c, _) = ctx::unpack_nzcv(m.read_u32(CTX + ctx::NZCV as u64).unwrap());
        assert_eq!((n, z, c), (1, 0, 1));
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
        dispatch(&mut s, &mut m, CTX, false).unwrap();
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
        dispatch(&mut s, &mut m, CTX, false).unwrap();
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
            dispatch(&mut s, &mut m, CTX, false).unwrap(),
            Delivery::Resume(0x400000 + sig::SIGUSR1 as u64)
        );
    }
}
