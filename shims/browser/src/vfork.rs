//! A `posix_spawn` child in its parent's Worker.
//!
//! glibc's and musl's `posix_spawn` clone with `CLONE_VM | CLONE_VFORK`: the
//! child shares the parent's memory and runs on a stack the parent set
//! aside, and the calling thread is suspended until the child execs or
//! exits; the parent's other threads run on. The child only applies its
//! file actions and attributes and execs (or, when the exec fails, writes
//! the error to a pipe and exits). Under gex every other child gets a Worker
//! of its own with a copy of the parent's image; this one needs neither the
//! copy nor the Worker until its exec, and the exec needs none of the
//! parent's image.
//!
//! So the child runs in the calling thread's Worker, in the parent's memory
//! as Linux runs it (nothing of the memory is saved or put back: what the
//! child writes, the parent sees, as with `CLONE_VM`). A wasm32 program's
//! child runs nested inside the parent's `clone` (`#vforkChild` in
//! gex.mjs); a translated program's runs in the Worker's run loop in place
//! of the parent's thread (`#vforkEnter`), which resumes after it:
//!
//!   1. `gex_vfork_begin` claims the fork and gives this Worker the child's
//!      process: a `Host` built as a forked child's Worker builds one
//!      (`child_host`: its pid, descriptors, working directory, signal
//!      state, one thread), with the parent's memory map and translation
//!      state (the process's one `JIT`). `with_host` serves this Worker that host
//!      while the child runs; what is read without the kernel (the pid, the
//!      exit flag, the descriptor table, the signal state) is this Worker's
//!      too. The parent's process stays in place for its other Workers.
//!      The calling thread's register context, which the child's start and
//!      calls write, is kept aside;
//!   2. the child's calls go to its own kernel, except those that would
//!      change the memory map the parent shares (`check`): those end it as
//!      a crash, which `posix_spawn`'s child never makes;
//!   3. the child ends: it exits (published as any exit), or it execs
//!      (`stage_exec`: its command, descriptors, signal state and working
//!      directory, which JS hands to a new Worker), or it crashes;
//!   4. `gex_vfork_end` gives the Worker back to the parent's thread, with
//!      its register context, and the parent's `clone` returns the child's
//!      pid.
//!
//! A plain `fork` (and `vfork()`, which is `fork`) keeps the copy and the
//! Worker of its own: its child runs the parent's code on the parent's stack
//! and may do anything.

use super::*;
use std::cell::Cell;
use std::sync::atomic::Ordering::{Acquire, Relaxed};

/// Why the child's run ended without it exiting: [`EXEC`] or
/// [`UNSUPPORTED`].
const EXEC: u32 = 2;
const UNSUPPORTED: u32 = 3;

thread_local! {
    /// The last syscall this Worker served was a `clone` with
    /// `CLONE_VM | CLONE_VFORK`.
    static STAGED: Cell<bool> = const { Cell::new(false) };
    /// The pid of the vfork child running in this Worker; 0 for none. Set
    /// from before its kernel is built until after it is dropped.
    static PID: Cell<i32> = const { Cell::new(0) };
    /// Its `Host`, which `with_host` serves this Worker while it runs.
    static CHILD: Cell<*mut Host> = const { Cell::new(std::ptr::null_mut()) };
    /// Its descriptor table, for what reads one without the kernel.
    static FDS: Cell<*const gex_kernel::file::FdTable> = const { Cell::new(std::ptr::null()) };
    /// Whether it has exited ([`crate::exited`]).
    static EXITED: Cell<bool> = const { Cell::new(false) };
    /// Why its run ended: 0 (it exited, or crashed), [`EXEC`] or
    /// [`UNSUPPORTED`].
    static WHY: Cell<u32> = const { Cell::new(0) };
    /// What the child's run takes from the parent's thread, put back by
    /// `gex_vfork_end`.
    static SAVED: RefCell<Option<Saved>> = const { RefCell::new(None) };
    /// What a child that execs hands the Worker it goes on in: the command,
    /// its descriptors (as `stage_fork_fds` describes them, for this pid),
    /// its working directory and its signal state, pending signals included.
    static EXEC_PARTS: RefCell<[Vec<u8>; 4]> = const { RefCell::new([Vec::new(), Vec::new(), Vec::new(), Vec::new()]) };
}

struct Saved {
    /// The calling thread's register context, as its `clone` left it.
    ctx: Vec<u8>,
    ctx_addr: u64,
    /// Its index in the parent's thread table.
    cur: usize,
    /// Where it goes on: after the `clone`.
    resume: u64,
}

/// Whether a vfork child is running in this Worker.
pub(crate) fn active() -> bool {
    PID.with(|c| c.get()) != 0
}

/// The running child's pid, from before its kernel is built: what this
/// Worker's process is, for what asks without the kernel.
pub(crate) fn pid_here() -> Option<i32> {
    let pid = PID.with(|c| c.get());
    (pid != 0).then_some(pid)
}

/// The child's `Host`, while it runs here (`with_host`).
pub(crate) fn host_here() -> Option<*mut Host> {
    let p = CHILD.with(|c| c.get());
    (!p.is_null()).then_some(p)
}

/// The child's descriptor table, while it runs here (`fds_free`).
pub(crate) fn fds_here() -> Option<&'static gex_kernel::file::FdTable> {
    let p = FDS.with(|c| c.get());
    // SAFETY: inside the child's boxed `Host`, which outlives every call it
    // makes; cleared before the host is dropped.
    (!p.is_null()).then(|| unsafe { &*p })
}

pub(crate) fn set_fds_here(t: &gex_kernel::file::FdTable) {
    FDS.with(|c| c.set(t));
}

/// Whether the child has exited, while one runs here ([`crate::exited`]).
pub(crate) fn exited_here() -> Option<bool> {
    active().then(|| EXITED.with(|c| c.get()))
}

/// Record the child's exit, if one runs here: whether it did.
pub(crate) fn set_exited_here(v: bool) -> bool {
    if !active() {
        return false;
    }
    EXITED.with(|c| c.set(v));
    true
}

/// Note whether the `clone` being served is a vfork.
pub(crate) fn note_clone(vfork: bool) {
    STAGED.with(|c| c.set(vfork));
}

/// Whether the fork the last syscall staged is a vfork child to run here
/// (1; why not, negative): a `CLONE_VM | CLONE_VFORK` clone, from no
/// provider's frames, not a Windows process, with a global kernel to
/// publish the child's exit in.
#[no_mangle]
pub extern "C" fn gex_vfork_ok() -> i32 {
    if active() {
        -1
    } else if !STAGED.with(|c| c.get()) {
        -2
    } else if IN_PROVIDER.with(|c| c.get()) || cgen::provider_callout_open() {
        -4
    } else if nt::process().is_some() {
        -5
    } else if unsafe { k_present() } == 0 {
        -6
    } else if FORK[0].load(Acquire) == 0 || FORK_BY.load(Relaxed) != cur() as u64 + 1 {
        -7
    } else {
        1
    }
}

/// Make this Worker's process the staged vfork child, the calling thread's
/// state kept aside. The child's entry pc, or 0 with nothing to run.
#[no_mangle]
pub extern "C" fn gex_vfork_begin() -> i64 {
    STAGED.with(|c| c.set(false));
    // Where the calling thread goes on, which the claim below does not
    // change and the child's run does.
    // (None for a wasm32 program's, whose `clone` returns by itself.)
    let resume = if frames_stay() { 0 } else { resume().unwrap_or(0) };
    let pid = gex_fork_pending();
    if pid == 0 {
        return 0;
    }
    let [pc, ctx_addr, sp, tls, brk] = CLAIMED_FORK.with(|c| c.get());
    let ctx = {
        let gm = GuestMem::new(guest_bytes());
        gm.read(ctx_addr, ctx::SIZE).map(<[u8]>::to_vec).unwrap_or_default()
    };
    let (compat, started, wasm32, mem) =
        with_host(|h| (h.kernel.compat, h.kernel.started, h.kernel.wasm32, h.kernel.mem.clone()));
    // What `adoptProcess` stages before `gex_adopt_child`, from what
    // `do_fork` staged.
    *CWD.0.borrow_mut() = FORK_CWD.0.borrow().clone();
    *CHILD_EXE.0.borrow_mut() = FORK_EXE.0.borrow().clone();
    *CHILD_STATE.0.borrow_mut() = FORK_STATE.0.borrow().clone();
    let fds = FORK_FDS.0.borrow().clone();
    SAVED.with(|s| *s.borrow_mut() = Some(Saved { ctx, ctx_addr, cur: cur(), resume }));
    // From here this Worker's process is the child: what `child_host` says
    // about "this process" without the kernel goes to this Worker alone.
    PID.with(|c| c.set(pid));
    EXITED.with(|c| c.set(false));
    WHY.with(|c| c.set(0));
    let mut child = Box::new(super::child_host(pid, ctx_addr as i64, pc as i64, sp as i64, tls as i64, brk as i64));
    // The memory and its map are the parent's (the child may not change
    // the map: `check`); so are the code in it and what translated it (`JIT`).
    child.kernel.mem = mem;
    child.kernel.wasm32 = wasm32;
    child.kernel.compat = compat;
    child.kernel.started = started;
    CHILD.with(|c| c.set(&mut *child));
    std::mem::forget(child);
    CURRENT.with(|c| c.set(0));
    adopt_fds_text(&fds);
    with_host(|h| {
        // As `gex_set_compat` does for an adopted child: on an AArch64
        // build an i386-ABI process's stack is `esp`, where `__clone`'s
        // child stack has to be.
        if h.kernel.compat && !gex_abi::X86 && !gex_abi::PPC {
            let mut gm = GuestMem::new(guest_bytes());
            let c = h.kernel.ctx_addr;
            let sp = gm.read_u64(c + ctx::SP as u64).unwrap_or(0);
            if sp != 0 {
                let _ = gm.write_u64(c + ctx::sp_slot(true) as u64, sp);
                let _ = gm.write_u64(c + ctx::SP as u64, 0);
            }
        }
        publish_vfork_child(h);
    });
    // The getpid family's answers are the parent's until the child's first
    // full call: none until then.
    IDS.with(|c| c.set(gex_kernel::syscalls::Idents { pid: 0, ppid: 0, tid: 0, uid: 0, gid: 0 }));
    set_resume_none();
    set_resume_ctx_none();
    pc as i64
}

/// The child's syscall `nr` (canonical), before it is served: the memory
/// map is the parent's too, so a call that would change it, and a further
/// clone, end the child (as a crash: `posix_spawn`'s child makes none).
pub(crate) fn check(nr: u64) {
    use gex_kernel::syscalls::nr::*;
    let name = match nr {
        MMAP => "mmap",
        MUNMAP => "munmap",
        MREMAP => "mremap",
        MPROTECT => "mprotect",
        BRK => "brk",
        CLONE => "clone",
        CLONE3 => "clone3",
        _ => return,
    };
    warn_line(&format!(
        "[gex] pid {}: a vfork child called {name}, which would change its parent's memory map; ended",
        PID.with(|c| c.get())
    ));
    WHY.with(|c| c.set(UNSUPPORTED));
    set_resume_none();
    set_resume_ctx_none();
    trap();
}

/// The child's `execve`, which the kernel has checked and resolved: staged
/// for a new Worker instead of done here, where the memory is the parent's.
/// This kernel's references to the child's descriptors are closed -- the
/// staging took references of the new Worker's -- and the run ends (the
/// caller returns 1).
pub(crate) fn stage_exec(h: &mut Host, path: &str, argv: &[String], envp: &[String]) {
    let pid = h.kernel.pid;
    // The child's own `/proc/self/exe` (its parent's program) means
    // nothing in the Worker the exec goes on in: named by its file.
    let own = [String::from("/proc/self/exe"), format!("/proc/{pid}/exe")];
    let path = if own.iter().any(|p| p == path) && !h.kernel.exe_path.is_empty() {
        h.kernel.exe_path.as_str()
    } else {
        path
    };
    let path = path.to_string();
    let path = path.as_str();
    let mut cmd = Vec::new();
    for s in std::iter::once(path).chain(std::iter::once("")).chain(argv.iter().map(String::as_str)) {
        cmd.extend_from_slice(s.as_bytes());
        cmd.push(0);
    }
    cmd.push(0);
    for s in envp {
        cmd.extend_from_slice(s.as_bytes());
        cmd.push(0);
    }
    stage_fork_fds(h, pid);
    let fds = std::mem::take(&mut *FORK_FDS.0.borrow_mut()).into_bytes();
    let cwd = h.kernel.caller_cwd().into_bytes();
    let state = state_bytes(&h.kernel, true);
    EXEC_PARTS.with(|p| *p.borrow_mut() = [cmd, fds, cwd, state]);
    h.kernel.fds.close_all();
    WHY.with(|c| c.set(EXEC));
    set_resume_none();
    set_resume_ctx_none();
}

/// Why the child's run ended: 0 (it exited, or crashed), 2 an exec staged,
/// 3 a call it may not make here.
#[no_mangle]
pub extern "C" fn gex_vfork_why() -> u32 {
    WHY.with(|c| c.get())
}

/// Whether the child running here is done: it exited, or its run ended
/// (`gex_vfork_why`). A translated child's run loop asks after each trap.
#[no_mangle]
pub extern "C" fn gex_vfork_over() -> i32 {
    (active() && (EXITED.with(|c| c.get()) || WHY.with(|c| c.get()) != 0)) as i32
}

/// The running vfork child's pid, or 0.
#[no_mangle]
pub extern "C" fn gex_vfork_pid() -> i32 {
    PID.with(|c| c.get())
}

/// Part `i` of a staged exec (see `EXEC_PARTS`): where, and how long.
#[no_mangle]
pub extern "C" fn gex_vfork_exec_ptr(i: u32) -> u32 {
    EXEC_PARTS.with(|p| p.borrow().get(i as usize).map_or(0, |v| v.as_ptr() as u32))
}

#[no_mangle]
pub extern "C" fn gex_vfork_exec_len(i: u32) -> u32 {
    EXEC_PARTS.with(|p| p.borrow().get(i as usize).map_or(0, |v| v.len() as u32))
}

/// The staged exec's parts, no longer needed once JS has copied them.
#[no_mangle]
pub extern "C" fn gex_vfork_exec_done() {
    EXEC_PARTS.with(|p| *p.borrow_mut() = Default::default());
}

/// The child is done: the Worker back to the parent's process and its
/// calling thread, with that thread's register context, the child's kernel
/// dropped (its descriptors were closed by its exit or its exec; a crashed
/// child's close here). The parent's `clone` then returns the child's pid,
/// at the pc this returns (0 for none: a wasm32 program's `clone` returns
/// by itself).
#[no_mangle]
pub extern "C" fn gex_vfork_end() -> i64 {
    let Some(Saved { ctx, ctx_addr, cur, resume }) = SAVED.with(|s| s.borrow_mut().take()) else {
        return 0;
    };
    let child = CHILD.with(|c| c.replace(std::ptr::null_mut()));
    FDS.with(|c| c.set(std::ptr::null()));
    sigs::publish_here(None);
    if !child.is_null() {
        // SAFETY: boxed in `gex_vfork_begin` and leaked there; nothing else
        // holds it now that `CHILD` is clear. Its memory map is a copy of
        // the parent's, which owns nothing, and its translation state is
        // borrowed; its descriptors are gone or go now.
        drop(unsafe { Box::from_raw(child) });
    }
    PID.with(|c| c.set(0));
    EXITED.with(|c| c.set(false));
    WHY.with(|c| c.set(0));
    if !ctx.is_empty() {
        let mut gm = GuestMem::new(guest_bytes());
        let _ = gm.write(ctx_addr, &ctx);
    }
    CURRENT.with(|c| c.set(cur));
    IDS.with(|c| c.set(gex_kernel::syscalls::Idents { pid: 0, ppid: 0, tid: 0, uid: 0, gid: 0 }));
    with_host(|h| bind_mine(h, cur));
    // The caller goes on at the returned pc itself. Left in `RESUME`, that
    // pc was still there for the thread's next call that trapped without
    // setting one (a `read` the child's pipe kept waiting), and the thread
    // went back into `clone`'s return with that call's registers: rax 0,
    // the child's path, on the parent's stack.
    set_resume_none();
    set_resume_ctx_none();
    ctx_addr_set(ctx_addr);
    resume as i64
}

/// Run an exec a vfork child staged (`stage_exec`), in the fresh Worker the
/// host started for it, whose kernel `gex_adopt_child` has built and whose
/// descriptors and signal state are adopted. The entry, or 0 when the
/// program cannot be loaded -- and then the process has exited 127, as
/// `posix_spawn`'s child whose exec failed would.
#[no_mangle]
pub extern "C" fn gex_adopted_exec(ptr: u32, len: u32) -> u64 {
    let blob = shim_slice(ptr, len).to_vec();
    let mut parts = blob.split(|b| *b == 0).map(|s| String::from_utf8_lossy(s).into_owned());
    let path = parts.next().unwrap_or_default();
    let _ = parts.next();
    let mut argv = Vec::new();
    for s in parts.by_ref() {
        if s.is_empty() {
            break;
        }
        argv.push(s);
    }
    let envp: Vec<String> = parts.filter(|s| !s.is_empty()).collect();
    // This Worker's memory is new (host.mjs gives an exec child no recycled
    // one), so the exec need not clear what no program wrote.
    EXEC_FRESH.with(|c| c.set(true));
    with_host(|h| {
        let entry = do_exec(h, &path, &argv, &envp);
        if entry == 0 {
            warn_line(&format!("gex: {path}: the program could not be loaded\n"));
            exit_process(h, 127);
            return 0;
        }
        entry
    })
}
