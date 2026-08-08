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
/// `status` is an exit *code*, or the negated signal number when the process
/// died of one -- the same convention `ProcTable::exit` takes. The two are
/// different things to `wait4`, and [`k_wait`] is where they become the one
/// word a parent reads.
#[no_mangle]
pub extern "C" fn k_exit(pid: i32, status: i32) {
    procs().exit(pid, status);
}

/// Collect a finished child of `ppid`. Zero if none is ready.
///
/// Returns the pid in the high 32 bits and the wait status in the low 32:
/// one call and two results, rather than a second call for the status. The
/// pairing has to be atomic, because two processes in two Workers can be
/// collecting from this table at the same moment and a status left in a
/// static between calls belongs to whichever of them asked last.
///
/// `want` is a specific pid, or -1 for any.
#[no_mangle]
pub extern "C" fn k_wait(ppid: i32, want: i32) -> i64 {
    for c in procs().children(ppid) {
        if want > 0 && c.pid != want {
            continue;
        }
        let mut st = c.status.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(code) = st.take() {
            c.reaped.store(true, std::sync::atomic::Ordering::Release);
            procs().reap(c.pid);
            // `WIFEXITED`: the code in the second byte, low bits clear.
            // `WIFSIGNALED`: the signal in the low seven, which is what a
            // negative code means here.
            let word = if code < 0 {
                (-code) as u32 & 0x7f
            } else {
                ((code as u32) & 0xff) << 8
            };
            return ((c.pid as i64) << 32) | word as i64;
        }
    }
    0
}

/// Whether a `wait4` by `ppid` for `want` would answer now: a collectable
/// child, or no matching child at all.
///
/// Separate from [`k_wait`] because collecting *takes* the child, and a
/// scheduler asking whether a parked thread can run must not reap one.
#[no_mangle]
pub extern "C" fn k_wait_ready(ppid: i32, want: i32) -> i32 {
    procs().wait_ready(ppid, want) as i32
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

/// Forget a collected process, so a long-lived init does not accumulate a
/// record per respawn for the life of the machine.
#[no_mangle]
pub extern "C" fn k_reap(pid: i32) {
    procs().reap(pid);
}

/// `getpgid`, with Linux's zero-means-the-caller rule. Negative errno on
/// failure, as every call here reports it.
#[no_mangle]
pub extern "C" fn k_pgid(caller: i32, of: i32) -> i32 {
    match procs().pgid(caller, of) {
        Ok(v) => v,
        Err(e) => -e.0,
    }
}

/// `getsid`, same rule.
#[no_mangle]
pub extern "C" fn k_sid(caller: i32, of: i32) -> i32 {
    match procs().sid(caller, of) {
        Ok(v) => v,
        Err(e) => -e.0,
    }
}

/// `setsid`: become the leader of a new session and a new group.
#[no_mangle]
pub extern "C" fn k_setsid(pid: i32) -> i32 {
    match procs().setsid(pid) {
        Ok(v) => v,
        Err(e) => -e.0,
    }
}

/// `setpgid`. Zero for either argument means the caller.
#[no_mangle]
pub extern "C" fn k_setpgid(caller: i32, target: i32, pgid: i32) -> i32 {
    match procs().setpgid(caller, target, pgid) {
        Ok(()) => 0,
        Err(e) => -e.0,
    }
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

// -- pipes -------------------------------------------------------------------
//
// The second thing processes share, and the one a shell pipeline and apt's
// method workers are made of. A pipe is a buffer with two ends and a pair of
// counts, which is exactly the kind of state that forced every process into
// one address space: `Arc<Pipe>` is a pointer, and a pointer means a memory.
//
// Here it is a *number* instead. Each process's file table holds an id and the
// buffer lives in the global memory, so the two ends can be in Workers that
// share nothing else.

/// Every pipe in the system, by id.
fn pipes() -> &'static std::sync::Mutex<Vec<std::sync::Arc<gex_kernel::pipe::Pipe>>> {
    static PIPES: OnceLock<std::sync::Mutex<Vec<std::sync::Arc<gex_kernel::pipe::Pipe>>>> =
        OnceLock::new();
    PIPES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn pipe_at(id: i32) -> Option<std::sync::Arc<gex_kernel::pipe::Pipe>> {
    pipes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id as usize)
        .cloned()
}

/// Make a pipe with both ends open. Returns its id.
#[no_mangle]
pub extern "C" fn k_pipe_new() -> i32 {
    let mut t = pipes().lock().unwrap_or_else(|e| e.into_inner());
    t.push(std::sync::Arc::new(gex_kernel::pipe::Pipe::new()));
    (t.len() - 1) as i32
}

/// Another descriptor onto one end, as `dup` and `fork` both make.
#[no_mangle]
pub extern "C" fn k_pipe_open_end(id: i32, write: i32) {
    if let Some(p) = pipe_at(id) {
        p.open_end(write != 0);
    }
}

/// Drop a descriptor. The last one on an end is what a reader sees as EOF.
#[no_mangle]
pub extern "C" fn k_pipe_close_end(id: i32, write: i32) {
    if let Some(p) = pipe_at(id) {
        p.close_end(write != 0);
    }
}

/// Write `len` bytes of the caller's memory into the pipe.
///
/// The bytes are copied out of the calling process's memory first, because
/// this module cannot address it -- that copy is what makes the two ends able
/// to live in different address spaces at all. Returns bytes written, or a
/// negative errno.
#[no_mangle]
pub extern "C" fn k_pipe_write(id: i32, at: u32, len: u32) -> i32 {
    let Some(p) = pipe_at(id) else { return -9 }; // EBADF
    let mut buf = vec![0u8; len as usize];
    unsafe { gex_kmem::pull(buf.as_mut_ptr(), at, len) };
    match p.write(&buf) {
        Ok(n) => n as i32,
        Err(e) => -(e.0 as i32),
    }
}

/// Read up to `len` bytes into the caller's memory. See [`k_pipe_write`].
#[no_mangle]
pub extern "C" fn k_pipe_read(id: i32, at: u32, len: u32) -> i32 {
    let Some(p) = pipe_at(id) else { return -9 };
    let mut buf = vec![0u8; len as usize];
    match p.read(&mut buf) {
        Ok(n) => {
            unsafe { gex_kmem::push(at, buf.as_ptr(), n as u32) };
            n as i32
        }
        Err(e) => -(e.0 as i32),
    }
}

/// Would a read block? False also when every writer has gone, which is EOF.
#[no_mangle]
pub extern "C" fn k_pipe_readable(id: i32) -> i32 {
    pipe_at(id).is_some_and(|p| p.readable()) as i32
}

/// Would a write block?
#[no_mangle]
pub extern "C" fn k_pipe_writable(id: i32) -> i32 {
    pipe_at(id).is_some_and(|p| p.writable()) as i32
}

/// How many bytes are waiting, for `FIONREAD` and for a poll's report.
#[no_mangle]
pub extern "C" fn k_pipe_buffered(id: i32) -> i32 {
    pipe_at(id).map_or(0, |p| p.buffered() as i32)
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

// -- the filesystem ----------------------------------------------------------
//
// The third thing processes share, and the one without which the other two do
// not help. A shell that unpacks a rootfs and forks hands its child an empty
// root; the child's `execve` cannot find the program it was forked to run.
// Under the switcher the child got the parent's whole address space and the
// question never arose.
//
// So the files live here. Each process mounts a `RemoteFs` -- an id and
// nothing else -- and every call arrives as an address in that process's
// memory, which `gex-kmem` copies across. The layouts on the wire are in
// `gex_kernel::vfs::remote`, written once and used by both sides.

use gex_kernel::vfs::{remote, FileHandle, FileSystem};

/// The root every process sees.
fn rootfs() -> &'static std::sync::Arc<gex_kernel::vfs::TmpFs> {
    static ROOT: OnceLock<std::sync::Arc<gex_kernel::vfs::TmpFs>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let fs = gex_kernel::vfs::TmpFs::new();
        // The directories a guest expects to be there before anything has
        // been bundled. `/tmp` above all: a shell, a compiler and apt all
        // write into it, and `open(O_CREAT)` under a directory that does not
        // exist is `ENOENT` rather than a directory being made.
        for d in ["/tmp", "/proc", "/dev", "/run", "/var", "/var/tmp"] {
            fs.add_dir(d);
        }
        std::sync::Arc::new(fs)
    })
}

/// Every file a process has open here, by id.
///
/// A `Vec` with holes rather than a map: an id is handed straight back to a
/// process and used on every read, so the lookup is the hot part and the
/// churn is not.
type Handles = std::sync::Mutex<Vec<Option<std::sync::Arc<dyn FileHandle>>>>;
fn handles() -> &'static Handles {
    static H: OnceLock<Handles> = OnceLock::new();
    H.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn handle_at(id: i32) -> Option<std::sync::Arc<dyn FileHandle>> {
    handles()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id as usize)
        .cloned()
        .flatten()
}

/// A path argument: copied out of the caller's memory, because this module
/// cannot dereference an address in it.
fn path_of(at: u32, len: u32) -> String {
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        unsafe { gex_kmem::pull(buf.as_mut_ptr(), at, len) };
    }
    // Leading slashes go. A mount hands its filesystem a relative path and a
    // host handing one in from outside naturally writes the absolute one, and
    // `TmpFs` keys on the relative form -- so accepting both here is the
    // difference between a bundle that is readable and one that is only
    // writable.
    String::from_utf8_lossy(&buf)
        .trim_start_matches('/')
        .to_string()
}

/// A `Result` as the caller reads it: a count, or a negated errno.
fn as_ret(r: Result<u64, gex_kernel::errno::Errno>) -> i32 {
    match r {
        Ok(n) => n as i32,
        Err(e) => -e.0,
    }
}

fn as_unit(r: Result<(), gex_kernel::errno::Errno>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => -e.0,
    }
}

/// Open a path, returning a handle id or a negated errno.
#[no_mangle]
pub extern "C" fn k_fs_open(path: u32, path_len: u32, flags: u64, mode: u32) -> i32 {
    let p = path_of(path, path_len);
    match rootfs().open(&p, flags, mode) {
        Ok(h) => {
            let mut t = handles().lock().unwrap_or_else(|e| e.into_inner());
            // The first free slot, so a process that opens and closes in a
            // loop does not grow this without bound.
            match t.iter().position(|s| s.is_none()) {
                Some(i) => {
                    t[i] = Some(h);
                    i as i32
                }
                None => {
                    t.push(Some(h));
                    (t.len() - 1) as i32
                }
            }
        }
        Err(e) => -e.0,
    }
}

/// Drop a handle. Called from the client's `Drop`, so a process that exits
/// without closing still frees what it held.
#[no_mangle]
pub extern "C" fn k_fs_close(handle: i32) {
    let mut t = handles().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = t.get_mut(handle as usize) {
        *slot = None;
    }
}

/// Read into the caller's memory. Returns bytes read, or a negated errno.
#[no_mangle]
pub extern "C" fn k_fs_read(handle: i32, offset: u64, at: u32, len: u32) -> i32 {
    let Some(h) = handle_at(handle) else { return -9 };
    let mut buf = vec![0u8; len as usize];
    match h.read_at(offset, &mut buf) {
        Ok(n) => {
            if n > 0 {
                unsafe { gex_kmem::push(at, buf.as_ptr(), n as u32) };
            }
            n as i32
        }
        Err(e) => -e.0,
    }
}

/// Write from the caller's memory.
#[no_mangle]
pub extern "C" fn k_fs_write(handle: i32, offset: u64, at: u32, len: u32) -> i32 {
    let Some(h) = handle_at(handle) else { return -9 };
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        unsafe { gex_kmem::pull(buf.as_mut_ptr(), at, len) };
    }
    as_ret(h.write_at(offset, &buf))
}

/// This handle's metadata, as `remote::META_BYTES` bytes at `at`.
#[no_mangle]
pub extern "C" fn k_fs_meta(handle: i32, at: u32) -> i32 {
    let Some(h) = handle_at(handle) else { return -9 };
    match h.metadata() {
        Ok(m) => {
            let b = remote::encode_meta(&m);
            unsafe { gex_kmem::push(at, b.as_ptr(), b.len() as u32) };
            0
        }
        Err(e) => -e.0,
    }
}

#[no_mangle]
pub extern "C" fn k_fs_truncate(handle: i32, len: u64) -> i32 {
    let Some(h) = handle_at(handle) else { return -9 };
    as_unit(h.truncate(len))
}

/// A directory listing, or `-ERANGE` when `cap` will not hold it.
#[no_mangle]
pub extern "C" fn k_fs_read_dir(handle: i32, at: u32, cap: u32) -> i32 {
    let Some(h) = handle_at(handle) else { return -9 };
    match h.read_dir() {
        Ok(entries) => {
            let b = remote::encode_dir(&entries);
            if b.len() as u32 > cap {
                return -34; // ERANGE
            }
            if !b.is_empty() {
                unsafe { gex_kmem::push(at, b.as_ptr(), b.len() as u32) };
            }
            b.len() as i32
        }
        Err(e) => -e.0,
    }
}

/// A path's metadata, without opening it -- which matters for a symlink and
/// for anything an open would refuse.
#[no_mangle]
pub extern "C" fn k_fs_path_meta(path: u32, path_len: u32, at: u32) -> i32 {
    let p = path_of(path, path_len);
    match rootfs().metadata(&p) {
        Ok(m) => {
            let b = remote::encode_meta(&m);
            unsafe { gex_kmem::push(at, b.as_ptr(), b.len() as u32) };
            0
        }
        Err(e) => -e.0,
    }
}

#[no_mangle]
pub extern "C" fn k_fs_read_link(path: u32, path_len: u32, at: u32, cap: u32) -> i32 {
    let p = path_of(path, path_len);
    match rootfs().read_link(&p) {
        Ok(t) => {
            let b = t.as_bytes();
            if b.len() as u32 > cap {
                return -34; // ERANGE
            }
            if !b.is_empty() {
                unsafe { gex_kmem::push(at, b.as_ptr(), b.len() as u32) };
            }
            b.len() as i32
        }
        Err(e) => -e.0,
    }
}

#[no_mangle]
pub extern "C" fn k_fs_unlink(path: u32, path_len: u32) -> i32 {
    as_unit(rootfs().unlink(&path_of(path, path_len)))
}

#[no_mangle]
pub extern "C" fn k_fs_mkdir(path: u32, path_len: u32, mode: u32) -> i32 {
    as_unit(rootfs().mkdir(&path_of(path, path_len), mode))
}

#[no_mangle]
pub extern "C" fn k_fs_rmdir(path: u32, path_len: u32) -> i32 {
    as_unit(rootfs().rmdir(&path_of(path, path_len)))
}

#[no_mangle]
pub extern "C" fn k_fs_rename(from: u32, from_len: u32, to: u32, to_len: u32) -> i32 {
    as_unit(rootfs().rename(&path_of(from, from_len), &path_of(to, to_len)))
}

#[no_mangle]
pub extern "C" fn k_fs_chmod(path: u32, path_len: u32, mode: u32) -> i32 {
    as_unit(rootfs().chmod(&path_of(path, path_len), mode))
}

#[no_mangle]
pub extern "C" fn k_fs_symlink(target: u32, target_len: u32, path: u32, path_len: u32) -> i32 {
    as_unit(rootfs().symlink(&path_of(target, target_len), &path_of(path, path_len)))
}

#[no_mangle]
pub extern "C" fn k_fs_link(from: u32, from_len: u32, to: u32, to_len: u32) -> i32 {
    as_unit(rootfs().link(&path_of(from, from_len), &path_of(to, to_len)))
}

/// Put a file into the shared root before anything runs.
///
/// How a page's bundle gets in: it is the host's, not a guest's, and it
/// arrives once rather than through a process that would then be the only one
/// able to see it.
#[no_mangle]
pub extern "C" fn k_fs_add(path: u32, path_len: u32, at: u32, len: u32) -> i32 {
    let p = path_of(path, path_len);
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        unsafe { gex_kmem::pull(buf.as_mut_ptr(), at, len) };
    }
    rootfs().add_file(&p, buf);
    0
}

/// Whether the shared root has anything in it yet.
///
/// The first Worker fills it and the rest must not: they would each unpack
/// the same rootfs over the top, at some cost and to no effect.
#[no_mangle]
pub extern "C" fn k_fs_populated() -> i32 {
    POPULATED.load(std::sync::atomic::Ordering::Acquire) as i32
}

/// Say the root is filled, so no other Worker fills it again.
#[no_mangle]
pub extern "C" fn k_fs_populate_done() {
    POPULATED.store(true, std::sync::atomic::Ordering::Release);
}

static POPULATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
