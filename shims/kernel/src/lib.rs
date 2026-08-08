//! The global kernel module.
//!
//! One module, one memory, shared by every Worker of every guest process.
//! That is the whole point of it: the process table, and in time the
//! filesystem, the pipes and the sockets, live *here* -- in a memory every
//! Worker instantiated against -- so a syscall stays a direct call rather
//! than becoming a message and a wait.
//!
//! It is what lets the snapshot switcher go. The switcher exists because
//! every process shares one address space, so running a second one means
//! writing the first out and reading the second in. Give each process its own
//! memory and nothing collides and nothing needs saving -- but then the
//! things processes are *supposed* to share stop being shared too, and this
//! is where they go instead.
//!
//! The process table is first because it is what `fork`, `wait4` and `kill`
//! need across processes, and those are exactly the calls the switcher
//! currently fakes by keeping every process in one place.
//!
//! `gex_kernel::ProcTable` moves in unchanged. It was already written for
//! this: its state is behind a lock, signals are left for the target to
//! collect rather than delivered by the sender, and its comments describe a
//! world where "the target's signal state lives in its own kernel". The only
//! thing that was missing was somewhere for the shared half to live that is
//! not one process's memory.
//!
//! A guest buffer is in the *process's* memory, which this module imports as
//! a second memory and cannot address with a pointer; `gex-kmem` copies
//! across the two with `memory.copy`.

use std::sync::OnceLock;

use gex_kernel::proc::ProcTable;

/// The system's processes, in the memory every Worker shares.
///
/// A `static` rather than anything handed across a boundary: this module's
/// data lives in its own memory, that memory is the global one, and so a
/// static here is already the same bytes for every Worker that instantiated
/// against it. That is the entire trick.
fn procs() -> &'static ProcTable {
    static PROCS: OnceLock<ProcTable> = OnceLock::new();
    PROCS.get_or_init(ProcTable::new)
}

/// Somewhere to stage a guest buffer copied out of a process's memory.
static mut SCRATCH: [u8; 4096] = [0; 4096];

/// Create a process with `ppid` as its parent. Returns the new pid.
#[no_mangle]
pub extern "C" fn k_spawn(ppid: i32) -> i32 {
    procs().spawn(ppid)
}

/// Record that `pid` has exited with `status`.
///
/// `status` is a wait status, not an exit code: a normal exit is `code << 8`
/// and a signal death is the bare signal, which is what `wait4` reports and
/// how a parent tells them apart.
#[no_mangle]
pub extern "C" fn k_exit(pid: i32, status: i32) {
    procs().exit(pid, status);
}

/// Collect a finished child of `ppid`, or -1 if none is ready.
///
/// `want` is a specific pid, or -1 for any. The status is left in
/// `k_wait_status` -- one call, two results, and no pointer into a memory
/// this module cannot address.
#[no_mangle]
pub extern "C" fn k_wait(ppid: i32, want: i32) -> i32 {
    for c in procs().children(ppid) {
        if want > 0 && c.pid != want {
            continue;
        }
        let mut st = c.status.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(status) = st.take() {
            c.reaped.store(true, std::sync::atomic::Ordering::Release);
            procs().reap(c.pid);
            LAST_STATUS.store(status, std::sync::atomic::Ordering::Relaxed);
            return c.pid;
        }
    }
    -1
}

static LAST_STATUS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// The wait status of the child [`k_wait`] last collected.
#[no_mangle]
pub extern "C" fn k_wait_status() -> i32 {
    LAST_STATUS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether `ppid` has any live children, which is what tells `wait4` the
/// difference between "nothing ready yet" and `ECHILD`.
#[no_mangle]
pub extern "C" fn k_has_children(ppid: i32) -> i32 {
    procs().has_children(ppid) as i32
}

/// Send a signal, leaving it pending for the target to collect.
///
/// Returns the number of processes signalled, or a negative errno. The
/// sender never delivers: the target picks it up at its next syscall, which
/// is what makes this safe to call from another process's Worker.
#[no_mangle]
pub extern "C" fn k_kill(caller: i32, target: i32, signo: u32) -> i32 {
    match procs().kill(caller, target, signo) {
        Ok(n) => n as i32,
        Err(e) => -(e.0 as i32),
    }
}

/// Take everything pending for `pid`, as a bitmask of `1 << (signo - 1)`.
#[no_mangle]
pub extern "C" fn k_take_signals(pid: i32) -> u64 {
    procs().take_signals(pid)
}

/// Whether anything is pending for `pid`, without taking it.
#[no_mangle]
pub extern "C" fn k_has_signals(pid: i32) -> i32 {
    procs().has_signals(pid) as i32
}

/// Whether `pid` is a live process.
#[no_mangle]
pub extern "C" fn k_exists(pid: i32) -> i32 {
    procs().exists(pid) as i32
}

/// The parent of `pid`.
#[no_mangle]
pub extern "C" fn k_ppid(pid: i32) -> i32 {
    procs().ppid(pid)
}

/// Ask every process to stop, with this status. See `ProcTable::request_halt`.
#[no_mangle]
pub extern "C" fn k_request_halt(code: i32) {
    procs().request_halt(code);
}

/// The halt status, or -1 if nothing has asked for one.
#[no_mangle]
pub extern "C" fn k_halt_code() -> i32 {
    procs().halt_code().unwrap_or(-1)
}

/// Copy `len` bytes of the calling process's memory in, and report their sum.
///
/// The shape every syscall taking a pointer will use: the argument names an
/// address in a memory this module cannot dereference, so it is copied here
/// first. Kept as a live check that the two-memory path works from inside the
/// real kernel and not only from the toy one it replaced.
#[no_mangle]
pub extern "C" fn k_checksum(at: u32, len: u32) -> u32 {
    let n = (len as usize).min(4096);
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
    unsafe { gex_kmem::pull(buf.as_mut_ptr(), at, n as u32) };
    let mut sum: u32 = 0;
    for b in &buf[..n] {
        sum = sum.wrapping_mul(31).wrapping_add(*b as u32);
    }
    sum
}
