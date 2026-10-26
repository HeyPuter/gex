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
//!      state (the process's one `JIT`). `with_exec_lock` serves this Worker that host
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
//!
//! A vfork child may itself spawn so (Qt's `startDetached`: its child's
//! `vforkfd`; glibc's `posix_spawn` from a child), and that grandchild runs
//! here too, nested: the child's state is kept aside as the parent's thread
//! is ([`STACK`]), and comes back when the grandchild is done. So the
//! grandchild writes the memory its parent reads, as glibc's `posix_spawn`
//! has its child report a failed exec (`args->err`), where a copy of the
//! memory would lose it.

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
    /// Its `Host`, which `with_exec_lock` serves this Worker while it runs.
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
    /// The staged exec's memory (`exec_target`): 1 a 64-bit one, 0 a 32-bit
    /// one, -1 the parent's (a page that runs one flavour).
    static EXEC_MEM64: Cell<i32> = const { Cell::new(-1) };
    /// The vfork children suspended here while one of theirs runs, the
    /// innermost last.
    static STACK: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

/// How deep vfork children nest here: a spawn from a spawned child (Qt's
/// `startDetached`) is two. A deeper one is copied as a fork.
const DEPTH: usize = 4;

/// A suspended vfork child: what [`PID`], [`CHILD`], [`FDS`], [`EXITED`],
/// [`WHY`] and [`SAVED`] held for it.
struct Frame {
    pid: i32,
    child: *mut Host,
    fds: *const gex_kernel::file::FdTable,
    exited: bool,
    why: u32,
    saved: Option<Saved>,
}

/// How many vfork children are here, running or suspended.
fn depth() -> usize {
    STACK.with(|s| s.borrow().len()) + active() as usize
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

/// The child's `Host`, while it runs here (`with_exec_lock`).
pub(crate) fn host_here() -> Option<*mut Host> {
    let p = CHILD.with(|c| c.get());
    (!p.is_null()).then_some(p)
}

/// `f` on the child's `Host`, which only this Worker reaches (`CHILD`): no
/// lock.
fn with_child<R>(f: impl FnOnce(&mut Host) -> R) -> R {
    let p = host_here().expect("gex: no vfork child here");
    // SAFETY: boxed in `gex_vfork_begin`, dropped only by `gex_vfork_end`,
    // both on this Worker, outside any call here.
    f(unsafe { &mut *p })
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

/// Note whether the `clone` being served is a vfork to run here: one a
/// vfork child makes (`check` let it through) runs nested, up to [`DEPTH`].
pub(crate) fn note_clone(vfork: bool) {
    STAGED.with(|c| c.set(vfork && depth() < DEPTH));
}

/// Whether the fork the last syscall staged is a vfork child to run here
/// (1; why not, negative): a `CLONE_VM | CLONE_VFORK` clone, from no
/// provider's frames, not a Windows process, with a global kernel to
/// publish the child's exit in.
#[no_mangle]
pub extern "C" fn gex_vfork_ok() -> i32 {
    if depth() >= DEPTH {
        -1
    } else if !STAGED.with(|c| c.get()) {
        -2
    } else if IN_PROVIDER.with(|c| c.get()) || cgen::provider_callout_open() {
        -4
    } else if unsafe { k_present() } == 0 {
        -6
    } else if !crate::fork_staged() {
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
    // The parent's constants and a copy of its map, from what it publishes
    // (no kernel lock, L25c).
    let compat = crate::compat_abi();
    let started = crate::clock_started();
    let wasm32 = crate::WASM32.load(Acquire);
    let mem = crate::sigs::proc_home().mm().clone();
    // What `adoptProcess` stages before `gex_adopt_child`, from what
    // `do_fork` staged.
    *CWD.0.borrow_mut() = FORK_CWD.with(|c| c.borrow().clone());
    *CHILD_EXE.0.borrow_mut() = FORK_EXE.with(|c| c.borrow().clone());
    *CHILD_STATE.0.borrow_mut() = FORK_STATE.with(|c| c.borrow().clone());
    // Not the shared mappings `do_fork` described with them (`m` lines):
    // the memory is the parent's, and its map has them already. Adopted, each
    // was there twice, the second writable, written back at the child's exec.
    let fds: String = FORK_FDS.with(|c| c.borrow().lines().filter(|l| !l.starts_with("m ")).flat_map(|l| [l, "\n"]).collect());
    // A vfork child spawning: it waits, kept aside, as its parent does.
    if active() {
        let f = Frame {
            pid: PID.with(|c| c.get()),
            child: CHILD.with(|c| c.replace(std::ptr::null_mut())),
            fds: FDS.with(|c| c.replace(std::ptr::null())),
            exited: EXITED.with(|c| c.get()),
            why: WHY.with(|c| c.get()),
            saved: SAVED.with(|s| s.borrow_mut().take()),
        };
        STACK.with(|s| s.borrow_mut().push(f));
    }
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
    // The child's host is this Worker's alone (`CHILD`): no lock (L25c).
    with_child(|h| {
        // As `gex_set_compat` does for an adopted child: on an AArch64
        // build an i386-ABI process's stack is `esp`, where `__clone`'s
        // child stack has to be.
        if h.kernel.compat && !gex_abi::X86 {
            crate::compat_stack(h.kernel.ctx_addr);
        }
        publish_vfork_child(h);
    });
    // Its descriptors, into its own table now that it is published here.
    adopt_fds_text(&fds);
    set_resume_none();
    set_resume_ctx_none();
    pc as i64
}

/// The child's syscall `nr` (canonical, first argument `a0`), before it is
/// served: the memory map is the parent's too, so a call that would change
/// it ends the child (as a crash: `posix_spawn`'s child makes none). So does
/// a further clone, except a `CLONE_VM | CLONE_VFORK` one, which shares the
/// map rather than changes it: Qt's `startDetached` spawns from its vfork
/// child (`vforkfd`), and that grandchild runs here, nested (`note_clone`);
/// and except a fork, whose child is a copy of the memory as this child
/// sees it.
pub(crate) fn check(nr: u64, a0: u64) {
    use gex_kernel::syscalls::nr::*;
    const SPAWN: u64 = 0x100 | 0x4000; // CLONE_VM | CLONE_VFORK
    let flags = match nr {
        CLONE => a0,
        CLONE3 => GuestMem::new(guest_bytes()).read_u64(a0).unwrap_or(0),
        _ => 0,
    };
    // A spawn shares the map; a fork (no CLONE_VM) copies it, and its
    // child gets a Worker of its own, as any fork's does.
    if flags & SPAWN == SPAWN || (matches!(nr, CLONE | CLONE3) && flags & 0x100 == 0) {
        return;
    }
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
        "[gex] pid {}: a vfork child called {name} ({a0:#x}), which would change its parent's memory map; ended",
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
    stage_exec_parts(h, path, argv, envp);
    WHY.with(|c| c.set(EXEC));
    set_resume_none();
    set_resume_ctx_none();
}

/// An exec's record for a new Worker (`EXEC_PARTS`, `EXEC_MEM64`): its
/// command line, descriptors (this kernel's references then closed: the
/// staging took the new Worker's), cwd and signal state. A vfork child's
/// (`stage_exec`) or a process's whose program needs the other memory
/// flavour (lib.rs `exec_away`).
pub(crate) fn stage_exec_parts(h: &mut Host, path: &str, argv: &[String], envp: &[String]) {
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
    stage_fork_fds(h, pid, false);
    let fds = FORK_FDS.with(|c| std::mem::take(&mut *c.borrow_mut())).into_bytes();
    let cwd = h.kernel.caller_cwd().into_bytes();
    let state = state_bytes(&h.kernel, true);
    let mem64 = if page_flavours() & 2 != 0 { exec_target(h, path) as i32 } else { -1 };
    EXEC_MEM64.with(|c| c.set(mem64));
    EXEC_PARTS.with(|p| *p.borrow_mut() = [cmd, fds, cwd, state]);
    h.kernel.fds.close_all();
}

/// [`exec_memory`] for an exec's path, relative to the caller's cwd.
fn exec_memory_of(h: &Host, path: &str) -> Need {
    if path.starts_with('/') {
        exec_memory(path)
    } else {
        let cwd = h.kernel.caller_cwd();
        exec_memory(&format!("{}/{path}", cwd.trim_end_matches('/')))
    }
}

/// The page's memory flavours, from JS (`gex_set_page_flavours`): bit 0 the
/// page's own is a 64-bit memory (`?mem64=1`), bit 1 it runs the other one
/// too (a global kernel, host.mjs `#other`). Unset (-1): this Worker's
/// flavour is the page's, and only a 64-bit memory's page runs the other.
static PAGE_FLAVOURS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

fn page_flavours() -> i32 {
    match PAGE_FLAVOURS.load(std::sync::atomic::Ordering::Relaxed) {
        -1 if gex_abi::mem64() => 3,
        -1 => 0,
        v => v,
    }
}

/// Bit 0: the page's memory is a 64-bit one; bit 1: it runs the other
/// flavour as well ([`exec_target`]). Set in every Worker before the guest
/// runs.
#[no_mangle]
pub extern "C" fn gex_set_page_flavours(bits: i32) {
    PAGE_FLAVOURS.store(bits & 3, std::sync::atomic::Ordering::Relaxed);
}

/// The memory flavour an exec of `path` goes on in (docs/mem64.md §5), true
/// a 64-bit one: what the program needs (`exec_memory`), else what the exec
/// asked for (the kernel's `exec_mem64`: the call's GEX_MEM64 entry, which
/// it took out of the new environment, or the image's `user.gex.mem64`
/// record) when the page runs that flavour, else the page's own -- the
/// lowered wasm32 memory unless the page is `?mem64=1`. So a 64-bit
/// program's children are the page's flavour again unless they ask.
pub(crate) fn exec_target(h: &Host, path: &str) -> bool {
    let page = page_flavours();
    let own = page & 1 != 0;
    // The default page, and nothing asked: the lowered wasm32 memory, as
    // without the choice, the program not even looked at.
    if !own && h.kernel.exec_mem64.is_none() {
        return false;
    }
    match exec_memory_of(h, path) {
        Need::Is(m) => m,
        Need::Either => match h.kernel.exec_mem64 {
            Some(m) if m == own || page & 2 != 0 => m,
            Some(m) => {
                crate::warn_line(&format!(
                    "gex: {path}: a {}-bit memory was asked for, which this page does not run\n",
                    if m { 64 } else { 32 }
                ));
                own
            }
            None => own,
        },
    }
}

/// An exec of the caller's own command line (its arguments after the
/// first, the same) keeps the caller's memory flavour unless the exec asks
/// otherwise (docs/mem64.md §5): it is the same program started again, as
/// 64-bit Wine's first Wine process starts the program it was given as a process
/// of its own. Any other child is the page's flavour unless it asks.
pub(crate) fn exec_relaunch(h: &mut Host, argv: &[String]) {
    if h.kernel.exec_mem64.is_none() && page_flavours() & 3 != 0 && argv.len() > 1 && argv.get(1..) == h.kernel.argv.get(1..) {
        h.kernel.exec_mem64 = Some(gex_abi::mem64());
    }
}

/// The program an exec of `path` runs instead (docs/mem64.md §5): of a
/// program and its memory64 twin `<path>.m64` side by side, the one of the
/// memory the exec wants (`exec_mem64`, else the page's). 64-bit Wine's loader in a
/// twin dist (wine64-dist.sh WINE64_TWIN=1) is the case: an exec of `wine` that
/// asked for a 64-bit memory runs `wine.m64`, and the 64-bit Wine's own
/// children, which Wine starts from its loader (`/proc/self/exe`, so
/// `wine.m64`), go back to `wine` unless they ask too.
pub(crate) fn exec_route(h: &Host, path: &str) -> Option<String> {
    let page = page_flavours();
    if page & 3 == 0 {
        return None;
    }
    let want = h.kernel.exec_mem64.unwrap_or(page & 1 != 0);
    let (other, need) = match path.strip_suffix(".m64") {
        Some(base) if !want => (base.to_string(), false),
        None if want => (format!("{path}.m64"), true),
        _ => return None,
    };
    if exec_memory_of(h, path) != Need::Is(!need) {
        return None;
    }
    (exec_memory_of(h, &other) == Need::Is(need)).then_some(other)
}

/// Whether this process's exec of `path` goes on in another Worker: its
/// program's memory flavour ([`exec_target`]) is not this Worker's (lib.rs
/// `exec_away`).
pub(crate) fn exec_elsewhere(h: &Host, path: &str) -> bool {
    page_flavours() & 2 != 0 && exec_target(h, path) != gex_abi::mem64()
}

/// What memory a program needs: [`Need::Is`] the one it can run in, or
/// either.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Need {
    Is(bool),
    Either,
}

/// What memory the program at `path` needs (docs/mem64.md §5): a 32-bit
/// ELF (an i386 program, or a wasm32 container such as 64-bit Wine's wineserver,
/// docs/wine-native-x64.md §4) a 32-bit one, a gex wasm container what its
/// module's memory import is, and a 64-bit ELF, translated, either. Not an
/// ELF (a script) or unreadable: either.
fn exec_memory(path: &str) -> Need {
    let home = crate::sigs::proc_home();
    let mut head = vec![0u8; 64 << 10];
    let Some(n) = home.read_file_at(path, 0, &mut head) else { return Need::Either };
    if n < 64 || &head[..4] != b"\x7fELF" {
        return Need::Either;
    }
    if head[4] == 1 {
        return Need::Is(false);
    }
    // The module sits right after the program headers (wasm-elf.py), in
    // the first 64 KiB, and its import section after the type section: the
    // 4 MiB from there hold it. A translated program has none, and only its
    // first 64 KiB are read.
    let Some(at) = head[..n].windows(8).position(|w| w == b"\0asm\x01\0\0\0") else {
        return Need::Either;
    };
    let mut buf = vec![0u8; 4 << 20];
    let Some(n) = home.read_file_at(path, at as u64, &mut buf) else { return Need::Either };
    wasm_memory64(&buf[..n]).map_or(Need::Either, Need::Is)
}

/// Whether `wasm`'s imported memory is a 64-bit one; `None` when its
/// import section is not in `wasm` or it imports no memory.
pub(crate) fn wasm_memory64(wasm: &[u8]) -> Option<bool> {
    fn leb(b: &[u8], i: &mut usize) -> Option<u64> {
        let (mut r, mut s) = (0u64, 0);
        loop {
            let x = *b.get(*i)?;
            *i += 1;
            r |= ((x & 0x7f) as u64) << s;
            s += 7;
            if x < 0x80 {
                return Some(r);
            }
        }
    }
    let mut i = 8;
    while i < wasm.len() {
        let id = wasm[i];
        i += 1;
        let size = leb(wasm, &mut i)? as usize;
        let end = i.checked_add(size)?;
        if id == 2 {
            let mut j = i;
            for _ in 0..leb(wasm, &mut j)? {
                for _ in 0..2 {
                    let l = leb(wasm, &mut j)? as usize;
                    j += l;
                }
                match *wasm.get(j)? {
                    0 => {
                        j += 1;
                        leb(wasm, &mut j)?;
                    }
                    1 => {
                        j += 2;
                        let fl = *wasm.get(j - 1)?;
                        leb(wasm, &mut j)?;
                        if fl & 1 != 0 {
                            leb(wasm, &mut j)?;
                        }
                    }
                    2 => return Some(*wasm.get(j + 1)? & 4 != 0),
                    3 => j += 3,
                    _ => return None,
                }
            }
            return None;
        }
        if id > 2 && id != 0 {
            return None;
        }
        i = end;
    }
    None
}

/// The staged exec's memory (`exec_memory`): 1 64-bit, 0 32-bit, -1 the
/// parent's (and always -1 outside a 64-bit memory's page).
#[no_mangle]
pub extern "C" fn gex_vfork_exec_mem64() -> i32 {
    EXEC_MEM64.with(|c| c.get())
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

/// The child is done: the Worker back to the parent's process (or to the
/// vfork child it nested in, [`STACK`]) and its calling thread, with that
/// thread's register context, the child's kernel dropped (its descriptors were closed by its exit or its exec; a crashed
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
    let outer = STACK.with(|s| s.borrow_mut().pop());
    PID.with(|c| c.set(outer.as_ref().map_or(0, |f| f.pid)));
    EXITED.with(|c| c.set(outer.as_ref().is_some_and(|f| f.exited)));
    WHY.with(|c| c.set(outer.as_ref().map_or(0, |f| f.why)));
    if !ctx.is_empty() {
        let mut gm = GuestMem::new(guest_bytes());
        let _ = gm.write(ctx_addr, &ctx);
    }
    CURRENT.with(|c| c.set(cur));
    if let Some(f) = outer {
        // The vfork child that spawned, its process this Worker's again.
        CHILD.with(|c| c.set(f.child));
        FDS.with(|c| c.set(f.fds));
        SAVED.with(|s| *s.borrow_mut() = f.saved);
        with_child(publish_vfork_child);
    } else {
        // The parent's thread again, from its thread table (no kernel lock).
        crate::bind_free(cur);
    }
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
    // A fresh Worker's process, its own until the program runs (L25c).
    crate::with_own_host(|h| {
        let entry = do_exec(h, &path, &argv, &envp);
        if entry == 0 {
            warn_line(&format!("gex: {path}: the program could not be loaded\n"));
            exit_process(127);
            return 0;
        }
        entry
    })
}
