//! A `posix_spawn` child in its parent's Worker.
//!
//! musl's `posix_spawn` clones with `CLONE_VM | CLONE_VFORK`: the child
//! shares the parent's memory, runs on a stack the parent set aside, and
//! the parent is suspended until the child execs or exits. The child only
//! applies its file actions and attributes and execs (or, when the exec
//! fails, writes the error to a pipe and exits). Under gex every other
//! child gets a Worker of its own with a copy of the parent's image; this
//! one needs neither the copy nor the Worker until its exec, and the exec
//! needs none of the parent's image.
//!
//! So a single-threaded wasm32 program's vfork child runs here, nested
//! inside the parent's `clone`, in the parent's memory as Linux would run
//! it (nothing of the memory is saved or put back: what the child writes,
//! the parent sees, as with `CLONE_VM`):
//!
//!   1. `gex_vfork_begin` claims the fork and swaps the child's process
//!      state into this Worker's `Host`: a kernel built as a forked child's
//!      Worker builds one (`child_host`: its pid, descriptors, working
//!      directory, signal state), its one thread. The parent's are kept
//!      aside, as is its register context, which the child's start and
//!      calls write;
//!   2. JS enters the child at its thread entry. Its calls go to its own
//!      kernel, except those that would change the memory map the parent
//!      shares (`check`): those end it as a crash, which `posix_spawn`'s
//!      child never makes;
//!   3. the child's frames end: it exits (published as any exit), or it
//!      execs (`stage_exec`: its command, descriptors, signal state and
//!      working directory, which JS hands to a new Worker), or it crashes;
//!   4. `gex_vfork_end` puts the parent's process state and register
//!      context back, and the parent's `clone` returns the child's pid.
//!
//! A plain `fork` (and musl's wasm32 `vfork`, which is `fork`) keeps the
//! copy and the Worker of its own: its child runs the parent's code on the
//! parent's stack and may do anything.

use super::*;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering::{Acquire, Relaxed, Release}};

/// The pid of the vfork child running in this Worker; 0 for none.
static ACTIVE: AtomicI32 = AtomicI32::new(0);
/// Why the child's frames ended without it exiting: [`EXEC`] or
/// [`UNSUPPORTED`].
static WHY: AtomicU32 = AtomicU32::new(0);
const EXEC: u32 = 2;
const UNSUPPORTED: u32 = 3;

thread_local! {
    /// The last syscall this Worker served was a `clone` with
    /// `CLONE_VM | CLONE_VFORK`, from a wasm32 program.
    static STAGED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What the child's run takes from the parent, put back by `gex_vfork_end`.
struct Saved {
    /// A `Host` holding the parent's kernel, threads and exit status (the
    /// rest of it is the child's, unused).
    parent: Host,
    /// The parent's register context, as its `clone` left it.
    ctx: Vec<u8>,
    ctx_addr: u64,
    cur: usize,
    exited: bool,
}

static SAVED: SingleThreaded<Option<Saved>> = SingleThreaded(const { RefCell::new(None) });

/// Whether a vfork child is running.
pub(crate) fn active() -> bool {
    ACTIVE.load(Relaxed) != 0
}

/// Note whether the `clone` being served is a vfork (its flags, as the
/// canonical call takes them).
pub(crate) fn note_clone(flags: u64) {
    const CLONE_VM: u64 = 0x100;
    const CLONE_VFORK: u64 = 0x4000;
    STAGED.with(|c| c.set(flags & (CLONE_VM | CLONE_VFORK) == CLONE_VM | CLONE_VFORK));
}

/// Whether the fork the last syscall staged is a vfork child to run here
/// (1; why not, negative): a wasm32 program's `CLONE_VM | CLONE_VFORK`
/// clone, with one thread, not a Windows process, with a global kernel to
/// publish the child's exit in.
#[no_mangle]
pub extern "C" fn gex_vfork_ok() -> i32 {
    let why = if active() {
        -1
    } else if !STAGED.with(|c| c.get()) {
        -2
    } else if !WASM32.load(Acquire) {
        -3
    } else if IN_PROVIDER.with(|c| c.get()) || cgen::provider_callout_open() {
        -4
    } else if nt::process().is_some() || nt::windows_space() != 0 {
        -5
    } else if unsafe { k_present() } == 0 {
        -6
    } else if FORK[0].load(Acquire) == 0 || FORK_BY.load(Relaxed) != cur() as u64 + 1 {
        -7
    } else {
        0
    };
    if why != 0 {
        return why;
    }
    with_host(|h| {
        let live = h.threads.iter().filter(|t| !matches!(t.run().waiting, Waiting::Exited)).count();
        if live == 1 && h.kernel.live_threads <= 1 { 1 } else { -8 }
    })
}

/// Make this Worker's process the staged vfork child, the parent's state
/// kept aside. The child's entry pc, or 0 with nothing to run.
#[no_mangle]
pub extern "C" fn gex_vfork_begin() -> i64 {
    STAGED.with(|c| c.set(false));
    let pid = gex_fork_pending();
    if pid == 0 {
        return 0;
    }
    let [pc, ctx_addr, sp, tls, brk] = CLAIMED_FORK.with(|c| c.get());
    let ctx = {
        let gm = GuestMem::new(guest_bytes());
        gm.read(ctx_addr, ctx::SIZE).map(<[u8]>::to_vec).unwrap_or_default()
    };
    let (compat, mem) = with_host(|h| (h.kernel.compat, h.kernel.mem.clone()));
    // What `adoptProcess` stages before `gex_adopt_child`, from what
    // `do_fork` staged.
    *CWD.0.borrow_mut() = FORK_CWD.0.borrow().clone();
    *CHILD_EXE.0.borrow_mut() = FORK_EXE.0.borrow().clone();
    *CHILD_STATE.0.borrow_mut() = FORK_STATE.0.borrow().clone();
    let fds = FORK_FDS.0.borrow().clone();
    let mut child = child_host(pid, ctx_addr as i64, pc as i64, sp as i64, tls as i64, brk as i64);
    // The child's kernel, thread and status in this Worker's `Host`; the
    // parent's into `child`, kept. The rest (its translation state, the
    // root, `/proc`'s source) is the process's and stays.
    with_host(|h| {
        std::mem::swap(&mut h.kernel, &mut child.kernel);
        std::mem::swap(&mut h.threads, &mut child.threads);
        std::mem::swap(&mut h.exit, &mut child.exit);
    });
    *SAVED.0.borrow_mut() = Some(Saved {
        parent: child,
        ctx,
        ctx_addr,
        cur: cur(),
        exited: EXITED.load(Relaxed),
    });
    adopt_fds_text(&fds);
    with_host(|h| {
        h.kernel.wasm32 = true;
        // The memory is the parent's, and so is its map: the child may not
        // change it (`check`).
        h.kernel.mem = mem;
        publish_process(h);
    });
    // As `adoptProcess` does after `gex_adopt_child`: the ABI, the clock's
    // start, and on an AArch64 build the stack `__clone` passed moved from
    // `ctx::SP` to `esp`, where `__gex_thread_start` looks.
    gex_set_compat(compat as i32);
    CURRENT.with(|c| c.set(0));
    // The getpid family's answers are the parent's until the child's first
    // full call: none until then.
    IDS.with(|c| c.set(gex_kernel::syscalls::Idents { pid: 0, ppid: 0, tid: 0, uid: 0, gid: 0 }));
    EXITED.store(false, Relaxed);
    set_resume_none();
    set_resume_ctx_none();
    WHY.store(0, Relaxed);
    ACTIVE.store(pid, Release);
    pc as i64
}

/// The child's syscall `nr` (canonical), before it is served: the memory
/// map is the parent's too, so a call that would change it, and a further
/// clone, end the child (as a crash: `posix_spawn`'s child makes none).
pub(crate) fn check(nr: u64) {
    use gex_kernel::syscalls::nr::*;
    if matches!(nr, MMAP | MUNMAP | MREMAP | MPROTECT | BRK | CLONE | CLONE3) {
        warn_line(&format!(
            "[gex] pid {}: a vfork child made syscall {nr}, which would change its parent's memory map; ended",
            ACTIVE.load(Relaxed)
        ));
        WHY.store(UNSUPPORTED, Relaxed);
        set_resume_none();
        set_resume_ctx_none();
        trap();
    }
}

/// What a child that execs hands the Worker it goes on in: the command, its
/// descriptors (as `stage_fork_fds` describes them, for this pid), its
/// working directory and its signal state, pending signals included.
static EXEC_PARTS: SingleThreaded<[Vec<u8>; 4]> =
    SingleThreaded(const { RefCell::new([Vec::new(), Vec::new(), Vec::new(), Vec::new()]) });

/// The child's `execve`, which the kernel has checked and resolved: staged
/// for a new Worker instead of done here, where the memory is the parent's.
/// This kernel's references to the child's descriptors are closed -- the
/// staging took references of the new Worker's -- and the frames end (the
/// caller returns 1).
pub(crate) fn stage_exec(h: &mut Host, path: &str, argv: &[String], envp: &[String]) {
    let pid = h.kernel.pid;
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
    let cwd = h.kernel.vfs.cwd.clone().into_bytes();
    let state = state_bytes(&h.kernel, true);
    *EXEC_PARTS.0.borrow_mut() = [cmd, fds, cwd, state];
    h.kernel.fds.close_all();
    h.kernel.remote_sockets.clear();
    h.kernel.sockets.clear();
    WHY.store(EXEC, Relaxed);
    set_resume_none();
    set_resume_ctx_none();
}

/// Why the child's frames ended: 0 (it exited, or crashed), 2 an exec
/// staged, 3 a call it may not make here.
#[no_mangle]
pub extern "C" fn gex_vfork_why() -> u32 {
    WHY.load(Relaxed)
}

/// The running vfork child's pid, or 0.
#[no_mangle]
pub extern "C" fn gex_vfork_pid() -> i32 {
    ACTIVE.load(Relaxed)
}

/// Part `i` of a staged exec (see `EXEC_PARTS`): where, and how long.
#[no_mangle]
pub extern "C" fn gex_vfork_exec_ptr(i: u32) -> u32 {
    EXEC_PARTS.0.borrow().get(i as usize).map_or(0, |v| v.as_ptr() as u32)
}

#[no_mangle]
pub extern "C" fn gex_vfork_exec_len(i: u32) -> u32 {
    EXEC_PARTS.0.borrow().get(i as usize).map_or(0, |v| v.len() as u32)
}

/// The staged exec's parts, no longer needed once JS has copied them.
#[no_mangle]
pub extern "C" fn gex_vfork_exec_done() {
    *EXEC_PARTS.0.borrow_mut() = Default::default();
}

/// The child is done: the parent's process state and register context back
/// in this Worker, the child's kernel dropped (its descriptors were closed
/// by its exit or its exec; a crashed child's close here). The parent's
/// `clone` then returns the child's pid.
#[no_mangle]
pub extern "C" fn gex_vfork_end() {
    let Some(saved) = SAVED.0.borrow_mut().take() else { return };
    let Saved { mut parent, ctx, ctx_addr, cur, exited } = saved;
    with_host(|h| {
        std::mem::swap(&mut h.kernel, &mut parent.kernel);
        std::mem::swap(&mut h.threads, &mut parent.threads);
        std::mem::swap(&mut h.exit, &mut parent.exit);
    });
    // `parent` now holds the child's: its memory map is a copy of the
    // parent's, which owns nothing, and its descriptors are gone or go now.
    drop(parent);
    if !ctx.is_empty() {
        let mut gm = GuestMem::new(guest_bytes());
        let _ = gm.write(ctx_addr, &ctx);
    }
    CURRENT.with(|c| c.set(cur));
    IDS.with(|c| c.set(gex_kernel::syscalls::Idents { pid: 0, ppid: 0, tid: 0, uid: 0, gid: 0 }));
    EXITED.store(exited, Relaxed);
    with_host(|h| {
        clock_constants(h.kernel.compat, h.kernel.started);
        gex_xlate::set_compat(h.kernel.compat);
        publish_process(h);
    });
    set_resume_none();
    set_resume_ctx_none();
    ACTIVE.store(0, Release);
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
