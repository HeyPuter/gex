//! The NT personality's native host side (docs/nt-layer.md §2.3, §10).
//!
//! A Windows program runs through the same machinery as a Linux one
//! ([`super::run_program_with`] with an empty image and the runtime
//! translator on), with three differences, all here:
//! - [`boot`] replaces `place_image`: `gex_kernel::nt::boot` maps the program
//!   and Wine's ntdll and sets the main thread up to enter
//!   `LdrInitializeThunk`;
//! - [`syscall_import`] replaces the body of the `syscall` import: every
//!   `syscall` of an NT process is an NT call, dispatched by
//!   `gex_kernel::nt::syscall` outside the kernel lock;
//! - a ticker keeps `KUSER_SHARED_DATA`'s clocks moving.

use super::*;
use gex_kernel::nt;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// What `gexc run` decided about a Windows program.
#[derive(Clone, Debug)]
pub struct NtSetup {
    pub abi: nt::Abi,
    /// The program's guest path.
    pub exe_path: String,
    /// Arguments after argv[0].
    pub args: Vec<String>,
    pub cfg: nt::Config,
}

fn mono_ns() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

fn wall() -> (u64, u32) {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs(), d.subsec_nanos())
}

pub fn clocks() -> nt::Clocks {
    nt::Clocks { mono_ns, wall }
}

/// Run a Windows program to completion.
pub fn run_nt_program(opts: &RunOptions, xlate: &Options, setup: &NtSetup) -> Result<(Exit, Stats)> {
    // Nothing is translated ahead of time: Wine's loader maps every image
    // itself, so all code arrives through the runtime translator.
    let program = Program { exe: gex_ld::elf::headers_only(0, 0, 0, 0), interp: None };
    let mut opts = opts.clone();
    opts.jit = true;
    run_program_with(&program, &opts, xlate, None, Some(setup))
}

/// Build the NT process in place of `place_image`. Returns the pc the main
/// thread starts at (its registers are already in context slot 0).
pub(crate) fn boot(store: &mut Store<HostState>, memory: gex_mem::GuestMemory, setup: &NtSetup) -> Result<u64> {
    let (futexes, pid) = store.data_mut().with_kernel(|k| (Arc::clone(&k.futexes), k.pid));
    let p = Arc::new(nt::Process::new(setup.abi, nt::thread::nt_pid(pid), setup.cfg.clone(), clocks(), futexes));
    let req = nt::boot::BootRequest { exe_path: &setup.exe_path, args: &setup.args };
    let pc = store
        .data_mut()
        .with_kernel(|k| nt::boot::boot(&p, k, memory.bytes(), &req))
        .map_err(|e| anyhow!("starting {}: {e}", setup.exe_path))?;
    store.data_mut().nt = Some(Arc::clone(&p));
    start_ticker(memory, Arc::downgrade(&p));
    Ok(pc)
}

/// Advance `KUSER_SHARED_DATA`'s clocks every millisecond for as long as the
/// process exists, so a guest polling `GetTickCount` without syscalls sees
/// time pass (docs/nt-layer.md §4).
fn start_ticker(memory: gex_mem::GuestMemory, p: std::sync::Weak<nt::Process>) {
    let _ = std::thread::Builder::new().name("gex-nt-usd".into()).spawn(move || loop {
        match p.upgrade() {
            Some(p) if !p.terminating.load(std::sync::atomic::Ordering::Relaxed) => {
                nt::sysinfo::usd_tick(memory.bytes(), &p.clocks);
            }
            _ => return,
        }
        std::thread::sleep(Duration::from_millis(1));
    });
}

/// The `syscall` import for an NT process.
pub(crate) fn syscall_import(caller: &mut Caller<'_, HostState>, pc: u64) -> Result<()> {
    catch_up_in_caller(caller)?;
    let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let state = caller.data_mut();
    if state.spawn.as_ref().is_some_and(|s| s.stopping.load(std::sync::atomic::Ordering::Acquire)) {
        state.exit = Some(Exit::Status(0));
        return Err(StopExecution.into());
    }
    let p = Arc::clone(state.nt.as_ref().expect("an NT syscall in a Linux process"));
    let kernel = Arc::clone(&state.kernel);
    let ctx_addr = state.thread.ctx_addr;
    let outcome = nt::syscall(&p, &kernel, memory.bytes(), ctx_addr, pc);
    // Code that went away (an unmapped image, a protection change) loses its
    // translations. NT images are never statically recompiled, so newly
    // mapped code needs nothing here.
    let (retired, _mapped) = caller
        .data_mut()
        .with_kernel(|k| (std::mem::take(&mut k.retired_code), std::mem::take(&mut k.new_code)));
    if !retired.is_empty() {
        retire_translations(caller, &retired)?;
    }
    let state = caller.data_mut();
    let resume_pc = || read_u64(memory.bytes(), (ctx_addr + ctx::PC as u64) as usize);
    match outcome {
        nt::Outcome::Continue => Ok(()),
        nt::Outcome::Resume(target) => {
            state.resume_at = Some(target);
            Err(StopExecution.into())
        }
        nt::Outcome::Exit(code) => {
            state.exit = Some(Exit::Status(code as i32));
            Err(StopExecution.into())
        }
        nt::Outcome::ThreadExit => {
            // The main thread's host thread is the process's: when it ends
            // first (ExitThread, or a kill from another thread's
            // ExitProcess), it waits for the process to end and reports the
            // process's exit code, as Windows does.
            if ctx_addr == gex_abi::ctx_addr(0) {
                while !p.terminating.load(std::sync::atomic::Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                state.exit = Some(Exit::Status(p.exit_code.load(std::sync::atomic::Ordering::Acquire) as i32));
            } else {
                state.exit = Some(Exit::Status(0));
            }
            Err(StopExecution.into())
        }
        nt::Outcome::SpawnThread { ctx_addr: child, gex_tid } => {
            // Started by the run loop between calls, as a Linux clone is.
            state.pending_spawn = Some(ThreadState { ctx_addr: child, tid: gex_tid, clear_child_tid: 0 });
            state.resume_at = Some(resume_pc());
            Err(StopExecution.into())
        }
        nt::Outcome::CallbackReturn => {
            bail!("gex-nt: NtCallbackReturn, but the nested guest loop for user-mode callbacks is not built yet (N4)")
        }
    }
}

/// A processor exception in an NT process's translated code.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Fault {
    /// `int3`; the pc is the instruction's own address.
    Breakpoint,
    /// An instruction gex does not implement (`ud2` included).
    Illegal,
    /// A branch to memory that is not executable.
    NotExecutable,
}

/// Deliver `fault` at `pc` to the guest as an NT exception, as Wine's signal
/// handlers do. Returns the StopExecution that redirects the thread into
/// `KiUserExceptionDispatcher`.
pub(crate) fn fault(caller: &mut Caller<'_, HostState>, fault: Fault, pc: u64) -> Result<()> {
    use nt::st;
    let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let state = caller.data_mut();
    let p = Arc::clone(state.nt.as_ref().expect("an NT fault in a Linux process"));
    let (code, rip, info): (u32, u64, Vec<u64>) = match fault {
        // The context's rip is after the int3, as hardware leaves it.
        Fault::Breakpoint => (st::BREAKPOINT, pc + 1, vec![0]),
        Fault::Illegal => (st::ILLEGAL_INSTRUCTION, pc, vec![]),
        // EXCEPTION_EXECUTE_FAULT at the target.
        Fault::NotExecutable => (st::ACCESS_VIOLATION, pc, vec![8, pc]),
    };
    let outcome = nt::hardware_exception(&p, &state.kernel, memory.bytes(), state.thread.ctx_addr, code, rip, &info);
    match outcome {
        nt::Outcome::Resume(target) => state.resume_at = Some(target),
        nt::Outcome::Exit(code) => state.exit = Some(Exit::Status(code as i32)),
        other => bail!("gex-nt: unexpected outcome {other:?} from exception dispatch"),
    }
    Err(StopExecution.into())
}
