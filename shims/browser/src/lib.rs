//! The browser host: the same kernel, in wasm.
//!
//! PLAN §9's claim is that a host is thin enough for both of them to share one
//! `gex-kernel`. The native host is a Rust program embedding wasmtime; this
//! one *is* wasm, and the guest module runs beside it.
//!
//! # Sharing the address space
//!
//! Two wasm modules cannot share a memory unless both import it, so this shim
//! imports the guest's -- the same 4 GiB the guest module gets -- and puts its
//! own heap and stack inside it. That is not a workaround, it is what makes a
//! syscall cheap: the kernel reads the guest's buffers where they lie, with no
//! copy across a host boundary and no JS in the path.
//!
//! It works because the guest's layout leaves a hole. Everything the guest
//! uses ends at `STACK_TOP`, two gigabytes in; the memory is just under four.
//! The linker is told to put this module's data and stack above that (see
//! `build.sh`), so the two never meet.
//!
//! # Status
//!
//! Runs static programs, including real ones: Lua 5.4 passes all sixteen of
//! its own conformance files here byte-for-byte against QEMU. The suite
//! checks twelve programs on every invocation -- the whole conformance suite,
//! the VFS, signals, NEON and C++ exceptions.
//!
//! Guest threads run too, cooperatively on the one wasm stack rather than on
//! Workers: pthreads, mutexes, condition variables and thread-local storage
//! all work, and six threads sharing one SQLite connection give byte-identical
//! output. See [`schedule`].
//!
//! Not yet: real parallelism (a Worker each, with the memory declared shared),
//! `fork`, and runtime translation, so a module must be built with
//! `--exhaustive`.
//!
//! # What JS still does
//!
//! Creates the memory, instantiates both modules against it, wires this
//! module's exports to the guest module's `gex.*` imports, and supplies the
//! two things wasm cannot do for itself: a console and a clock.

// Only meaningful as wasm: everything below assumes it shares a linear memory
// with the guest module beside it. On any other target the crate is empty, so
// `cargo build --workspace` on the development host still works.
#![cfg(target_family = "wasm")]

// `memory.atomic.wait32` and `memory.atomic.notify`, which the kernel lock in
// the shared build is built on. Unstable in `core::arch::wasm32` and nowhere
// else: they are wasm instructions, and the shared build is a nightly build
// already because a module importing a shared memory has to be.
#![cfg_attr(feature = "shared-memory", feature(stdarch_wasm_atomic_wait))]

use core::arch::wasm32 as warch;

use gex_abi::ctx;
use gex_kernel::errno::{Errno, SysResult};
use gex_kernel::file::OpenFile;
use gex_kernel::vfs::{FileHandle, Metadata};
use gex_kernel::net;
use gex_kernel::{GuestMem, Kernel, SyscallOutcome, ThreadState};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
pub mod gl;
pub mod virgl;

use std::sync::Arc;

// The two things wasm cannot do for itself. Everything else about a syscall
// happens in this module.
#[link(wasm_import_module = "gex_env")]
extern "C" {
    /// Write `len` bytes at `ptr` -- an address in the shared memory -- to the
    /// host's console for `fd`.
    fn write(fd: i32, ptr: u32, len: u32);
    /// Nanoseconds since an arbitrary epoch.
    fn now() -> i64;

    // The network. A page cannot open a socket, so every one of these is a
    // question for JS, which has a Wisp connection to a proxy. All are
    // non-blocking: the waiting is the kernel's job, because the kernel is
    // what can park a guest thread, and JS blocking here would take the
    // page's only wasm stack with it.
    //
    // A stream is named by a small integer JS hands out. Zero is never a
    // stream, so it can mean failure.
    fn net_open() -> i32;
    fn net_connect(id: i32, host: u32, host_len: u32, port: i32);
    /// 0 connecting, 1 open, 2 closed, 3 refused.
    fn net_status(id: i32) -> i32;
    /// Bytes taken, or -1.
    fn net_send(id: i32, ptr: u32, len: u32) -> i32;
    /// Bytes delivered, 0 at end of stream, -1 when nothing has arrived yet.
    fn net_recv(id: i32, ptr: u32, len: u32) -> i32;
    /// Whether `net_recv` would answer without waiting.
    fn net_avail(id: i32) -> i32;
    fn net_close(id: i32);
}

/// Whether the page gave the shim a network at all.
static mut HAVE_NET: bool = false;

/// Say that the page has a Wisp connection behind the `net_*` imports.
///
/// Off by default: a page with no proxy configured should have its guest see
/// a machine with no network, which fails cleanly, rather than one whose
/// every connection hangs.
#[no_mangle]
pub extern "C" fn gex_set_network(on: i32) {
    unsafe { HAVE_NET = on != 0 };
    // Only when there is a kernel to tell. A Worker that has instantiated the
    // shim but not yet been given a process -- everything between `attach`
    // and `gex_adopt_child` -- has none, and every kernel built after this
    // reads the flag on its way up anyway.
    if gex_has_host() != 0 {
        with_host(|h| {
            h.kernel.net =
                unsafe { HAVE_NET }.then(|| Arc::new(WispNet) as Arc<dyn net::NetStack>);
        });
    }
}

/// One Wisp stream, as the kernel's [`net::Transport`].
#[derive(Debug)]
struct WispStream {
    id: Cell<i32>,
}

// Single-threaded by construction, like everything else here.
unsafe impl Send for WispStream {}
unsafe impl Sync for WispStream {}

impl net::Transport for WispStream {
    fn connect(&self, host: &str, port: u16) -> Result<(), Errno> {
        let id = unsafe { net_open() };
        if id <= 0 {
            return Err(Errno::ENETUNREACH);
        }
        self.id.set(id);
        unsafe { net_connect(id, host.as_ptr() as u32, host.len() as u32, port as i32) };
        Ok(())
    }

    fn status(&self) -> net::Status {
        match self.id.get() {
            0 => net::Status::Closed,
            id => match unsafe { net_status(id) } {
                0 => net::Status::Connecting,
                1 => net::Status::Open,
                3 => net::Status::Refused,
                _ => net::Status::Closed,
            },
        }
    }

    fn send(&self, data: &[u8]) -> Result<usize, Errno> {
        let id = self.id.get();
        if id == 0 {
            return Err(Errno::ENOTCONN);
        }
        match unsafe { net_send(id, data.as_ptr() as u32, data.len() as u32) } {
            n if n >= 0 => Ok(n as usize),
            _ => Err(Errno::EPIPE),
        }
    }

    fn recv(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let id = self.id.get();
        if id == 0 {
            return Ok(0);
        }
        match unsafe { net_recv(id, buf.as_mut_ptr() as u32, buf.len() as u32) } {
            -1 => Err(Errno::EAGAIN),
            n if n >= 0 => Ok(n as usize),
            _ => Err(Errno::ECONNRESET),
        }
    }

    fn has_data(&self) -> bool {
        let id = self.id.get();
        id != 0 && unsafe { net_avail(id) } != 0
    }

    fn close(&self) {
        let id = self.id.replace(0);
        if id != 0 {
            unsafe { net_close(id) };
        }
    }
}

/// Where the browser's connections come from.
#[derive(Debug)]
struct WispNet;

unsafe impl Send for WispNet {}
unsafe impl Sync for WispNet {}

impl net::NetStack for WispNet {
    fn tcp(&self) -> Result<Arc<dyn net::Transport>, Errno> {
        Ok(Arc::new(WispStream { id: Cell::new(0) }))
    }
}

// The global kernel: the process table, in a module of its own.
//
// Every process is a Worker with its own memory, so no two of them can share
// a `ProcTable` the way two threads do -- an `Arc` is an address, and an
// address means a memory. The table lives instead in a second wasm module
// (`shims/kernel`) that every Worker instantiates against one shared memory,
// so its `Vec<Proc>` is literally the same bytes for all of them.
//
// Reached by *calling*, which is what these imports are. JS wires them to
// that module's exports; a host with no global kernel wires `k_present` to
// zero and the rest to stubs, and the run keeps the table it always had, in
// its own memory, behaving exactly as it did before this existed.
#[link(wasm_import_module = "gex_k")]
extern "C" {
    /// Is there a global kernel behind these calls at all?
    fn k_present() -> i32;
    /// The wake counter in the shared memory: read it, bump it, sleep on it.
    /// See `k_wake` in the kernel module for why it lives over there.
    fn k_wake_seq() -> i32;
    fn k_wake();
    fn k_park(seq: i32, ns: i64) -> i32;
    fn k_spawn(ppid: i32) -> i32;
    fn k_exit(pid: i32, status: i32);
    fn k_reap(pid: i32);
    /// pid in the high 32 bits, wait status in the low 32; zero for none.
    fn k_wait(ppid: i32, want: i32) -> i64;
    fn k_wait_ready(ppid: i32, want: i32) -> i32;
    fn k_has_children(ppid: i32) -> i32;
    fn k_kill(caller: i32, target: i32, signo: u32) -> i32;
    fn k_take_signals(pid: i32) -> u64;
    fn k_has_signals(pid: i32) -> i32;
    fn k_ppid(pid: i32) -> i32;
    fn k_pgid(caller: i32, of: i32) -> i32;
    fn k_sid(caller: i32, of: i32) -> i32;
    fn k_setsid(pid: i32) -> i32;
    fn k_setpgid(caller: i32, target: i32, pgid: i32) -> i32;
    fn k_request_halt(code: i32);
    fn k_halt_code() -> i32;

    // The filesystem. Every address below is in *this* process's memory; the
    // other side copies across with `memory.copy`, which is the only way it
    // can reach a memory it does not own. See `gex_kernel::vfs::remote` for
    // the two layouts that cross.
    fn k_fs_open(path: u32, path_len: u32, flags: u64, mode: u32) -> i32;
    fn k_fs_close(handle: i32);
    fn k_fs_read(handle: i32, offset: u64, at: u32, len: u32) -> i32;
    fn k_fs_write(handle: i32, offset: u64, at: u32, len: u32) -> i32;
    fn k_fs_meta(handle: i32, at: u32) -> i32;
    fn k_fs_truncate(handle: i32, len: u64) -> i32;
    fn k_fs_read_dir(handle: i32, at: u32, cap: u32) -> i32;
    fn k_fs_path_meta(path: u32, path_len: u32, at: u32) -> i32;
    fn k_fs_read_link(path: u32, path_len: u32, at: u32, cap: u32) -> i32;
    fn k_fs_unlink(path: u32, path_len: u32) -> i32;
    fn k_fs_mkdir(path: u32, path_len: u32, mode: u32) -> i32;
    fn k_fs_rmdir(path: u32, path_len: u32) -> i32;
    fn k_fs_rename(from: u32, from_len: u32, to: u32, to_len: u32) -> i32;
    fn k_fs_chmod(path: u32, path_len: u32, mode: u32) -> i32;
    fn k_fs_symlink(target: u32, target_len: u32, path: u32, path_len: u32) -> i32;
    fn k_fs_link(from: u32, from_len: u32, to: u32, to_len: u32) -> i32;
    fn k_fs_mksocket(path: u32, path_len: u32) -> i32;
    fn k_fs_add(path: u32, path_len: u32, at: u32, len: u32) -> i32;
    /// The system-wide map from a page of a mapped object to the dispatch
    /// slots its code was translated into. Zero means nobody has.
    ///
    /// In the kernel rather than here because a shim's statics are one
    /// Worker's, and this has to be every Worker's: the whole point is that
    /// the second process to run `ls` finds what the first one did.
    fn k_slot_get(page: u64, key: u64) -> u32;
    fn k_slot_put(page: u64, key: u64, base: u32);
    /// Translated modules, once for the whole system. Knowing which slots a
    /// page's code is in does not let a Worker reach it -- a dispatch table
    /// belongs to a module instance -- so the module itself has to be
    /// shareable too, and between two wasm modules that means bytes.
    fn k_mod_store(ptr: u32, len: u32, slots: u64, dirs_ptr: u32, dirs_len: u32) -> u32;
    fn k_mod_claim(page: u64, key: u64, id: u32);
    fn k_mod_for(page: u64, key: u64) -> u32;
    fn k_mod_slots(id: u32) -> u64;
    fn k_mod_len(id: u32) -> u32;
    fn k_mod_dirs_len(id: u32) -> u32;
    fn k_mod_read(id: u32, out: u32, cap: u32) -> u32;
    fn k_mod_dirs_read(id: u32, out: u32, cap: u32) -> u32;
    /// Has some process already filled the shared root? Only the first
    /// should: the rest would unpack the same rootfs over the top of it.
    fn k_place(key: u64, len: u64) -> u64;
    fn k_place_set(key: u64, base: u64, len: u64);
    fn k_fs_populated() -> i32;
    fn k_fs_populate_done();

    // Pipes. A shell pipeline is two processes and a pipe, and apt is a
    // process and four method workers on pairs of them; with a Worker per
    // process the other end is always in another memory.
    fn k_pipe_new() -> i32;
    fn k_pipe_open_end(id: i32, write: i32);
    fn k_pipe_close_end(id: i32, write: i32);
    fn k_pipe_read(id: i32, at: u32, len: u32) -> i32;
    fn k_pipe_write(id: i32, at: u32, len: u32) -> i32;
    fn k_pipe_readable(id: i32) -> i32;
    fn k_pipe_writable(id: i32) -> i32;
    fn k_pipe_buffered(id: i32) -> i32;

    // Unix sockets. An X server binds a name and its client connects to it,
    // and with a Worker per process those are two memories -- so the name has
    // to be somewhere neither of them owns.
    fn k_sock_bind(path: u32, path_len: u32) -> i32;
    fn k_sock_connect(path: u32, path_len: u32) -> i32;
    fn k_sock_accept(listener: i32) -> i32;
    fn k_sock_pending(listener: i32) -> i32;
    fn k_sock_read(id: i32, at: u32, len: u32) -> i32;
    fn k_sock_write(id: i32, at: u32, len: u32) -> i32;
    fn k_sock_readable(id: i32) -> i32;
    fn k_sock_writable(id: i32) -> i32;
    fn k_sock_available(id: i32) -> i32;
    fn k_sock_close(id: i32);
    fn k_sock_shutdown(id: i32);
    fn k_sock_exists(path: u32, path_len: u32) -> i32;

    // The GPU's client numbers. One renderer, one canvas, and every drawing
    // process a client of it -- so the number cannot come from a table each
    // process keeps for itself.
    fn k_virgl_take() -> i32;
    fn k_virgl_release(id: i32);
}

/// Claim a renderer session number, or -1 when there is no global kernel to
/// claim it from -- in which case the caller falls back to its own table,
/// which is right when there is only one process.
pub(crate) fn virgl_take_session() -> i32 {
    if unsafe { k_present() } == 0 {
        return -1;
    }
    unsafe { k_virgl_take() }
}

pub(crate) fn virgl_release_session(id: i32) {
    if unsafe { k_present() } != 0 {
        unsafe { k_virgl_release(id) };
    }
}

// A wasm import is `unsafe`, and `Remote` holds safe fn pointers, so each one
// needs a wrapper. Written out rather than generated: a macro here would save
// thirty lines and cost the ability to read the surface at a glance, which is
// the one thing this list is for.
extern "C" fn r_spawn(ppid: i32) -> i32 {
    unsafe { k_spawn(ppid) }
}
extern "C" fn r_exit(pid: i32, status: i32) {
    unsafe { k_exit(pid, status) }
}
extern "C" fn r_reap(pid: i32) {
    unsafe { k_reap(pid) }
}
extern "C" fn r_wait(ppid: i32, want: i32) -> i64 {
    unsafe { k_wait(ppid, want) }
}
extern "C" fn r_wait_ready(ppid: i32, want: i32) -> i32 {
    unsafe { k_wait_ready(ppid, want) }
}
extern "C" fn r_has_children(ppid: i32) -> i32 {
    unsafe { k_has_children(ppid) }
}
extern "C" fn r_kill(caller: i32, target: i32, signo: u32) -> i32 {
    unsafe { k_kill(caller, target, signo) }
}
extern "C" fn r_take_signals(pid: i32) -> u64 {
    unsafe { k_take_signals(pid) }
}
extern "C" fn r_has_signals(pid: i32) -> i32 {
    unsafe { k_has_signals(pid) }
}
extern "C" fn r_ppid(pid: i32) -> i32 {
    unsafe { k_ppid(pid) }
}
extern "C" fn r_pgid(caller: i32, of: i32) -> i32 {
    unsafe { k_pgid(caller, of) }
}
extern "C" fn r_sid(caller: i32, of: i32) -> i32 {
    unsafe { k_sid(caller, of) }
}
extern "C" fn r_setsid(pid: i32) -> i32 {
    unsafe { k_setsid(pid) }
}
extern "C" fn r_setpgid(caller: i32, target: i32, pgid: i32) -> i32 {
    unsafe { k_setpgid(caller, target, pgid) }
}
extern "C" fn r_request_halt(code: i32) {
    unsafe { k_request_halt(code) }
}
extern "C" fn r_halt_code() -> i32 {
    unsafe { k_halt_code() }
}

/// The process table every process shares, if the host gave us one.
///
/// `None` is a run with one process in it -- a page that loads a program and
/// never forks, or a host that has not wired the second module up. Nothing
/// about that case changes.
fn global_procs() -> Option<Arc<gex_kernel::proc::ProcTable>> {
    if unsafe { k_present() } == 0 {
        return None;
    }
    Some(Arc::new(gex_kernel::proc::ProcTable::shared(
        gex_kernel::proc::Remote {
            spawn: r_spawn,
            exit: r_exit,
            reap: r_reap,
            wait: r_wait,
            wait_ready: r_wait_ready,
            has_children: r_has_children,
            kill: r_kill,
            take_signals: r_take_signals,
            has_signals: r_has_signals,
            ppid: r_ppid,
            pgid: r_pgid,
            sid: r_sid,
            setsid: r_setsid,
            setpgid: r_setpgid,
            request_halt: r_request_halt,
            halt_code: r_halt_code,
        },
    )))
}

extern "C" fn rf_open(path: u32, path_len: u32, flags: u64, mode: u32) -> i32 {
    unsafe { k_fs_open(path, path_len, flags, mode) }
}
extern "C" fn rf_close(handle: i32) {
    unsafe { k_fs_close(handle) }
}
extern "C" fn rf_read(handle: i32, offset: u64, at: u32, len: u32) -> i32 {
    unsafe { k_fs_read(handle, offset, at, len) }
}
extern "C" fn rf_write(handle: i32, offset: u64, at: u32, len: u32) -> i32 {
    unsafe { k_fs_write(handle, offset, at, len) }
}
extern "C" fn rf_meta(handle: i32, at: u32) -> i32 {
    unsafe { k_fs_meta(handle, at) }
}
extern "C" fn rf_truncate(handle: i32, len: u64) -> i32 {
    unsafe { k_fs_truncate(handle, len) }
}
extern "C" fn rf_read_dir(handle: i32, at: u32, cap: u32) -> i32 {
    unsafe { k_fs_read_dir(handle, at, cap) }
}
extern "C" fn rf_path_meta(path: u32, path_len: u32, at: u32) -> i32 {
    unsafe { k_fs_path_meta(path, path_len, at) }
}
extern "C" fn rf_read_link(path: u32, path_len: u32, at: u32, cap: u32) -> i32 {
    unsafe { k_fs_read_link(path, path_len, at, cap) }
}
extern "C" fn rf_unlink(path: u32, path_len: u32) -> i32 {
    unsafe { k_fs_unlink(path, path_len) }
}
extern "C" fn rf_mkdir(path: u32, path_len: u32, mode: u32) -> i32 {
    unsafe { k_fs_mkdir(path, path_len, mode) }
}
extern "C" fn rf_rmdir(path: u32, path_len: u32) -> i32 {
    unsafe { k_fs_rmdir(path, path_len) }
}
extern "C" fn rf_rename(from: u32, from_len: u32, to: u32, to_len: u32) -> i32 {
    unsafe { k_fs_rename(from, from_len, to, to_len) }
}
extern "C" fn rf_chmod(path: u32, path_len: u32, mode: u32) -> i32 {
    unsafe { k_fs_chmod(path, path_len, mode) }
}
extern "C" fn rf_symlink(target: u32, target_len: u32, path: u32, path_len: u32) -> i32 {
    unsafe { k_fs_symlink(target, target_len, path, path_len) }
}
extern "C" fn rf_link(from: u32, from_len: u32, to: u32, to_len: u32) -> i32 {
    unsafe { k_fs_link(from, from_len, to, to_len) }
}
extern "C" fn rf_mksocket(path: u32, path_len: u32) -> i32 {
    unsafe { k_fs_mksocket(path, path_len) }
}

extern "C" fn rp_new() -> i32 {
    unsafe { k_pipe_new() }
}
extern "C" fn rp_open_end(id: i32, write: i32) {
    unsafe { k_pipe_open_end(id, write) }
}
extern "C" fn rp_close_end(id: i32, write: i32) {
    unsafe { k_pipe_close_end(id, write) }
}
extern "C" fn rp_read(id: i32, at: u32, len: u32) -> i32 {
    unsafe { k_pipe_read(id, at, len) }
}
extern "C" fn rp_write(id: i32, at: u32, len: u32) -> i32 {
    unsafe { k_pipe_write(id, at, len) }
}
extern "C" fn rp_readable(id: i32) -> i32 {
    unsafe { k_pipe_readable(id) }
}
extern "C" fn rp_writable(id: i32) -> i32 {
    unsafe { k_pipe_writable(id) }
}
extern "C" fn rp_buffered(id: i32) -> i32 {
    unsafe { k_pipe_buffered(id) }
}

/// Where a pipe's buffer goes, when there is a global kernel to put it in.
fn global_pipes() -> Option<gex_kernel::pipe::Remote> {
    if unsafe { k_present() } == 0 {
        return None;
    }
    Some(gex_kernel::pipe::Remote {
        new: rp_new,
        open_end: rp_open_end,
        close_end: rp_close_end,
        read: rp_read,
        write: rp_write,
        readable: rp_readable,
        writable: rp_writable,
        buffered: rp_buffered,
    })
}

extern "C" fn rs_bind(path: u32, path_len: u32) -> i32 {
    unsafe { k_sock_bind(path, path_len) }
}
extern "C" fn rs_connect(path: u32, path_len: u32) -> i32 {
    unsafe { k_sock_connect(path, path_len) }
}
extern "C" fn rs_accept(listener: i32) -> i32 {
    unsafe { k_sock_accept(listener) }
}
extern "C" fn rs_pending(listener: i32) -> i32 {
    unsafe { k_sock_pending(listener) }
}
extern "C" fn rs_read(id: i32, at: u32, len: u32) -> i32 {
    unsafe { k_sock_read(id, at, len) }
}
extern "C" fn rs_write(id: i32, at: u32, len: u32) -> i32 {
    unsafe { k_sock_write(id, at, len) }
}
extern "C" fn rs_readable(id: i32) -> i32 {
    unsafe { k_sock_readable(id) }
}
extern "C" fn rs_writable(id: i32) -> i32 {
    unsafe { k_sock_writable(id) }
}
extern "C" fn rs_available(id: i32) -> i32 {
    unsafe { k_sock_available(id) }
}
extern "C" fn rs_close(id: i32) {
    unsafe { k_sock_close(id) }
}
extern "C" fn rs_shutdown(id: i32) {
    unsafe { k_sock_shutdown(id) }
}
extern "C" fn rs_exists(path: u32, path_len: u32) -> i32 {
    unsafe { k_sock_exists(path, path_len) }
}

/// Where a unix socket's two ends live, when there is a global kernel to put
/// them in.
fn global_sockets() -> Option<gex_kernel::unix::Remote> {
    if unsafe { k_present() } == 0 {
        return None;
    }
    Some(gex_kernel::unix::Remote {
        bind: rs_bind,
        connect: rs_connect,
        accept: rs_accept,
        pending: rs_pending,
        read: rs_read,
        write: rs_write,
        readable: rs_readable,
        writable: rs_writable,
        available: rs_available,
        close: rs_close,
        shutdown: rs_shutdown,
        exists: rs_exists,
    })
}

/// The root every process shares, if the host gave us a global kernel.
///
/// Mounted at `/` in place of this process's own `TmpFs`. Without it a forked
/// child arrives with an empty root and its `execve` cannot find the program
/// it was forked to run -- under the switcher the child got its parent's
/// whole address space and the question never came up.
fn global_root() -> Option<Arc<gex_kernel::vfs::RemoteFs>> {
    if unsafe { k_present() } == 0 {
        return None;
    }
    Some(Arc::new(gex_kernel::vfs::RemoteFs::new(
        gex_kernel::vfs::remote::Remote {
            open: rf_open,
            close: rf_close,
            read: rf_read,
            write: rf_write,
            meta: rf_meta,
            truncate: rf_truncate,
            read_dir: rf_read_dir,
            path_meta: rf_path_meta,
            read_link: rf_read_link,
            unlink: rf_unlink,
            mkdir: rf_mkdir,
            rmdir: rf_rmdir,
            rename: rf_rename,
            chmod: rf_chmod,
            symlink: rf_symlink,
            link: rf_link,
            mksocket: rf_mksocket,
        },
    )))
}

/// Put the page's bundle into the shared root, once for the whole system.
///
/// The first process to load fills it; every later one finds it filled and
/// does nothing, which matters because the bundle is the same two hundred
/// megabytes of rootfs each time and unpacking it per process would be both
/// slow and pointless.
fn install_bundle_remote() {
    // The files went in as they arrived -- see `gex_add_file` -- so all that
    // is left is to say the bundle is complete. `FILES` is empty on this path
    // and clearing it is only tidiness.
    FILES.0.borrow_mut().clear();
    if unsafe { k_fs_populated() } == 0 {
        unsafe { k_fs_populate_done() };
    }
}

/// Back-edges a guest may take between safepoint calls, matching the native
/// host's interval.
const SAFEPOINT_INTERVAL: u32 = 1024;

/// The host's clock, in the shape `gex-kernel` wants.
fn browser_clock() -> (u64, u32) {
    let ns = unsafe { now() } as u64;
    (ns / 1_000_000_000, (ns % 1_000_000_000) as u32)
}

/// The guest's standard input, as a queue the page appends to.
///
/// A page that hands over a fixed buffer up front can run a filter. A page
/// with a terminal in it cannot: the bytes do not exist until someone types
/// them, and a shell asks for them long before that. So this is a queue --
/// `gex_push_stdin` adds to it, a read takes from it, and a read that finds
/// it empty parks the calling thread until it is not (see `Waiting::Read`).
///
/// `eof` is the difference between "nothing yet" and "nothing ever". Without
/// it a shell reading its script from stdin waits forever at the end instead
/// of exiting.
#[derive(Debug, Default)]
struct StdinQueue {
    bytes: RefCell<VecDeque<u8>>,
    eof: Cell<bool>,
}

impl StdinQueue {
    fn push(&self, data: &[u8]) {
        self.bytes.borrow_mut().extend(data.iter().copied());
    }
}

// The browser shim is single-threaded by construction; `SingleThreaded` is
// the wrapper the rest of this file uses to say so.
unsafe impl Send for StdinQueue {}
unsafe impl Sync for StdinQueue {}

impl FileHandle for StdinQueue {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        let mut q = self.bytes.borrow_mut();
        let n = buf.len().min(q.len());
        for (i, b) in q.drain(..n).enumerate() {
            buf[i] = b;
        }
        Ok(n as u64)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> SysResult {
        Err(Errno::EBADF)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: gex_kernel::vfs::FileType::CharDevice,
            size: 0,
            mode: 0o600,
            ino: 2,
            mtime: 0,
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// A terminal, when the page says there is one. musl line-buffers stdout
    /// on a tty, and a shell will not print a prompt without one.
    fn is_tty(&self) -> bool {
        unsafe { STDIN_TTY }
    }

    /// Nothing typed yet is a wait, unless the page has said there will be no
    /// more.
    fn poll_readable(&self) -> bool {
        self.eof.get() || !self.bytes.borrow().is_empty()
    }
}

/// Mount `/dev`, and bind the virtio-gpu socket if the page brought a
/// renderer. Returns the device, which the `Host` holds.
///
/// A function because every kernel this shim builds needs it, and there is
/// more than one: the process `gex_load` places, and every child
/// `gex_adopt_child` adopts. A child given only the root filesystem has no
/// `/dev/null` to redirect into and no GPU to draw on, and the gears demo is
/// a shell script that wants both in its first two lines.
fn mount_devices(kernel: &mut Kernel) -> Option<crate::virgl::VirglDevice> {
    let mut devfs = gex_kernel::vfs::DevFs::new();
    if crate::gl::have_gl() {
        devfs.add("gex-gl", crate::gl::GlDevice::new());
    }
    kernel.vfs.mount("/dev", Arc::new(devfs));
    // Only when the page brought one: a guest that connects to a socket
    // nobody serves is worse off than one that finds no socket, because
    // Mesa's winsys does not fall back -- it writes its handshake into the
    // dead descriptor and blocks reading a reply that cannot come.
    crate::virgl::have_virgl().then(|| {
        let device = crate::virgl::VirglDevice::new();
        kernel.socket_names.bind_host(
            crate::virgl::DEFAULT_SOCKET,
            Arc::new(device.clone()) as Arc<dyn gex_kernel::unix::HostListener>,
        );
        device
    })
}

/// Mount `/proc`. Returns the source it reads from, which the `Host` holds
/// and the scheduler keeps up to date.
///
/// It matters more than it looks: a program that cannot open
/// `/proc/self/maps` gets a null `FILE*` back, and the next `fgets` calls
/// through it. That surfaces as a jump to address zero -- a dispatch miss at
/// pc=0 -- with nothing to say the filesystem was the cause.
fn mount_procfs(kernel: &mut Kernel) -> Arc<gex_kernel::KernelProcSource> {
    let proc_state: Arc<gex_kernel::KernelProcSource> = Arc::new(gex_kernel::KernelProcSource(
        std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
    ));
    kernel.vfs.mount(
        "/proc",
        Arc::new(gex_kernel::vfs::ProcFs::new(proc_state.clone())),
    );
    proc_state
}

/// Give a kernel the three standard descriptors, pointed at the page.
///
/// `Kernel::new` installs `gex-kernel`'s own, which write through `std::io`
/// and on `wasm32-unknown-unknown` go nowhere at all. Every kernel this shim
/// builds has to have them replaced, and there is more than one such kernel:
/// the process `gex_load` places, and every child `gex_adopt_child` adopts.
/// A forked child whose output vanished is exactly what happened when only
/// the first of those did it -- the guest ran correctly and said nothing.
fn install_console(kernel: &mut Kernel) {
    let _ = kernel.fds.install(
        0,
        OpenFile::new(
            Arc::clone(&*STDIN) as Arc<dyn FileHandle>,
            "/dev/stdin".to_string(),
            0,
            true,
            false,
        ),
        false,
    );
    for fd in [1, 2] {
        let f = OpenFile::new(
            Arc::new(Console { fd }),
            if fd == 1 {
                "/dev/stdout"
            } else {
                "/dev/stderr"
            }
            .to_string(),
            0,
            false,
            true,
        );
        let _ = kernel.fds.install(fd, f, false);
    }
}

/// stdout and stderr, as the host's console.
///
/// `gex-kernel`'s own `StdStream` writes through `std::io`, which on
/// `wasm32-unknown-unknown` goes nowhere. The `FileHandle` trait is the seam:
/// the kernel does not care what is behind a descriptor.
#[derive(Debug)]
struct Console {
    fd: i32,
}

impl FileHandle for Console {
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> SysResult {
        Err(Errno::EBADF)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        // The buffer is already in the shared memory the host can see, so its
        // address is all that has to cross.
        unsafe { write(self.fd, buf.as_ptr() as u32, buf.len() as u32) };
        Ok(buf.len() as u64)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: gex_kernel::vfs::FileType::Fifo,
            size: 0,
            mode: 0o600,
            ino: 1,
            mtime: 0,
        })
    }

    /// A console is not seekable, so writes must not advance an offset.
    fn is_stream(&self) -> bool {
        true
    }

    /// Not a terminal. A page's console looks like one, but saying so makes
    /// musl line-buffer stdout, and then a program's bytes interleave
    /// differently from the same program's under a pipe -- which is what
    /// every comparison in the suite runs the reference against.
    fn is_tty(&self) -> bool {
        false
    }
}

// The shim's own heap.
//
// Rust's default allocator grows the memory when it runs out, and this memory
// cannot grow: it is the guest's, fixed at its maximum so the guest's stack
// lands where the ABI says. So the heap is a fixed region instead --
// everything between the end of this module's data and the end of the address
// space, which is the rest of the two gigabytes the guest never touches.
//
// It has to be a *good* allocator, which only became obvious once runtime
// translation started running inside the shim. A first-fit linked list walks
// its free list on every allocation and the translator allocates constantly:
// lifting tcc's largest source took 15.2 seconds that way, against 0.3 with a
// bump allocator that never freed. Talc bins its free lists, so allocation
// does not degrade as the heap fragments, and it still frees.
// Two of them, because the two builds differ in exactly one way that matters
// here. Against a plain memory the module is single-threaded and a cell is
// enough. Against a *shared* memory each of a process's threads can be its own
// Worker and be inside this module at the same time, so the heap needs a real
// lock -- and `TalcSyncCell` says so itself, refusing to compile once the
// target has atomics rather than being quietly unsound.
#[cfg(not(feature = "shared-memory"))]
type Heap = talc::cell::TalcSyncCell<ShimRegion, talc::wasm::WasmBinning>;
#[cfg(not(feature = "shared-memory"))]
const HEAP: Heap = talc::cell::TalcSyncCell::new_wasm(ShimRegion);

#[cfg(feature = "shared-memory")]
type Heap =
    talc::sync::TalcLock<spinning_top::RawSpinlock, ShimRegion, talc::wasm::WasmBinning>;
#[cfg(feature = "shared-memory")]
const HEAP: Heap = talc::sync::TalcLock::new(ShimRegion);

#[global_allocator]
static ALLOC: Counting = Counting(HEAP, std::sync::atomic::AtomicUsize::new(0));

/// The heap, plus how much of it is out on loan.
///
/// Talc keeps this figure itself, behind its `counters` feature -- but only
/// on the plain `TalcCell`, and the shim's allocator is a `TalcSyncCell`,
/// which wraps one and does not pass the accessor through. Counting here is
/// less code than working around that, and it counts the same thing.
///
/// The total is worth having because the heap is *fixed*: `__heap_base` to
/// the top of memory, about 2039 MiB, with no growing, because the memory
/// belongs to the guest and moving its top would move where the guest's stack
/// lands. Running out has no symptom of its own -- the page sees a fault at
/// whatever the guest touched next -- so the only way to know is to ask.
///
/// Without `heap-counters` this is the allocator and nothing else: the
/// counting arms compile away and what is left is a delegating newtype.
struct Counting(Heap, std::sync::atomic::AtomicUsize);

// SAFETY: both inner allocators are `Sync` on their intended target, and the
// counter is atomic.
unsafe impl Sync for Counting {}

unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        let p = unsafe { self.0.alloc(l) };
        // Rust turns a null into `handle_alloc_error`, which aborts, which in
        // wasm is a bare `unreachable` with nothing said. That is the least
        // debuggable failure this shim has: the heap is a *fixed* region --
        // the memory belongs to the guest and cannot grow -- so running out
        // is a real and reachable state, and it used to surface as a guest
        // that mysteriously stopped. Say it before the abort.
        if p.is_null() {
            // With where the arena had got to and how big the memory is.
            //
            // "Out of heap" stopped being the whole story when the heap
            // started taking spans from the growing arena: the request can
            // fail because the arena has reached its ceiling, or because the
            // memory would not grow, and those want different fixes. The
            // numbers say which.
            trace_line(&format!(
                "gex: out of shim heap asking for {} bytes (align {}); {} of {} MiB in use; \
                 arena at {:#x} of {:#x}, memory {} MiB\n",
                l.size(),
                l.align(),
                gex_heap_used_kib() / 1024,
                gex_heap_total_kib() / 1024,
                ARENA_NEXT.load(std::sync::atomic::Ordering::Relaxed),
                gex_abi::MMAP_TOP,
                warch::memory_size(0) as u64 * 65536 / (1024 * 1024),
            ));
        }
        #[cfg(feature = "heap-counters")]
        if !p.is_null() {
            self.1.fetch_add(l.size(), std::sync::atomic::Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        #[cfg(feature = "heap-counters")]
        self.1.fetch_sub(l.size(), std::sync::atomic::Ordering::Relaxed);
        unsafe { self.0.dealloc(p, l) }
    }

    // Realloc is forwarded rather than left to the default, which would
    // allocate-copy-free: talc grows in place where it can, and the lifter
    // pushes onto a great many vectors.
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, new: usize) -> *mut u8 {
        let q = unsafe { self.0.realloc(p, l, new) };
        #[cfg(feature = "heap-counters")]
        if !q.is_null() {
            if new >= l.size() {
                self.1.fetch_add(new - l.size(), std::sync::atomic::Ordering::Relaxed);
            } else {
                self.1.fetch_sub(l.size() - new, std::sync::atomic::Ordering::Relaxed);
            }
        }
        q
    }
}

extern "C" {
    static __heap_base: u8;
}

/// Hands the allocator everything above this module's data, once.
///
/// A linker symbol's address is not a constant, so the region cannot be named
/// in the allocator's initialiser; it is claimed the first time anything is
/// allocated instead.
#[derive(Debug)]
struct ShimRegion;

/// Make the memory cover everything below `end`.
///
/// A process's memory starts at what its fixed low layout needs -- contexts,
/// image, break, stack -- and grows from there. Both things that grow, the
/// guest's mmap arena and this module's heap, come through here, so the
/// memory is never larger than what a run has actually asked for.
///
/// Returns false when the engine will not give any more, which is a real
/// answer: `mmap` reports ENOMEM and the allocator fails, rather than the
/// process being killed by a trap on an address that was never backed.
fn grow_memory_to(end: u64) -> bool {
    const PAGE: u64 = 65536;
    let have = warch::memory_size(0) as u64 * PAGE;
    if end <= have {
        return true;
    }
    // A megabyte at a time past what was asked for. `memory.grow` is not
    // cheap -- the engine may copy -- and a guest that maps a page at a time
    // would otherwise pay for each one.
    let want = (end - have).div_ceil(PAGE).max(16);
    warch::memory_grow(0, want as usize) != usize::MAX
}

/// The arena both the guest and this module take from, in address order.
///
/// One cursor rather than two regions: two would each need a fixed ceiling,
/// and a fixed ceiling is memory a process pays for whether it uses it or
/// not. Anything the guest unmaps goes back to the kernel's own free list;
/// this only ever moves up.
static ARENA_NEXT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Take `len` bytes of arena for this module's heap.
fn arena_take(len: u64) -> Option<u64> {
    let at = ARENA_NEXT.load(std::sync::atomic::Ordering::Relaxed);
    // Above the placement window, never inside it. The window is where
    // shared objects go so that every process has them at the same address,
    // and this cursor is the one thing that could put something else there:
    // the heap and the arena both take from it, and neither is visible to
    // the memory map's own idea of what is free.
    let at = if at == 0 { gex_abi::ARENA_BOTTOM } else { at };
    let end = at.checked_add(len)?;
    if end > gex_abi::MMAP_TOP || !grow_memory_to(end) {
        return None;
    }
    ARENA_NEXT.store(end, std::sync::atomic::Ordering::Relaxed);
    Some(at)
}

/// Make the memory reach `end`, and keep the arena above it.
///
/// For a mapping the guest placed itself -- `MAP_FIXED`, or an address hint
/// the map honoured. The range is the guest's, so nothing else may be handed
/// out below it: this module's heap takes from the same cursor and would
/// otherwise put its bookkeeping inside a mapping the guest is using.
fn arena_back(end: u64) -> bool {
    if !grow_memory_to(end) {
        return false;
    }
    ARENA_NEXT.fetch_max(end, std::sync::atomic::Ordering::Relaxed);
    true
}

/// What the kernel's memory map calls to claim a span of arena.
///
/// The same cursor this module's heap takes from, so the two can never hand
/// out the same bytes. Zero means the memory would not grow.
fn arena_grow(len: u64) -> u64 {
    arena_take(len).unwrap_or(0)
}

// SAFETY: `acquire` claims a region of this module's own linear memory and
// touches no other allocator.
unsafe impl talc::source::Source for ShimRegion {
    fn acquire<B: talc::base::binning::Binning>(
        talc: &mut talc::base::Talc<Self, B>,
        _layout: std::alloc::Layout,
    ) -> Result<(), ()> {
        // A span at a time, from the arena the guest's mappings come from
        // too, and each one grows the memory behind it.
        //
        // It used to claim everything between this module's data and the top
        // of a fixed four-gigabyte memory, once. That is why the memory had
        // to *be* four gigabytes: the ceiling was the allocator's, and a
        // process paid for it before it had allocated a byte.
        const CHUNK: u64 = 16 * 1024 * 1024;
        let len = (_layout.size() as u64 + _layout.align() as u64).max(CHUNK);
        let len = len.next_multiple_of(65536);
        let base = arena_take(len).ok_or(())? as *mut u8;
        // SAFETY: `arena_take` handed this span out to nobody else and grew
        // the memory to cover it.
        unsafe { talc.claim(base, len as usize).ok_or(())? };
        Ok(())
    }
}

/// State wrapped so it can be a `static`: wasm without threads is
/// single-threaded, and this module is only ever entered from JS one call at a
/// time, so there is nothing to race with.
struct SingleThreaded<T>(RefCell<T>);
// SAFETY: see above.
unsafe impl<T> Sync for SingleThreaded<T> {}

/// Why a guest thread is not running.
enum Waiting {
    /// Ready to be picked up.
    Runnable,
    /// Parked on a futex, until it is woken or the deadline passes.
    Futex {
        parked: gex_kernel::futex::Parked,
        deadline: Option<u64>,
    },
    /// Sleeping until this many nanoseconds on the host clock.
    Sleep(u64),
    /// Waiting on a stream with nothing in it: a terminal nobody has typed at
    /// yet. Ready as soon as the handle says a read would return.
    Read(Arc<dyn gex_kernel::vfs::FileHandle>),
    /// Waiting for room in a stream: a pipe nobody has drained yet.
    Write(Arc<dyn gex_kernel::vfs::FileHandle>),
    /// Waiting in `accept` for a client to arrive.
    ///
    /// This used to park as `Runnable`, on the reasoning that the scheduler
    /// would come back round and ask again. It does -- and `finish_parked`
    /// answers EAGAIN when nothing has arrived, which is the right answer for
    /// a *non-blocking* accept and is handed straight to the guest. A server
    /// therefore got EAGAIN from a blocking `accept` and gave up. Nothing
    /// noticed because the only host-served socket was the virtio-gpu
    /// renderer, where the guest connects and never accepts.
    Accept(Arc<dyn gex_kernel::vfs::FileHandle>),
    /// Waiting in `wait4` for a child to finish.
    ///
    /// This used to be impossible: the parent was frozen until the child
    /// exited, so a `wait4` that ran at all was asking about a child whose
    /// status was already recorded. Now that processes take turns, a parent
    /// can reach `wait4` while its child is still running, and has to park
    /// like anything else.
    Child,
    /// Waiting on the network -- a connection coming up, or a `poll` with
    /// nothing ready. Whether it is ready is a question for the operation
    /// itself, which the scheduler holds in `blocked_on`, so this only
    /// carries the deadline.
    Net(Option<u64>),
    /// Finished; the slot stays so a `join` can still find the tid.
    Exited,
    /// Running in another Worker of this process.
    ///
    /// The thread is real and the table entry is real -- `join`, the futexes
    /// and `live_threads` all still want it -- but this Worker must never
    /// pick it up, because another one is already on it. That is the whole
    /// difference between a thread that is a Worker and a thread that is a
    /// turn on somebody else's stack.
    Elsewhere,
}
/// One guest thread.
///
/// Every register it has is in its context in guest memory, so switching to it
/// is a call to the guest module's entry with a different context pointer.
/// There is no stack to save: the shim leaves the guest's wasm stack behind
/// entirely, which is the same thing the native host does for a signal.
struct Thread {
    kstate: ThreadState,
    /// Where to resume, which for a thread that blocked is the instruction
    /// after its `svc`.
    pc: u64,
    waiting: Waiting,
    /// The syscall to finish before it runs again.
    blocked_on: Option<gex_kernel::BlockOp>,
}

/// Runtime translation state.
///
/// The shim can lift and encode a module -- `gex-xlate` is ordinary Rust and
/// compiles to wasm like everything else -- but it cannot *compile* one:
/// nothing in wasm can hand bytes to the engine. So a miss produces the
/// module here, leaves it for JS to instantiate, and comes back.
/// One emitted module, waiting for JavaScript to instantiate it.
struct PendingModule {
    wasm: Vec<u8>,
    /// The pages it is the first to claim, published only once its blocks are
    /// actually in the table.
    dirs: Vec<(u64, u32)>,
    /// How many slots the dispatch table must hold for it.
    slots: u64,
}

struct Jit {
    /// Which guest pages own which slots in the dispatch table.
    page_table: gex_xlate::PageTable,
    /// Whether the module said where it had got to. Without that this would
    /// start handing out slots from zero and quietly overwrite the image
    /// module's own blocks in the table, which is a very confusing way to
    /// find out the module is too old.
    described: bool,
    /// Addresses whose slot now holds real code.
    translated: std::collections::HashSet<u64>,
    /// Every translated module's bytes, in the order they were installed.
    ///
    /// A dispatch table belongs to a module instance, and instances are not
    /// shared between Workers -- so a thread joining this process has an
    /// empty table while the page directory, which *is* shared, already
    /// points at slots. Keeping the modules is what lets a joining Worker
    /// instantiate them into its own tables and catch up. Order matters: each
    /// module's page assignments are the next one's starting point.
    registry: Vec<Vec<u8>>,
    /// Slots that code of a given identity was translated into, for the whole
    /// tab rather than for one process.
    ///
    /// Keyed by the guest address of the page and by what the bytes there
    /// are: the file, the offset within it, and the length of the mapping.
    /// Two processes that map the same library at the same address hash to
    /// the same key and share the slots, so the second one dispatches into
    /// translations the first already paid for.
    ///
    /// Not part of a process's saved state. That is the point -- it outlives
    /// every `fork` and `execve`, which is where the waste was.
    shared: std::collections::HashMap<(u64, u64), u32>,
    /// Shared modules this Worker has already put into its own tables.
    ///
    /// A dispatch table belongs to a module instance, so this describes the
    /// Worker and not the program: it survives `execve` for the same reason
    /// the tables do.
    installed_mods: std::collections::HashSet<u32>,
    /// How the shared-slot lookup went, for the question of why a forked and
    /// exec'd child re-lifts its parent's libraries. Three outcomes: the page
    /// already had a base in *this* process's table, the identity map knew
    /// it, or it did not. See `gex_reuse_stats`.
    reuse_have: u64,
    reuse_hit: u64,
    reuse_miss: u64,
    reuse_noident: u64,
    /// Modules installed from the shared store instead of being lifted.
    reuse_served: u64,
    /// Guest addresses that are `memcpy` and friends, to be served by a wasm
    /// bulk-memory instruction rather than translated.
    ///
    /// Filled from an object's symbol table and relocations the first time a
    /// dispatch miss lands anywhere in that object -- not when it is mapped,
    /// because a mapping is not evidence anything in it will run, and not
    /// from the recompiler, because the runtime translator needs the same
    /// answers and the page may never have asked for bulk recompilation.
    intrinsics: std::collections::HashMap<u64, gex_xlate::Intrinsic>,
    /// How many times each object has been through [`note_intrinsics`], so a
    /// library is parsed once when the answer is settled and a bounded number
    /// of times while it is not.
    intrinsics_scanned: std::collections::HashMap<String, u32>,
    /// Objects the guest has mapped that have not been recompiled yet, and
    /// how many dispatch misses have landed in each.
    ///
    /// Translating an object the moment it is mapped is the wrong policy
    /// even though it is the right mechanism. Measured on the gears demo:
    /// 1,487,251 blocks against 352,080 for the demand path, to reach the
    /// same two places later, because whole-object translation pays for
    /// every function in Mesa and libc whether or not the run calls one.
    /// The X-server test goes the other way -- 30 seconds to 12 -- because
    /// there the same libraries stay in use.
    ///
    /// So an object waits until it proves itself. A library the run touches
    /// twice stays on the demand path; libc is entered thousands of times
    /// before anything else happens and is recompiled almost at once.
    candidates: Vec<(gex_kernel::CodeMap, u32)>,
    /// Modules JS has yet to instantiate, oldest first.
    ///
    /// A queue rather than one, because statically recompiling a whole
    /// object produces as many modules as it needs: a wasm module holds at
    /// most a million functions and gex emits one per block, so libc is
    /// several and libz3 is many. They must be installed in the order they
    /// were emitted, since each one's page assignments are the next one's
    /// starting point.
    pending: std::collections::VecDeque<PendingModule>,
    blocks: u64,
    compiles: u64,
    /// Nanoseconds spent lifting and encoding, which is the shim's own share
    /// of a miss -- the engine's compile of what comes out is JS's.
    nanos: u64,
}

struct Host {
    kernel: Kernel,
    /// The virtio-gpu renderer, when the page brought one. Held here rather
    /// than in the kernel because the scheduler is what pumps it, and the
    /// scheduler is the one place every thread passes through on its way to
    /// waiting for a reply.
    virgl: Option<crate::virgl::VirglDevice>,
    /// The root filesystem, kept so files can be added before the guest runs.
    root: Arc<gex_kernel::vfs::TmpFs>,
    /// The snapshot `/proc` reads from, refreshed before each syscall that
    /// could look at it.
    proc_state: Arc<gex_kernel::KernelProcSource>,
    /// Every guest thread, the running one included.
    threads: Vec<Thread>,
    /// Index into `threads` of the one on the wasm stack right now.

    exit: Option<i32>,
    /// Set alongside `resume` when the scheduler switched threads.
    /// Why the scheduler gave the stack back to JS, when it was not an exit.
    /// 1: every thread is waiting for input the page must supply. 2: every
    /// thread is waiting on the network, and the page's event loop is what
    /// makes the bytes arrive. Either way the page is expected to call
    /// `gex_wake`; the difference is what it should do first.
    jit: Jit,
}

// Against a plain memory the shim is single-threaded and one Worker owns
// everything, so a thread-local is exactly right and costs nothing.
#[cfg(not(feature = "shared-memory"))]
thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

/// The kernel, shared by every Worker running a thread of this process.
///
/// A thread-local is wrong here, and was the thing standing between this
/// runtime and real threads. With `+atomics` each Worker gets its own TLS
/// block, so each would have had its own file table, its own memory map and
/// its own process table -- which is a fork, not a thread.
///
/// A plain `static` is right instead, and for a reason particular to this
/// design: the shim's data and heap live in the *guest's* linear memory, and
/// against a shared memory that memory is a `SharedArrayBuffer`. So a static
/// here is already visible to every Worker that instantiated against it. The
/// only thing missing was mutual exclusion, which is what the lock is.
///
/// A spinlock rather than a blocking one because there is no OS underneath to
/// park on, and because a syscall holds this for a few microseconds at most.
#[cfg(feature = "shared-memory")]
static mut HOST: Option<Host> = None;

/// How much of the address space the guest may use.
///
/// Not all of it: a Rust slice may not be longer than `isize::MAX`, which on
/// wasm32 is just under 2 GiB, and `GuestMem` is built on a slice. Claiming
/// the whole 4 GiB produces an invalid slice, and the consequences are not a
/// clean failure -- the compiler deleted stores through it, and reads above
/// 2 GiB returned `EFAULT` from one call site and succeeded from another.
///
/// So the guest gets everything below this line and the shim takes what is
/// above, which it reaches through ordinary Rust pointers rather than through
/// the slice. The guest's own layout fits comfortably: its stack is the
/// highest thing in it, and the shim places it at this address rather than at
/// `STACK_TOP`.
const GUEST_LIMIT: u64 = 0x7fff_0000;

/// The guest's memory, as a slice.
///
/// This module's memory *is* the guest's, so the address space below
/// [`GUEST_LIMIT`] is addressable directly.
#[allow(clippy::mut_from_ref)]
#[inline(never)]
fn guest_bytes() -> &'static mut [u8] {
    // The base goes through `black_box` so the optimiser cannot see that it is
    // the constant zero. Without that, LLVM concludes the pointer cannot alias
    // anything and deletes every store through it: the first version of this
    // booted the guest with a zero stack pointer, because the writes setting
    // it up had all been optimised away. Reads survived, which made it look
    // like the two modules had different memories.
    //
    // SAFETY: in wasm, address zero is an ordinary location in linear memory
    // and the guest's address space starts there. The linker places this
    // module's own data and stack above `STACK_TOP`, and the guest never maps
    // there, so the two do not overlap; the memory is imported and never
    // grows, so base and length are fixed for the life of the instance.
    // The *pointer* goes through `black_box`, not just the address it is made
    // from. Hiding only the address was not enough: LLVM still tracked the
    // pointer's provenance and miscompiled accesses through it, so the same
    // read succeeded from a small function and returned EFAULT from inside the
    // kernel's call path. Hiding the pointer leaves the optimiser nothing to
    // reason about.
    let p: *mut u8 = std::hint::black_box(std::ptr::with_exposed_provenance_mut::<u8>(0));
    // The length is hidden as well. With it a constant, LLVM pairs it against
    // what it believes the pointer's allocation to be -- nothing -- and folds
    // slice bounds checks to "always out of range": the same read succeeded
    // from a small function and returned EFAULT from inside the kernel.
    // The slice spans the whole address space a guest pointer may name. What
    // actually *exists* is smaller and moves -- the memory is created at
    // `GUEST_INITIAL_BYTES` and grown as the guest maps -- so the real bound
    // is `memory.size`, and `GuestMem` reads it live on every access rather
    // than trusting this length. See `GuestMem::limit`.
    let len = std::hint::black_box(GUEST_LIMIT as usize);
    unsafe { std::slice::from_raw_parts_mut(p, len) }
}

/// Make a panic say what it was.
///
/// On wasm32 a Rust panic is a bare `unreachable`, and what reaches the page
/// is `RuntimeError: unreachable` with no file, no line and no message. That
/// is the whole of the diagnosis for any bug in the shim, which is not
/// enough to act on -- it took a bisect to find out even which subsystem had
/// failed. The hook writes the message and location to stderr through the
/// import the guest's own output uses, so a panic reads like a panic.
///
/// Installed on the first `with_host`, which every entry point goes through.
/// Write to the guest's stderr, which in a page is the only channel the shim
/// has to say anything at all.
///
/// # Safety
/// The one thread a page has; `write` only reads the bytes.
pub(crate) unsafe fn write_stderr(text: &str) {
    write(2, text.as_ptr() as u32, text.len() as u32);
}

fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let text = format!("gex: panic in the shim: {info}\n");
            // SAFETY: the one thread a page has, and `write` only reads.
            unsafe { write(2, text.as_ptr() as u32, text.len() as u32) };
        }));
    });
}

/// Install the kernel this Worker will run against.
#[cfg(not(feature = "shared-memory"))]
fn set_host(host: Host) {
    HOST.with(|h| *h.borrow_mut() = Some(host));
}

#[cfg(feature = "shared-memory")]
fn set_host(host: Host) {
    // SAFETY: see `with_host` -- one Worker is inside the kernel at a time.
    unsafe { *core::ptr::addr_of_mut!(HOST) = Some(host) };
}

/// Whether this Worker has a kernel yet.
///
/// A Worker that has instantiated the shim but not yet been given a process
/// -- everything between `attach` and `gex_adopt_child` -- has a memory and
/// no `Host`, and every other export here would panic on the way in. The host
/// asks this rather than guessing, because the alternative is that a joining
/// thread's first diagnostic call takes down the Worker it was diagnosing.
#[no_mangle]
pub extern "C" fn gex_has_host() -> i32 {
    #[cfg(not(feature = "shared-memory"))]
    {
        HOST.with(|h| h.borrow().is_some()) as i32
    }
    #[cfg(feature = "shared-memory")]
    {
        // SAFETY: a read of an `Option`'s discriminant; see `with_host`.
        unsafe { (*core::ptr::addr_of!(HOST)).is_some() as i32 }
    }
}

/// Where the call currently inside `with_host` came from.
///
/// Re-entering the kernel is the failure this shim is most prone to and the
/// hardest to place: the panic says "already borrowed" and names the borrow
/// that *failed*, which is never the interesting one. This records the borrow
/// that succeeded, so the message can name both.
static HOST_HOLDER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The last few `with_host` call sites, oldest first, for the message below.
///
/// A location alone says who is holding the kernel and not how it got there,
/// and the interesting part of a re-entrancy is the path. Six is enough to
/// see the shape and small enough to cost nothing.
static RING: [std::sync::atomic::AtomicUsize; 6] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; 6];
static RING_AT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn note(l: &'static std::panic::Location<'static>) {
    let i = RING_AT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % RING.len();
    RING[i].store(l as *const _ as usize, std::sync::atomic::Ordering::Relaxed);
}

fn recent() -> String {
    let n = RING_AT.load(std::sync::atomic::Ordering::Relaxed);
    let mut out = String::new();
    for k in 0..RING.len().min(n) {
        let i = (n + k) % RING.len();
        if let 0 = RING[i].load(std::sync::atomic::Ordering::Relaxed) {
            continue;
        }
        let p = RING[i].load(std::sync::atomic::Ordering::Relaxed);
        // SAFETY: only ever set from `&'static Location`.
        let l = unsafe { &*(p as *const std::panic::Location<'static>) };
        out.push_str(&format!(" {}:{}", l.file().rsplit('/').next().unwrap_or(""), l.line()));
    }
    out
}

fn holder() -> String {
    match HOST_HOLDER.load(std::sync::atomic::Ordering::Relaxed) {
        0 => "?".to_string(),
        p => {
            // SAFETY: only ever set from `&'static Location`.
            let l = unsafe { &*(p as *const std::panic::Location<'static>) };
            format!("{}:{}", l.file(), l.line())
        }
    }
}

#[cfg(not(feature = "shared-memory"))]
#[track_caller]
fn with_host<R>(f: impl FnOnce(&mut Host) -> R) -> R {
    install_panic_hook();
    let here = std::panic::Location::caller();
    HOST.with(|h| {
        let mut b = match h.try_borrow_mut() {
            Ok(b) => b,
            Err(_) => panic!(
                "gex: the kernel was re-entered at {}:{} while {} holds it; recent:{}",
                here.file(),
                here.line(),
                holder(),
                recent(),
            ),
        };
        HOST_HOLDER.store(here as *const _ as usize, std::sync::atomic::Ordering::Relaxed);
        note(here);
        // Named, because "not initialised" on its own says only that some
        // export ran before `gex_init` and not which -- and the glue calls
        // thirty of them.
        let r = f(b.as_mut().unwrap_or_else(|| {
            panic!("gex: not initialised, called from {}:{}", here.file(), here.line())
        }));
        HOST_HOLDER.store(0, std::sync::atomic::Ordering::Relaxed);
        r
    })
}

/// The same, holding the lock for the duration.
///
/// Re-entrancy is a real hazard and not a theoretical one: a host call that
/// asks the guest to write something can come back through here, and with a
/// spinlock that is a deadlock rather than a panic. Every such path already
/// had to avoid it -- `gex_hot_blocks` is the one that caught it before, by
/// re-entering from inside a guest `write` -- so the rule is unchanged and
/// only the punishment is worse.
#[cfg(feature = "shared-memory")]
#[track_caller]
fn with_host<R>(f: impl FnOnce(&mut Host) -> R) -> R {
    install_panic_hook();
    // A borrow in place, with neither a lock nor a move.
    //
    // Both were tried and both were wrong. *Holding* a lock across `f` is the
    // obvious shape: this shim traps out of `with_host` as ordinary control
    // flow -- that is how a guest hands the wasm stack back to JavaScript --
    // and a trap does not unwind, so the guard would never drop and the
    // kernel would be locked out for the rest of the run. *Moving* the `Host`
    // out and back avoids that and is worse: it copies the whole structure
    // twice on every syscall and every dispatch miss, which is not a hang but
    // is indistinguishable from one -- `park` produced no output at all,
    // where the same build with a thread-local ran it to completion.
    //
    // So: a plain static, borrowed where it lies. It is shared between this
    // process's Workers for free, because the shim's data lives in the
    // guest's linear memory and that memory is a `SharedArrayBuffer`.
    //
    // SAFETY: one Worker is inside the kernel at a time, and `KLock` is what
    // makes that true rather than hoped for. It is taken here and given back
    // before this returns; every path out of `f` that is not a return is a
    // trap, and a trap ends the process.
    let _guard = KLock::take();
    unsafe {
        f((*core::ptr::addr_of_mut!(HOST))
            .as_mut()
            .expect("gex: not initialised"))
    }
}

/// Room for the Workers running threads of this process: a stack and a
/// thread-local block each.
///
/// Both have to be per Worker and neither was. The stack pointer is a wasm
/// *global*, which belongs to an instance, so every Worker had one -- all
/// initialised to the same address, so two Workers ran on one stack. The
/// thread-locals are worse: a Rust module built with `+atomics` has real TLS
/// and `__wasm_init_tls` is what gives an instance a block of its own, and
/// nothing was calling it. Every Worker was reading and writing the same
/// `CURRENT` and the same lock depth, which is the exact opposite of what a
/// thread-local is for.
///
/// Taken from the arena, one slot at a time, and never from `.bss`.
///
/// It was a static array of sixty-four slots, on the reasoning that `.bss` is
/// address space and costs nothing until it is written. That is true of a
/// memory that may grow and false of one that may not: `wasm-ld` will not
/// link a `--shared-memory` module whose data does not fit inside
/// `--initial-memory`, so sixty-eight megabytes of untouched slots became
///
///   rust-lld: error: initial memory too small, 146908720 bytes needed
///
/// and the shared build simply stopped being produced. Which is worse than
/// it sounds: the build is silent about it, so the threads work went on
/// against a shim that no longer existed.
///
/// A slot comes from the same growing arena as everything else now, claimed
/// by the Worker that is about to stand on it. Sixty-four slots' worth of
/// address space is never reserved, and a process running one thread pays
/// for one stack.
#[cfg(feature = "shared-memory")]
mod slots {
    /// A megabyte of stack each. The shim's deepest call is a translation,
    /// which is recursive over a control-flow graph.
    pub const STACK: usize = 1 << 20;
    /// And room for the thread-locals, which are a handful of words today.
    pub const TLS: usize = 1 << 16;
    pub const COUNT: usize = 64;
}

#[cfg(feature = "shared-memory")]
extern "C" {
    /// Generated by wasm-ld for a module built with `+atomics`: point this
    /// instance's `__tls_base` at `ptr` and copy the initial values in.
    fn __wasm_init_tls(ptr: *mut u8);
}

/// Claim a slot for this Worker and return the top of its stack.
///
/// Called once, by each Worker, before anything else. The caller writes the
/// answer into `__stack_pointer`, because a wasm global cannot be assigned
/// from inside the module that owns it.
///
/// Slots are never given back. A process may start more threads over its life
/// than this has room for; what it may not do is run more than this many at
/// once, and the host recycles Workers rather than accumulating them.
#[cfg(feature = "shared-memory")]
#[no_mangle]
pub extern "C" fn gex_worker_init() -> u32 {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let i = NEXT.fetch_add(1, std::sync::atomic::Ordering::AcqRel) as usize;
    if i >= slots::COUNT {
        return 0;
    }
    // This Worker's own span of arena. The cursor is atomic and the bytes
    // above it belong to nobody, so two Workers arriving together get two
    // spans without a lock between them.
    let Some(base) = arena_take((slots::STACK + slots::TLS) as u64) else {
        return 0;
    };
    // SAFETY: `base` is this Worker's alone -- the arena cursor handed it out
    // and will not hand it out again -- and the memory behind it has been
    // grown to cover it.
    unsafe {
        let at = base as u32 as *mut u8;
        __wasm_init_tls(at.add(slots::STACK));
        // The thread-locals exist now, so this is the first thing that can be
        // said about which Worker this is.
        WORKER.with(|w| w.set(i as u32));
        (at as u32) + slots::STACK as u32
    }
}

/// Which Worker this is, among the ones running threads of this process.
///
/// Per Worker and not per process: with `+atomics` each Worker gets its own
/// TLS block, which is exactly wrong for the kernel -- that is why `HOST` is
/// a plain static -- and exactly right for this. Zero is "not yet told",
/// which the first Worker never is, so the lock's owner word is the id plus
/// one and zero means free.
#[cfg(feature = "shared-memory")]
thread_local! {
    static WORKER: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// How many times this Worker has taken the kernel lock without giving it
    /// back. A syscall that comes back through `with_host` -- the panic
    /// handler writing to fd 2 is the one that does -- would deadlock against
    /// itself otherwise, where the plain build merely panics.
    static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Name this Worker, before it runs any guest code.
///
/// The host hands out the numbers because it is the only thing that knows how
/// many Workers a process has.
#[cfg(feature = "shared-memory")]
#[no_mangle]
pub extern "C" fn gex_set_worker(id: u32) {
    WORKER.with(|w| w.set(id));
}

/// Who holds the kernel: a Worker id plus one, or zero for nobody.
#[cfg(feature = "shared-memory")]
static KLOCK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The kernel lock, held for one call into `with_host`.
///
/// A wasm `memory.atomic.wait32` rather than a spin, because a Worker that
/// spins on a memory another Worker is about to write burns a core doing it
/// and this runs on a laptop with the tab in the foreground. Waiting is legal
/// here for the reason the whole design rests on: every guest thread runs in
/// a Worker, and only the page's main thread is forbidden to block.
#[cfg(feature = "shared-memory")]
struct KLock;

#[cfg(feature = "shared-memory")]
impl KLock {
    fn take() -> KLock {
        let me = WORKER.with(|w| w.get()) + 1;
        if DEPTH.with(|d| d.get()) > 0 {
            // Already ours. Counted rather than taken again.
            DEPTH.with(|d| d.set(d.get() + 1));
            return KLock;
        }
        loop {
            match KLOCK.compare_exchange_weak(
                0,
                me,
                std::sync::atomic::Ordering::Acquire,
                std::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(held) => {
                    if held == 0 {
                        continue;
                    }
                    // SAFETY: the address of a `static AtomicU32`, which is
                    // four-byte aligned and in the memory every Worker shares.
                    unsafe {
                        warch::memory_atomic_wait32(
                            KLOCK.as_ptr() as *mut i32,
                            held as i32,
                            // A millisecond, so a lock lost to a trapped
                            // Worker is a slow run rather than a dead one.
                            1_000_000,
                        );
                    }
                }
            }
        }
        DEPTH.with(|d| d.set(1));
        KLock
    }
}

#[cfg(feature = "shared-memory")]
impl Drop for KLock {
    fn drop(&mut self) {
        let d = DEPTH.with(|d| d.get());
        DEPTH.with(|c| c.set(d.saturating_sub(1)));
        if d > 1 {
            return;
        }
        KLOCK.store(0, std::sync::atomic::Ordering::Release);
        // SAFETY: as above.
        unsafe {
            warch::memory_atomic_notify(KLOCK.as_ptr() as *mut i32, 1);
        }
    }
}

/// Sockets the host holds one end of, so JS can carry a guest's stream
/// somewhere the guest cannot reach -- another Worker, in particular.
static HOST_SOCKETS: SingleThreaded<Vec<Arc<gex_kernel::unix::UnixSocket>>> =
    SingleThreaded(const { RefCell::new(Vec::new()) });

/// Connect the host to a unix socket the *guest* has bound.
///
/// The mirror of `bind_host`, and the half that was missing. A guest that
/// connects to a socket the host serves already works -- that is how the
/// virtio-gpu renderer is reached. A guest that *serves* one had no way to be
/// reached from outside, which is the problem with putting each process in
/// its own Worker: an X server in one Worker and its client in another need a
/// socket between them, and only one of those two directions existed.
///
/// `SocketNames::connect` already does the work; it only needed a host-owned
/// socket to do it with.
///
/// Returns a handle for [`gex_host_send`] and [`gex_host_recv`], or -1 when
/// nothing is bound at that path.
#[no_mangle]
pub extern "C" fn gex_host_connect(path: u32, path_len: u32) -> i32 {
    let Ok(name) = std::str::from_utf8(shim_slice(path, path_len)) else {
        return -1;
    };
    with_host(|h| {
        let sock = Arc::new(gex_kernel::unix::UnixSocket::new(true));
        if h.kernel.socket_names.connect(&sock, name).is_err() {
            return -1;
        }
        let mut table = HOST_SOCKETS.0.borrow_mut();
        table.push(sock);
        (table.len() - 1) as i32
    })
}

/// Write to the guest through a handle from [`gex_host_connect`].
#[no_mangle]
pub extern "C" fn gex_host_send(handle: i32, ptr: u32, len: u32) -> i32 {
    let bytes = shim_slice(ptr, len);
    let sock = HOST_SOCKETS
        .0
        .borrow()
        .get(handle.max(0) as usize)
        .cloned();
    match sock {
        Some(s) => s.send(bytes).map(|n| n as i32).unwrap_or(-1),
        None => -1,
    }
}

/// Read whatever the guest has written. Returns 0 when there is nothing yet,
/// which is not the same as end of stream -- the caller comes back.
#[no_mangle]
pub extern "C" fn gex_host_recv(handle: i32, ptr: u32, max: u32) -> i32 {
    let sock = HOST_SOCKETS
        .0
        .borrow()
        .get(handle.max(0) as usize)
        .cloned();
    let Some(s) = sock else { return -1 };
    let out = shim_slice_mut(ptr, max);
    s.recv(out, 0, false).map(|n| n as i32).unwrap_or(0)
}

/// A socket the host serves, whose accepted connections JS collects.
///
/// The other half of [`gex_host_connect`], and the one that makes two guest
/// processes in two Workers able to talk. Each Worker has its own kernel and
/// its own filesystem -- that is the point of the design -- so a client in
/// one cannot see a socket a server bound in the other. What crosses is the
/// stream: the client's Worker serves the path, the host accepts, and the
/// host also connects into the server's Worker with `gex_host_connect`. Two
/// handles, and JS moves bytes between them.
#[derive(Default, Debug)]
struct JsListener {
    accepted: std::sync::Mutex<Vec<Arc<gex_kernel::unix::UnixSocket>>>,
}

impl gex_kernel::unix::HostListener for JsListener {
    fn accept(&self, peer: Arc<gex_kernel::unix::UnixSocket>) {
        self.accepted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(peer);
    }
}

static HOST_LISTENERS: SingleThreaded<Vec<Arc<JsListener>>> =
    SingleThreaded(const { RefCell::new(Vec::new()) });

/// Serve a unix socket at `path` from the host. Returns a listener id.
///
/// A guest connecting to it succeeds immediately; the connection waits in
/// this listener until [`gex_host_accept`] takes it.
#[no_mangle]
pub extern "C" fn gex_host_bind(path: u32, path_len: u32) -> i32 {
    let Ok(name) = std::str::from_utf8(shim_slice(path, path_len)) else {
        return -1;
    };
    let listener = Arc::new(JsListener::default());
    with_host(|h| {
        h.kernel.socket_names.bind_host(
            name,
            Arc::clone(&listener) as Arc<dyn gex_kernel::unix::HostListener>,
        );
        // And in the filesystem, for the same reason the virtio-gpu socket
        // is: `connect` goes through the name table and never needs a node,
        // but anything that checks the path exists first is told there is
        // nothing there. Mesa connects without looking; Xlib does not, and
        // neither does a shell script guarding on `test -S`.
        //
        // Through the mount, because with a global kernel `h.root` is this
        // process's own `TmpFs` and nothing is mounted on it.
        let _ = h.kernel.vfs.mksocket(name);
    });
    let mut table = HOST_LISTENERS.0.borrow_mut();
    table.push(listener);
    (table.len() - 1) as i32
}

/// Take one waiting connection, as a handle for [`gex_host_send`] and
/// [`gex_host_recv`]. -1 when nothing is waiting.
#[no_mangle]
pub extern "C" fn gex_host_accept(listener: i32) -> i32 {
    let l = HOST_LISTENERS
        .0
        .borrow()
        .get(listener.max(0) as usize)
        .cloned();
    let Some(l) = l else { return -1 };
    let sock = l
        .accepted
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pop();
    let Some(sock) = sock else { return -1 };
    let mut table = HOST_SOCKETS.0.borrow_mut();
    table.push(sock);
    (table.len() - 1) as i32
}

/// How many regions of guest memory this process actually owns.
///
/// A process's world is two gigabytes of address space and a few hundred
/// megabytes of it at most, so anything that has to *move* a process -- a
/// `fork` that hands the child to another Worker, a host that wants to
/// checkpoint one -- needs to know which parts are real. Copying the address
/// space instead is not an option at this size.
///
/// Paired with [`gex_vma_at`]: count first, then ask for each.
#[no_mangle]
pub extern "C" fn gex_vma_count() -> i32 {
    with_host(|h| h.kernel.mem.vmas().len() as i32)
}

/// Describe region `i`, writing `start`, `end` and `prot` as three `u64` at
/// `out` in this module's own region.
///
/// Returns 0, or -1 if there is no such region. The three words rather than a
/// return value because a wasm function returns one thing and this is the
/// cheapest way to hand back three without a struct layout both sides have to
/// agree on.
#[no_mangle]
pub extern "C" fn gex_vma_at(i: i32, out: u32) -> i32 {
    with_host(|h| {
        let Some(v) = h.kernel.mem.vmas().get(i.max(0) as usize) else {
            return -1;
        };
        let (start, end, prot) = (v.start, v.end, v.prot as u64);
        let dst = shim_slice_mut(out, 24);
        dst[0..8].copy_from_slice(&start.to_le_bytes());
        dst[8..16].copy_from_slice(&end.to_le_bytes());
        dst[16..24].copy_from_slice(&prot.to_le_bytes());
        0
    })
}

/// How much of the shim's heap is in use, in KiB. -1 without the counters.
///
/// The heap is a *fixed* region -- `__heap_base` to the top of the memory,
/// about 2039 MiB -- because the memory belongs to the guest and cannot grow
/// without moving where the guest's stack lands. So exhausting it is a real
/// failure mode, and one with no symptom of its own: what a page sees is a
/// fault at whatever the guest touched next, at whatever address the guest
/// was last recorded at, which is nowhere near the allocation that failed.
///
/// Recompiling Mesa's software rasteriser is the case that motivated this.
/// It lifts 471,205 blocks, and natively that costs a peak working set of
/// 2109 MiB against those 2039 -- close enough on both sides of the line that
/// guessing is no good and a number is needed.
///
/// Needs `--features heap-counters`, which charges every allocation and free
/// and is therefore not what the page ships.
#[no_mangle]
pub extern "C" fn gex_heap_used_kib() -> i32 {
    #[cfg(feature = "heap-counters")]
    {
        (ALLOC.1.load(std::sync::atomic::Ordering::Relaxed) / 1024) as i32
    }
    #[cfg(not(feature = "heap-counters"))]
    {
        -1
    }
}

/// How large the shim's heap is in total, in KiB. Always available.
///
/// `__heap_base` is a link-time address, so this is not a constant anyone can
/// write down -- it moves whenever the shim's own data does. Reported rather
/// than assumed, so a ceiling quoted in a bug report is the real one.
///
/// `total - used` is an upper bound on what is left, not the answer: talc can
/// only hand out a contiguous span, so a fragmented heap refuses allocations
/// while this still says there is room. A lift that fails with a gigabyte
/// nominally free has a fragmentation problem rather than a size one, and the
/// two want different fixes.
#[no_mangle]
pub extern "C" fn gex_heap_total_kib() -> i32 {
    let base = (&raw const __heap_base) as usize;
    ((gex_abi::GUEST_MEM_BYTES as usize - base - 65536) / 1024) as i32
}

/// Reserve `len` bytes of this module's own heap and return a guest address
/// JS can write to. Used to hand the ELF across.
#[no_mangle]
pub extern "C" fn gex_alloc(len: u32) -> u32 {
    // `u32` rather than `usize`: this is the JS boundary, and a length is a
    // Number there. Nothing JS allocates is near 4 GiB.
    let len = len as usize;
    let mut v = Vec::<u8>::with_capacity(len);
    v.resize(len, 0);
    let p = v.as_mut_ptr() as u32;
    std::mem::forget(v);
    p
}

/// Give back an allocation from [`gex_alloc`].
///
/// The setup calls allocate once and never care; interactive input is the
/// case that does, because it allocates per keystroke and a page stays open.
///
/// # Safety
/// `ptr` and `len` must be exactly what a previous `gex_alloc` returned.
#[no_mangle]
pub unsafe extern "C" fn gex_free(ptr: u32, len: u32) {
    // `u32` for the same reason as `gex_alloc`.
    let len = len as usize;
    if ptr != 0 && len != 0 {
        drop(Vec::from_raw_parts(ptr as *mut u8, len, len));
    }
}

/// Load a static ELF that JS has written at `ptr`, place it and its stack, and
/// return the entry point. Zero means the image could not be loaded.
#[no_mangle]
pub extern "C" fn gex_load(ptr: u32, len: u32, argv: u32, argv_len: u32) -> u64 {
    // The ELF sits in the shim's own heap, above the guest's limit, so it is
    // reached through its own pointer rather than through the guest slice.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            std::ptr::with_exposed_provenance::<u8>(std::hint::black_box(ptr as usize)),
            len as usize,
        )
    };
    // The bundle is a filesystem before anything else happens, because a
    // dynamic executable's interpreter is read out of it.
    //
    // Two roots, and only one of them mounted. With a global kernel the files
    // belong to the *system* rather than to this process -- a child forked
    // from here has to see them, and it has a memory of its own -- so the
    // bundle goes there and this process mounts a `RemoteFs` over it. Without
    // one there is only this process, and its own `TmpFs` is the whole
    // filesystem, as it always was.
    let root = Arc::new(gex_kernel::vfs::TmpFs::new());
    let shared_root = global_root();
    match &shared_root {
        Some(_) => install_bundle_remote(),
        None => install_bundle(&root),
    }
    let read_file = |path: &str| -> Option<Vec<u8>> {
        use gex_kernel::vfs::FileSystem;
        let p = path.trim_start_matches('/');
        let f = match &shared_root {
            Some(r) => r.open(p, 0, 0).ok()?,
            None => root.open(p, 0, 0).ok()?,
        };
        let len = f.metadata().ok()?.size as usize;
        let mut buf = vec![0u8; len];
        f.read_at(0, &mut buf).ok()?;
        Some(buf)
    };
    let program = match gex_ld::load_program(bytes, gex_abi::PIE_BASE, INTERP_BASE, &read_file) {
        Ok(p) => p,
        Err(_) => return 0,
    };

    let mut kernel = Kernel::new(gex_abi::ctx_addr(0), program.brk(), String::new());
    // The staged layout, now that there is somewhere to put it. Taken rather
    // than copied: this runs once, and a second `gex_load` in the same Worker
    // is an `execve`, which gets its placement from the global table like
    // every other process.
    kernel.pins = std::mem::take(&mut *PINS.0.borrow_mut()).into_iter().collect();
    // This host's memory has to be allocated up to whatever the map hands
    // out, so the map asks before it hands anything out.
    kernel.mem.grows_on_demand(arena_grow, arena_back);
    // The table every process shares, when the host has wired one up. This
    // process is the first, so it keeps the pid the kernel gave it -- the
    // global table's `INIT_PID` is the same 1.
    if let Some(procs) = global_procs() {
        kernel.procs = procs;
        // And the layout they share. Only with a global kernel: without one
        // this process is the whole system and has nobody to agree with.
        kernel.place = Some(std::sync::Arc::new(SharedPlaces));
    }
    kernel.pipes = global_pipes();
    kernel.unix_remote = global_sockets();
    match &shared_root {
        Some(r) => kernel.vfs.mount("/", r.clone()),
        None => kernel.vfs.mount("/", root.clone()),
    }
    // The graphics device, when the page said it has a canvas to draw on.
    // Absent otherwise, so a guest sees a machine with no GL rather than one
    // whose drawing vanishes.
    let virgl = mount_devices(&mut kernel);
    // And the socket in the filesystem, so a guest that looks before it
    // connects finds it. `connect` goes through the name table and never
    // needed this; `test -e` does, and so does the gears script.
    //
    // Through the mount rather than into `root` directly: with a global
    // kernel `root` is this process's own `TmpFs` and is not mounted
    // anywhere, so a node added to it is a node nothing can see.
    if virgl.is_some() {
        let _ = kernel.vfs.mksocket(crate::virgl::DEFAULT_SOCKET);
    }
    // `/tmp` is part of the root filesystem rather than a mount of its own,
    // so a page can bundle a file at any path at all. A mount there meant a
    // bundled `/tmp/x` went into the root and every lookup of it went to the
    // mount, which had never heard of it.
    root.add_dir("/tmp");
    // `/proc` matters more here than it looks: a program that cannot open
    // `/proc/self/maps` gets a null `FILE*` back, and the next `fgets` calls
    // through it. That surfaces as a jump to address zero -- a dispatch miss
    // at pc=0 -- with nothing to say the filesystem was the cause.
    root.add_dir("/proc");
    let proc_state: Arc<gex_kernel::KernelProcSource> = Arc::new(gex_kernel::KernelProcSource(
        std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
    ));
    kernel.vfs.mount(
        "/proc",
        Arc::new(gex_kernel::vfs::ProcFs::new(proc_state.clone())),
    );
    kernel.clock = browser_clock;
    kernel.net = unsafe { HAVE_NET }.then(|| Arc::new(WispNet) as Arc<dyn net::NetStack>);
    {
        let cwd = std::mem::take(&mut *CWD.0.borrow_mut());
        if !cwd.is_empty() {
            let _ = kernel.vfs.set_cwd(&cwd);
        }
    }
    kernel.uid = unsafe { IDENTITY.0 };
    // Replace the descriptors `Kernel::new` opened on `std::io` with ones the
    // host can actually see.
    install_console(&mut kernel);

    let data = guest_bytes();
    for seg in program.segments() {
        let (a, b) = (seg.vaddr as usize, (seg.vaddr + seg.mem_size) as usize);
        data[a..b].fill(0);
        data[a..a + seg.bytes.len()].copy_from_slice(&seg.bytes);
        let mut p = 0;
        if seg.readable {
            p |= gex_kernel::mem::prot::READ;
        }
        if seg.writable {
            p |= gex_kernel::mem::prot::WRITE;
        }
        if seg.executable {
            p |= gex_kernel::mem::prot::EXEC;
        }
        // See `do_exec`: an image segment is built, not mapped, so it has
        // no file to be reconstructed from.
        kernel
            .mem
            .insert(seg.vaddr, seg.vaddr + seg.mem_size, p, "[image]");
    }

    // argv arrives as a NUL-separated blob in the shim's own region.
    let args: Vec<String> = if argv_len == 0 {
        vec!["prog".to_string()]
    } else {
        let blob = unsafe {
            std::slice::from_raw_parts(
                std::ptr::with_exposed_provenance::<u8>(std::hint::black_box(argv as usize)),
                argv_len as usize,
            )
        };
        blob.split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    };
    let env = ["PATH=/usr/bin:/bin".to_string(), "HOME=/root".to_string()];
    // `/proc/self/{cmdline,exe}` read these. A relative argv[0] is resolved
    // against the root, which is where a bundled program lives.
    kernel.exe_path = if args[0].starts_with('/') {
        args[0].clone()
    } else {
        format!("/{}", args[0])
    };
    kernel.argv = args.clone();
    kernel.envp = env.to_vec();
    let layout = gex_ld::build_initial_stack(
        &program.exe,
        // `STACK_TOP`, not the top of the guest's addressable range.
        //
        // A stack at the top makes the top load-bearing, and a linear memory
        // has to be allocated up to the highest address anything touches --
        // so a process cost four gigabytes before running an instruction,
        // and six of them cost more than a browser would give.
        gex_abi::STACK_TOP,
        &args,
        &env,
        *b"gexdeterministic",
        gex_ld::Identity {
            uid: unsafe { IDENTITY.0 },
            gid: unsafe { IDENTITY.1 },
            // Where the interpreter ended up, which is how it finds itself.
            interp_base: program.interp_base(),
        },
    );
    let start = layout.low as usize;
    data[start..start + layout.bytes.len()].copy_from_slice(&layout.bytes);
    kernel.mem.insert(
        gex_abi::STACK_TOP - gex_abi::STACK_SIZE,
        gex_abi::STACK_TOP,
        gex_kernel::mem::prot::READ | gex_kernel::mem::prot::WRITE,
        "[stack]",
    );

    let c = gex_abi::ctx_addr(0) as usize;
    write_u64(data, c + ctx::SP, layout.sp);
    write_u64(data, c + ctx::PC, program.entry());
    data[c + ctx::SAFEPOINT_COUNTER..c + ctx::SAFEPOINT_COUNTER + 4]
        .copy_from_slice(&1024i32.to_le_bytes());
    for i in 0..31u8 {
        write_u64(data, c + ctx::x(i), 0);
    }

    let meta = gex_xlate::Metadata::decode(&METADATA.0.borrow()).unwrap_or_default();
    let entry = program.entry();
    set_host(Host {
            kernel,
            virgl,
            root,
            proc_state,
            threads: vec![Thread {
                kstate: ThreadState::new(gex_abi::ctx_addr(0), 1),
                pc: entry as u64,
                waiting: Waiting::Runnable,
                blocked_on: None,
            }],
            exit: None,
            jit: Jit {
                described: meta.next_slot != 0,
                page_table: gex_xlate::PageTable::from_entries(&meta.dir_entries, meta.next_slot),
                translated: Default::default(),
                registry: Vec::new(),
                shared: Default::default(),
                installed_mods: Default::default(),
                reuse_have: 0,
                reuse_hit: 0,
                reuse_miss: 0,
                reuse_noident: 0,
                reuse_served: 0,
                intrinsics: Default::default(),
                intrinsics_scanned: Default::default(),
                candidates: Vec::new(),
                pending: Default::default(),
                blocks: 0,
                compiles: 0,
                nanos: 0,
            },
    });
    entry
}

/// Place `path` at `base`, because something was built assuming it is there.
///
/// The layout a closure build wrote out. Every module in a statically
/// recompiled closure has the guest addresses of the object it came from
/// baked into it, so those addresses are a precondition rather than a
/// preference -- and `ld.so` will take whatever `mmap(NULL, span)` hands it,
/// so gex is free to hand it the right one.
///
/// Two places have to agree and both are set here. `Kernel::pins` is what
/// the reservation in *this* process consults; the global placement table is
/// what every other process consults, so a `fork` and an `execve` of the same
/// program land the library in the same place and can share what was built
/// for it. Without the second, only the first process would be right.
#[no_mangle]
pub extern "C" fn gex_pin_object(path: u32, path_len: u32, base: u64, span: u64) {
    let Ok(name) = std::str::from_utf8(shim_slice(path, path_len)) else {
        return;
    };
    PINS.0.borrow_mut().push((name.to_string(), base));
    // SAFETY: `k_present` takes no borrow, and `k_place_set` takes scalars.
    unsafe {
        if k_present() != 0 {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            name.hash(&mut hasher);
            // With the span, which is not optional. Told zero, the counter
            // moved one alignment past each pinned base and the *next*
            // object placed automatically landed inside the pinned one --
            // whereupon libc's own reservation found its address occupied,
            // was placed elsewhere, and every module built for it was wrong.
            // `ls` segfaulted on that, and the pin report said so:
            // "wanted 0x5000000, landed 0xde42000".
            k_place_set(hasher.finish(), base, span);
        }
    }
}

/// Where each pinned object actually landed, as `path base landed` lines.
///
/// A pin that silently does not take is the worst outcome available: the
/// module was built for one address, the code runs at another, and the guest
/// branches into whatever is there. Written to stderr so a mismatch is loud.
#[no_mangle]
pub extern "C" fn gex_pin_report() {
    let landed = with_host(|h| h.kernel.pin_landed.clone());
    for (path, want, got) in landed {
        let how = if want == got { "ok" } else { "MISPLACED" };
        trace_line(&format!("[gex] pin {how}: {path} wanted {want:#x}, landed {got:#x}\n"));
    }
}

/// Put a file into the guest's filesystem, before it runs.
///
/// A browser has no filesystem to pass through to, so the page bundles what
/// the program needs: a script to interpret, data to read. Both pointers are
/// into the shim's own region, written by JS after [`gex_alloc`].
#[no_mangle]
pub extern "C" fn gex_add_file(path: u32, path_len: u32, data: u32, data_len: u32) {
    let Ok(name) = std::str::from_utf8(shim_slice(path, path_len)) else {
        return;
    };
    // Straight into the shared root when there is one, rather than staged.
    //
    // Staging the whole bundle first meant it sat in *this process's* memory
    // before any of it reached the filesystem that was going to own it, and a
    // wasm memory never gives anything back -- so a rootfs cost its own size
    // in every process that mounted it, including the later ones that find it
    // already filled and drop their copy unread. Debian's 226 MB rootfs put a
    // four-process pipeline at 1.3 GB.
    //
    // The bundle still exists before the program is loaded, which is all the
    // ordering requires: `k_fs_add` installs each file in the shared root as
    // it arrives, and the loader reads the interpreter back out of that same
    // filesystem in `gex_load`.
    if unsafe { k_present() } != 0 {
        if unsafe { k_fs_populated() } == 0 {
            unsafe { k_fs_add(path, path_len, data, data_len) };
        }
        return;
    }
    // No global kernel: this process is the whole system and its own `TmpFs`
    // is the filesystem. Staged as before, because that `TmpFs` does not
    // exist yet -- `gex_load` builds it.
    let bytes = shim_slice(data, data_len).to_vec();
    FILES.0.borrow_mut().push((name.to_string(), bytes));
}

/// The directory the closure's modules published, kept so an `execve` can
/// have it back.
///
/// `do_exec` clears the guest's dispatch directory, because anything the old
/// program left there is code at the same addresses doing something else.
/// That is right for translations, which belong to one program, and wrong
/// for a closure: its modules are already *in* the dispatch tables, which
/// `execve` does not touch -- the guest module instance is not rebuilt, only
/// the image in its memory -- so the functions are still there and only the
/// directory pointing at them was thrown away.
///
/// Replaying it is safe because the layout is pinned. libc is at one address
/// for the whole run, so a page this covers either holds the object the
/// module was built from or holds nothing, and a branch into nothing faults
/// whatever the directory says.
static CLOSURE_DIRS: SingleThreaded<Vec<(u64, u32)>> =
    SingleThreaded(const { RefCell::new(Vec::new()) });

/// Where the closure build says each object goes, staged until the kernel
/// that will honour it exists.
///
/// `gex_pin_object` is called before `gex_load`, because the layout has to be
/// in place before the loader maps anything -- and at that moment there is no
/// `Host` to put it in. The global placement table can be told straight away,
/// since it lives in the kernel module rather than here.
static PINS: SingleThreaded<Vec<(String, u64)>> =
    SingleThreaded(const { RefCell::new(Vec::new()) });

/// Files bundled by the page, in the order they arrived.
type Bundle = Vec<(String, Vec<u8>)>;
static FILES: SingleThreaded<Bundle> = SingleThreaded(const { RefCell::new(Vec::new()) });

/// Put the staged bundle into a filesystem, creating the directories on the
/// way to each file.
fn install_bundle(root: &gex_kernel::vfs::TmpFs) {
    for (name, bytes) in std::mem::take(&mut *FILES.0.borrow_mut()) {
        let mut at = String::new();
        for part in name
            .trim_start_matches('/')
            .split('/')
            .rev()
            .skip(1)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            at.push('/');
            at.push_str(part);
            root.add_dir(&at);
        }
        root.add_file(&name, bytes);
    }
}

/// Where the dynamic linker is placed, clear of the executable so neither can
/// overlap the other's mappings. The same address `gexc` uses, so a program
/// looks identical under both hosts.
// Inside the fixed low region, with the image and its break below it and the
// stack above.
//
// It was at 640 MiB, which is above where a process's memory now starts and
// is written *directly* rather than through the memory map -- so the first
// dynamic program to load trapped on memory that had never been grown. The
// graphics rootfs is all dynamic, which is why it was the one that showed it.
const INTERP_BASE: u64 = gex_abi::INTERP_BASE;

/// A file read back out of the guest's filesystem, kept alive for JS.
static EXTRACTED: SingleThreaded<Vec<u8>> = SingleThreaded(const { RefCell::new(Vec::new()) });

/// Read a file out of the guest's filesystem, and return its length.
///
/// The other half of bundling: a page that runs a compiler wants the object
/// file back. Zero means there is nothing there -- a length of zero and a
/// missing file are the same answer, which is all a caller can act on anyway.
#[no_mangle]
pub extern "C" fn gex_get_file(path: u32, path_len: u32) -> u32 {
    let Ok(name) = std::str::from_utf8(shim_slice(path, path_len)) else {
        return 0;
    };
    let bytes = with_host(|h| {
        let f = h.kernel.vfs.open(name, 0, 0).ok()?;
        let len = f.metadata().ok()?.size as usize;
        let mut buf = vec![0u8; len];
        let n = f.read_at(0, &mut buf).ok()? as usize;
        buf.truncate(n);
        Some(buf)
    })
    .unwrap_or_default();
    let len = bytes.len() as u32;
    *EXTRACTED.0.borrow_mut() = bytes;
    len
}

/// Where the last [`gex_get_file`] left its contents.
#[no_mangle]
pub extern "C" fn gex_file_data() -> u32 {
    EXTRACTED.0.borrow().as_ptr() as u32
}

/// The identity the guest is given, which the embedder may override before
/// loading. A page has no user, so 1000 is as good an answer as any; Node
/// passes its own so a comparison against QEMU on the same machine agrees.
static mut IDENTITY: (u32, u32) = (1000, 1000);

/// Bytes JS wrote into the shim's own region, which is above the guest's
/// limit and so is reached through a pointer rather than through
/// [`guest_bytes`].
fn shim_slice(p: u32, n: u32) -> &'static [u8] {
    unsafe {
        std::slice::from_raw_parts(
            std::ptr::with_exposed_provenance::<u8>(std::hint::black_box(p as usize)),
            n as usize,
        )
    }
}

/// As [`shim_slice`], for a buffer the shim is filling in for JS to read.
fn shim_slice_mut(p: u32, n: u32) -> &'static mut [u8] {
    unsafe {
        std::slice::from_raw_parts_mut(
            std::ptr::with_exposed_provenance_mut::<u8>(std::hint::black_box(p as usize)),
            n as usize,
        )
    }
}

/// The module's dispatch metadata, staged until the guest is loaded.
///
/// It rides in the module's own `gex.meta` custom section, which JS finds and
/// hands over: everything runtime translation needs to carry on allocating
/// table slots where the ahead-of-time build left off.
static METADATA: SingleThreaded<Vec<u8>> = SingleThreaded(const { RefCell::new(Vec::new()) });

#[no_mangle]
pub extern "C" fn gex_set_metadata(ptr: u32, len: u32) {
    *METADATA.0.borrow_mut() = shim_slice(ptr, len).to_vec();
}

/// Take a module the closure build made for one of this program's libraries.
///
/// The whole point of the project, finally reaching the browser: the object
/// was translated ahead of time, so the guest never pays a dispatch miss for
/// it at all. The module is `Linkage::Shared` -- it imports the memory and
/// the dispatch tables and fills them itself, exactly as a runtime
/// translation does -- so nothing new is needed to install it. What is
/// needed is the metadata: which pages it covers and at which base, and how
/// far the table must reach.
///
/// The bases are the build's, and they are safe to take without checking
/// because one `PageTable` in `gexc` allocated every object of the closure
/// including the image module this process is running. They cannot collide
/// with each other and they cannot collide with the image.
///
/// Returns the slot count the table must be grown to, or zero if the
/// metadata could not be read -- in which case the caller must not install
/// the module, since its blocks would land in slots nothing has claimed.
#[no_mangle]
pub extern "C" fn gex_add_closure_module(ptr: u32, len: u32) -> u32 {
    let Some(meta) = gex_xlate::Metadata::decode(shim_slice(ptr, len)) else {
        return 0;
    };
    CLOSURE_DIRS.0.borrow_mut().extend(meta.dir_entries.iter().copied());
    with_host(|h| {
        let mut gm = GuestMem::new(guest_bytes());
        for (page, base) in &meta.dir_entries {
            h.jit.page_table.assign(*page, *base);
            // And the guest's own directory, which is what a branch reads.
            // The image module publishes its own as a data segment; a
            // `Shared` module has no memory of its own to put one in, so the
            // host writes them.
            let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, *base);
        }
        h.jit.page_table.advance_to(meta.next_slot);
        h.jit.page_table.slots() as u32
    })
}

/// The directory the guest starts in, staged until it is loaded.
static CWD: SingleThreaded<String> = SingleThreaded(const { RefCell::new(String::new()) });

/// Set the guest's working directory. A program given a relative path -- a
/// benchmark reading its data files, a compiler given a source name -- has
/// nowhere to resolve it from otherwise.
#[no_mangle]
pub extern "C" fn gex_set_cwd(ptr: u32, len: u32) {
    if let Ok(s) = std::str::from_utf8(shim_slice(ptr, len)) {
        *CWD.0.borrow_mut() = s.to_string();
    }
}

/// Standard input, staged until the guest is loaded.
/// The one stdin queue, reachable both from the descriptor the kernel holds
/// and from the exports the page calls.
static STDIN: StdinHandle = StdinHandle(std::sync::LazyLock::new(|| Arc::new(StdinQueue::default())));

struct StdinHandle(std::sync::LazyLock<Arc<StdinQueue>>);
unsafe impl Sync for StdinHandle {}
impl std::ops::Deref for StdinHandle {
    type Target = Arc<StdinQueue>;
    fn deref(&self) -> &Arc<StdinQueue> {
        &self.0
    }
}

/// Whether stdin should report itself a terminal.
static mut STDIN_TTY: bool = false;

/// Append what the user typed. The page calls this from its keyboard handler,
/// between runs of the guest.
#[no_mangle]
pub extern "C" fn gex_push_stdin(ptr: u32, len: u32) {
    STDIN.push(shim_slice(ptr, len));
}

/// There will be no more input. A read that finds the queue empty returns
/// zero from here on rather than waiting.
#[no_mangle]
pub extern "C" fn gex_stdin_eof() {
    STDIN.eof.set(true);
}

/// Whether the guest stopped because it is waiting for input rather than
/// because it finished.
///
/// A page with a terminal in it needs to tell the two apart: one means "show
/// a cursor and wait", the other means "the program is over". Both arrive in
/// JS the same way -- the guest gives up the wasm stack -- so the difference
/// has to be asked for.
#[no_mangle]
pub extern "C" fn gex_paused_for_input() -> i32 {
    with_host(|h| (paused() & 1 != 0) as i32)
}

/// Whether the guest stopped because it is waiting on the network. The page
/// has nothing to supply -- it only has to let its event loop run, so the
/// data it is waiting for can arrive -- and then call [`gex_wake`].
#[no_mangle]
pub extern "C" fn gex_paused_for_net() -> i32 {
    with_host(|h| (paused() & 2 != 0) as i32)
}

/// Whether ending this wait needs the page's own event loop to turn.
///
/// The narrow half of `gex_paused_for_net`. A guest waiting on another guest
/// process is waiting on a different Worker, which will bump the shared wake
/// word when it gets there -- so this Worker can sleep on that word and be
/// woken in microseconds. A guest waiting on the network is waiting on a
/// callback that runs on *this* Worker's event loop, and sleeping is exactly
/// how it never arrives.
#[no_mangle]
pub extern "C" fn gex_paused_for_host() -> i32 {
    with_host(|h| (paused() & 4 != 0) as i32)
}

/// Carry on after input has arrived: pick a thread and say where to re-enter.
///
/// Returns 1 when there is somewhere to go, 2 when the guest is finished.
#[no_mangle]
/// The wake counter as it read when this Worker last looked at its threads.
static WAKE_SEEN: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// How long a syscall waits where it stands before giving the stack back.
///
/// Short, and deliberately so. This is not how long the wait may take -- a
/// wait another Worker ends is ended by its notify, in microseconds, and
/// never reaches this bound. It is what a *misjudged* wait costs: something
/// only the page can end, waited for here, is dead time until the bound
/// passes and the stack goes back.
///
/// It was twenty milliseconds, on the reasoning that a sleeping Worker costs
/// nothing, and that was the wrong reasoning about the wrong quantity. The
/// gears demo booted to the same point and then crawled -- the X handshake is
/// a great many small waits, and paying twenty milliseconds for each one that
/// this could not classify is most of a minute of nothing.
const PARK_HERE_NS: u64 = 2_000_000;

/// Whether the page, rather than another Worker, is what ends this wait.
///
/// The distinction the whole of this turns on. A guest waiting for another
/// guest process is waiting for a different Worker, which bumps the wake
/// counter when it gets there. A guest waiting for a keystroke, or for bytes
/// off a WebSocket, is waiting for a callback on *this* Worker's event loop,
/// and a Worker that has not returned from a syscall is not running it.
fn wait_needs_page(t: &Thread, kernel: &gex_kernel::Kernel) -> bool {
    // Something another guest process is the other end of: a socket or a
    // pipe. A pipe is how apt talks to its http method and how a shell joins
    // a pipeline, and whoever fills it is a Worker in this run.
    //
    // A socket only counts when this process has no network. `FileType`
    // cannot tell a unix socket between two guests from a TCP socket carried
    // over Wisp, and the two want opposite answers: the first is ended by
    // another Worker, the second by a WebSocket message on this Worker's own
    // event loop, which a Worker asleep is not running. Where there is no
    // network there are no TCP sockets, so the question does not arise -- and
    // that is the case the X server and its client are in.
    let is_peer = |h: &Arc<dyn gex_kernel::vfs::FileHandle>| {
        use gex_kernel::vfs::FileType;
        match h.metadata().map(|m| m.file_type) {
            Ok(FileType::Fifo) => true,
            Ok(FileType::Socket) => kernel.net.is_none(),
            _ => false,
        }
    };
    // A socket the *page* is the other end of.
    //
    // `remote_sockets` is exactly that set: the renderer's vtest socket is
    // one, and a guest reading a reply off it is waiting for the page to run
    // its half. Calling that a peer wait is what made this cost rather than
    // save -- every such wait paid the bound here and then went out to
    // JavaScript and waited again.
    //
    // Compared by address. The handle in a parked thread is the same `Arc`
    // the table holds, upcast, and the data half of a fat pointer survives
    // the cast.
    let is_remote = |h: &Arc<dyn gex_kernel::vfs::FileHandle>| {
        let p = Arc::as_ptr(h) as *const ();
        kernel
            .remote_sockets
            .values()
            .any(|r| Arc::as_ptr(r) as *const () == p)
    };
    match &t.waiting {
        // A terminal: nobody but the page can type into it. Nor a socket
        // whose far end is the page.
        Waiting::Read(h) | Waiting::Write(h) => !is_peer(h) || is_remote(h),
        Waiting::Accept(h) => is_remote(h),
        // Every timed wait and every `poll` parks as this, whatever it is
        // watching -- a Wisp socket whose bytes arrive on this thread, a
        // descriptor the page serves, or a pipe between two guests. So look
        // at what it is watching.
        //
        // This used to answer "the page" for all of them, which is most of
        // what made apt slow: a `poll` over the pipes to its http method is
        // how it waits between every message, and there are thousands of
        // them. A poll whose whole set is pipes and guest sockets is ended by
        // another Worker, and that Worker's notify can end it.
        //
        // A deadline is the clock's to end, and the clock is the page's.
        // Every timed wait and every `poll` parks as this, whatever it is
        // watching. Classifying them by their descriptors was tried and it
        // stalled the gears demo halfway through -- glxgears drew for fifty
        // seconds and then stopped with its shell in `wait4` -- so until that
        // is understood the page keeps them all.
        Waiting::Net(_) => true,
        _ => false,
    }
}

/// Wait, here, for the syscall that just parked.
///
/// The syscall path already finishes a parked call without leaving wasm:
/// `gex_syscall` calls `schedule`, and `schedule` picks the thread up,
/// finishes the operation and says where to carry on. What it could not do
/// was *wait* -- with nothing ready it gave the stack back, and the wait
/// happened in JavaScript, as a poll, at whatever interval that loop was
/// turning at.
///
/// So this waits and does nothing else. It changes no kernel state, finishes
/// no operation and picks no thread; it returns when `schedule` will find
/// something ready, or when the bound passes and the page should have its
/// stack back. Everything that was hard about finishing a parked call stays
/// in the one place that already did it correctly.
fn park_until_ready() {
    if unsafe { k_present() } == 0 {
        return;
    }
    // Only when this Worker has nothing else it could run. Threads sharing a
    // Worker still take turns, and sleeping in a syscall would take the turn
    // from the sibling that could have made this very wait come true.
    if !thread_workers() && with_host(|h| h.threads.len() != 1) {
        return;
    }
    if with_host(|h| h.threads.iter().any(|t| wait_needs_page(t, &h.kernel))) {
        return;
    }
    let give_up = now_ns() + PARK_HERE_NS;
    // The nearest deadline anything is holding, so a guest that asked to be
    // woken in a hundred microseconds is not woken in twenty milliseconds.
    let soonest = with_host(|h| {
        h.threads
            .iter()
            .filter_map(|t| match &t.waiting {
                Waiting::Sleep(at) => Some(*at),
                Waiting::Futex { deadline, .. } => *deadline,
                Waiting::Net(deadline) => *deadline,
                _ => None,
            })
            .min()
    });
    loop {
        // Read before asking, so a wake that lands between the question and
        // the sleep is not slept through: the sleep is entered only while the
        // counter still reads this.
        let seq = unsafe { k_wake_seq() };
        let now = now_ns();
        if with_host(|h| h.threads.iter().any(|t| thread_ready(t, now))) {
            return;
        }
        if now >= give_up {
            return;
        }
        let mut ns = give_up - now;
        if let Some(at) = soonest {
            ns = ns.min(at.saturating_sub(now));
        }
        // A deadline already past subtracts to nothing, and a zero-nanosecond
        // wait returns the instant it is made. Never spin here.
        unsafe { k_park(seq, ns.max(1_000_000) as i64) };
    }
}

/// Sleep until another Worker does something, or `max_ms` passes.
///
/// The park loop used to be a poll: unwind out of wasm, ask "anything yet?",
/// go round the event loop, ask again. Nothing ever told it, so the interval
/// it asked at was the latency of every interaction between two guest
/// processes -- and an X client and an X server pay that twice a frame.
///
/// This blocks instead, on a word in the global kernel's memory that every
/// Worker of every process can see and that every syscall bumps. The wait is
/// bounded so a Worker still comes back to its event loop for the things only
/// the page can deliver, and it is cut short by anything with a deadline: a
/// guest asleep for a hundred microseconds must not be woken in fifty
/// milliseconds because that was the bound.
///
/// Returns the instruction's own answer: 0 woken, 1 the counter had already
/// moved, 2 timed out. -1 means there is no global kernel to wait on, and
/// the caller should go round its event loop as before.
#[no_mangle]
pub extern "C" fn gex_park(max_ms: u32) -> i32 {
    if unsafe { k_present() } == 0 {
        return -1;
    }
    let mut ns = max_ms as u64 * 1_000_000;
    // The nearest deadline any thread of this process is holding. A `sleep`
    // and a `poll` with a timeout are both waits the clock ends rather than
    // another Worker, and sleeping past them is how a program that asks to be
    // woken in a millisecond gets woken in fifty.
    let now = now_ns();
    let soonest = with_host(|h| {
        h.threads
            .iter()
            .filter_map(|t| match &t.waiting {
                Waiting::Sleep(at) => Some(*at),
                Waiting::Futex { deadline, .. } => *deadline,
                Waiting::Net(deadline) => *deadline,
                _ => None,
            })
            .min()
    });
    if let Some(at) = soonest {
        ns = ns.min(at.saturating_sub(now));
    }
    // And never nothing.
    //
    // A deadline already in the past subtracts to zero, and a zero-nanosecond
    // wait returns the instant it is made -- so a thread holding an expired
    // deadline that the scheduler has not yet acted on turns this loop as
    // fast as the Worker can go. Measured, a million turns in a minute. The
    // floor is what the old poll interval was, so a wait the clock ends is no
    // worse than it was, while a wait another Worker ends is still immediate.
    ns = ns.max(1_000_000);
    unsafe { k_park(WAKE_SEEN.load(std::sync::atomic::Ordering::Relaxed), ns as i64) }
}

#[no_mangle]
pub extern "C" fn gex_wake() -> i32 {
    // Counted, because the shape that hides is not a stall but a cycle. A
    // thread that parks, is woken a millisecond later, runs a handful of
    // instructions and parks again makes no progress and costs no CPU, and
    // resets every counter it passes on the way: the idle census never fires
    // because something *was* runnable, and the heartbeat never fires because
    // 2,000 safepoints at that rate is a very long time. What it looks like
    // is a run that has quietly stopped, which is what it is.
    //
    // On a curve from 4,096, which at a millisecond a turn is about four
    // seconds of getting nowhere.
    static WAKES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = WAKES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if n.is_power_of_two() && n >= 4096 {
        let census = with_host(|h| {
            let mut out = String::new();
            // One process per Worker now, so this Worker's threads are all
            // there are to report. The other processes are other Workers and
            // census their own.
            for (pid, p) in std::iter::once((h.kernel.pid as usize, &h.threads)) {
                for (i, t) in p.iter().enumerate() {
                    out.push_str(&format!(
                        " {pid}.{i}={}@{:#x}",
                        waiting_name(&t.waiting),
                        t.pc
                    ));
                }
            }
            out
        });
        trace_line(&format!("[gex] woken {n} times, threads:{census}\n"));
    }
    set_paused_to(0);
    schedule()
}

/// Hand the guest its standard input, from `len` bytes at `ptr` in the shim's
/// own region. Call before `gex_load`.
#[no_mangle]
pub extern "C" fn gex_set_stdin(ptr: u32, len: u32) {
    STDIN.push(shim_slice(ptr, len));
    // A page that hands over the whole of stdin at once is not going to type
    // any more, and a guest that reads to the end must see the end.
    STDIN.eof.set(true);
}

/// The guest's current `c_lflag`, so the page can follow it between cooked
/// and raw mode.
///
/// Bit 1 is ICANON and bit 3 is ECHO. While they are set the page does the
/// line editing -- collect a line, echo it, deliver it on Enter. When the
/// guest clears them it has taken over, and the page must pass keystrokes
/// through untouched and echo nothing.
#[no_mangle]
pub extern "C" fn gex_tty_lflag() -> u32 {
    with_host(|h| u32::from_le_bytes(h.kernel.termios[12..16].try_into().unwrap()))
}

/// Tell the guest how big the terminal is.
#[no_mangle]
pub extern "C" fn gex_set_winsize(rows: u32, cols: u32) {
    with_host(|h| h.kernel.winsize = (rows as u16, cols as u16));
}

/// Say that stdin is a terminal, which is what makes a shell print a prompt.
#[no_mangle]
pub extern "C" fn gex_set_tty(on: i32) {
    unsafe { STDIN_TTY = on != 0 };
}

#[no_mangle]
pub extern "C" fn gex_set_identity(uid: i32, gid: i32) {
    unsafe { IDENTITY = (uid as u32, gid as u32) };
}

/// Whether to narrate every syscall to fd 2, the browser's `--strace`.
///
/// `gex-kernel`'s own tracing writes with `eprintln!`, which on
/// `wasm32-unknown-unknown` goes nowhere, so the shim prints its own -- in the
/// same shape as the native host's, so the two traces diff directly. That
/// diff is the only way to see what a guest is doing differently under V8,
/// which is where every browser-only bug so far has been found.
static mut TRACE: bool = false;

#[no_mangle]
pub extern "C" fn gex_set_trace(on: i32) {
    unsafe { TRACE = on != 0 };
}

fn tracing() -> bool {
    unsafe { TRACE }
}

/// Write a line to fd 2 without going through the guest's descriptor table.
fn trace_line(s: &str) {
    unsafe { write(2, s.as_ptr() as u32, s.len() as u32) };
}

/// Which syscalls a run is making, and how many of each.
///
/// The third instrument of the same shape as the idle census and the wake
/// counter, and for the same reason: a run that is making no progress and a
/// run that is working can look identical from outside. This one covers the
/// case the other two miss -- a guest burning a whole core going round a
/// syscall that always answers immediately. Nothing stalls, nothing idles,
/// nothing waits; it simply never gets anywhere, at full speed.
///
/// Reported on powers of two, so a program doing ordinary work says one line
/// every so often and a program in a loop says a great many.
/// The list is trimmed to the ten commonest, because the tail is never the
/// answer.
/// The last few syscalls, in order, for when a run stops making them.
///
/// The histogram says what a run did; when it stops doing anything, the
/// question is what it did *last*. glxgears makes 4,096 syscalls and then
/// none at all while looping over a mutex, and which call it made
/// immediately before that is the whole question -- a `writev` that went out
/// and was never answered means something different from a `poll` that came
/// back empty.
const SYSLOG_N: usize = 24;
static mut SYSLOG: [(u64, u64, u64, u64); SYSLOG_N] = [(0, 0, 0, 0); SYSLOG_N];
static mut SYSLOG_AT: usize = 0;

/// The ring, oldest first, as `name(arg0, arg1) = ret` lines.
///
/// Without `with_host`: this is read from inside the guest's own write,
/// which already holds it, and re-entering panics as a bare "unreachable"
/// that unwinds the guest instead of the reporter.
#[no_mangle]
pub extern "C" fn gex_recent_syscalls(out: u32, max: u32) -> u32 {
    let mut text = String::new();
    unsafe {
        for k in 0..SYSLOG_N {
            let (nr, a0, a1, ret) = SYSLOG[(SYSLOG_AT + k) % SYSLOG_N];
            if nr == 0 && a0 == 0 && ret == 0 {
                continue;
            }
            text.push_str(&format!(
                "{}({a0:#x}, {a1:#x})={}\n",
                gex_kernel::syscalls::syscall_name(nr),
                ret as i64
            ));
        }
    }
    let n = text.len().min(max as usize);
    shim_slice_mut(out, n as u32).copy_from_slice(&text.as_bytes()[..n]);
    n as u32
}

/// Nanoseconds spent inside each syscall number, filled in by `gex_syscall`.
static mut SYS_NS: [u64; 512] = [0; 512];

fn count_syscall() {
    if unsafe { HEARTBEAT } == 0 {
        return;
    }
    // Indexed by number. aarch64 Linux stops well short of this, and anything
    // past it is a guest asking for something that does not exist.
    static mut COUNTS: [u32; 512] = [0; 512];
    static mut TOTAL: u32 = 0;
    let nr = with_host(|h| {
        let gm = GuestMem::new(guest_bytes());
        let at = h.threads[cur()].kstate.ctx_addr;
        gm.read_u64(at + ctx::x(8) as u64).unwrap_or(!0)
    });
    let (a0, a1) = with_host(|h| {
        let gm = GuestMem::new(guest_bytes());
        let at = h.threads[cur()].kstate.ctx_addr;
        (
            gm.read_u64(at + ctx::x(0) as u64).unwrap_or(0),
            gm.read_u64(at + ctx::x(1) as u64).unwrap_or(0),
        )
    });
    let total = unsafe {
        if (nr as usize) < COUNTS.len() {
            COUNTS[nr as usize] = COUNTS[nr as usize].saturating_add(1);
        }
        SYSLOG[SYSLOG_AT] = (nr, a0, a1, 0);
        SYSLOG_AT = (SYSLOG_AT + 1) % SYSLOG_N;
        TOTAL = TOTAL.wrapping_add(1);
        TOTAL
    };
    // From 512, and it has come down twice. It was 65,536, which a whole
    // glxgears run never reached; then 4,096, which apt's http method --
    // a process that lives for one download and makes a few hundred calls --
    // never reached either. A census that only speaks for long-lived
    // processes cannot say where a short one spent its time.
    if !total.is_power_of_two() || total < 512 {
        return;
    }
    let mut top: Vec<(u32, usize)> = unsafe {
        COUNTS
            .iter()
            .enumerate()
            .filter(|(_, &c)| c > 0)
            .map(|(i, &c)| (c, i))
            .collect()
    };
    top.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    top.truncate(10);
    let mut line = format!("[gex] {total} syscalls:");
    for (c, nr) in top {
        line.push_str(&format!(
            " {}={c}",
            gex_kernel::syscalls::syscall_name(nr as u64)
        ));
    }
    line.push('\n');
    trace_line(&line);
    // And the same list ordered by time rather than by count. The two answer
    // different questions, and for a run that is slow rather than busy it is
    // always this one: a single call made twice a frame and taking fifteen
    // milliseconds is invisible in a census and is the whole story here.
    let mut slow: Vec<(u64, usize)> = unsafe {
        SYS_NS
            .iter()
            .enumerate()
            .filter(|(_, &t)| t > 0)
            .map(|(i, &t)| (t, i))
            .collect()
    };
    slow.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    slow.truncate(8);
    let mut line = String::from("[gex] syscall ms:");
    for (t, nr) in slow {
        line.push_str(&format!(
            " {}={}",
            gex_kernel::syscalls::syscall_name(nr as u64),
            t / 1_000_000
        ));
    }
    line.push('\n');
    trace_line(&line);
}

/// `svc #0`.
///
/// Returns normally when the guest should carry straight on, which is almost
/// every syscall. Anything else -- a redirect, an exit -- traps out to JS
/// instead, the same way the native host unwinds out of wasm: there is no
/// other way for a callee to say "abandon this stack", and it keeps JS out of
/// the common path entirely.
#[no_mangle]
pub extern "C" fn gex_syscall(ctx: i32, pc: i64) {
    if tracing() {
        let gm = GuestMem::new(guest_bytes());
        let base = with_host(|h| h.threads[cur()].kstate.ctx_addr);
        let g = |i: usize| gm.read_u64(base + ctx::x(i as u8) as u64).unwrap_or(0);
        let n = g(8);
        let who = cur();
        // The path, for the calls whose whole meaning is which file. A trace
        // of `execve(0x2805ef70, ...)` says a program was run and not which,
        // and a run with four of them is four questions rather than an
        // answer. `openat` puts the path in the second argument, the rest in
        // the first.
        let path = |at: u64| {
            gm.read_cstr(at, 255)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default()
        };
        use gex_kernel::syscalls::nr;
        let named = match n {
            nr::EXECVE | nr::STATX => Some(path(g(0))),
            nr::OPENAT | nr::UNLINKAT | nr::RENAMEAT | nr::NEWFSTATAT => Some(path(g(1))),
            _ => None,
        };
        // The pid, not only the thread. Every process in a run writes its
        // trace to the same stderr, so without it a fork is unreadable: two
        // clones, two execves and a wait4 all appear as one sequence and
        // there is no way to say which process is waiting for which. That is
        // what made #88 take three passes to localise.
        let whose_pid = with_host(|h| h.kernel.pid);
        trace_line(&format!(
            "[gex:{whose_pid}] t{who} pc=0x{pc:x} {}({:#x}, {:#x}, {:#x}){} ...\n",
            gex_kernel::syscalls::syscall_name(n),
            g(0),
            g(1),
            g(2),
            named.map_or(String::new(), |p| format!(" \"{p}\"")),
        ));
        // For a gather write, what the kernel sees at each iovec. A wrong
        // byte here and a right one in the guest means the two disagree about
        // the memory they share, which is the failure mode this host has.
        if n == 66 {
            for i in 0..g(2).min(8) {
                let e = g(1) + i * 16;
                let (base, len) = (
                    gm.read_u64(e).unwrap_or(0),
                    gm.read_u64(e + 8).unwrap_or(0).min(32),
                );
                let bytes = gm.read(base, len as usize).unwrap_or(&[]);
                trace_line(&format!("[gex]   iov[{i}] {base:#x}+{len} {bytes:02x?}\n"));
            }
        }
    }
    count_syscall();
    // Recorded before the kernel is taken, and readable without taking it.
    //
    // A crash inside a syscall leaves the kernel borrowed, so every question
    // asked afterwards -- which pc, which thread, which anything -- panics on
    // the leaked borrow and answers nothing. This one number survives, and it
    // is the one that narrows a crash from "somewhere in the runtime" to a
    // single handler.
    LAST_SYSCALL.store(
        GuestMem::new(guest_bytes())
            .read_u64(ctx as u32 as u64 + ctx::x(8) as u64)
            .unwrap_or(!0) as u32,
        std::sync::atomic::Ordering::Relaxed,
    );
    // How long each kind of syscall takes, when there is a heartbeat.
    //
    // Counts alone cannot answer the question glxgears asks: it holds a
    // steady thirty frames a second while making only a few hundred calls,
    // so whatever the frame is spending is spent *inside* one of them and
    // the census says every one is rare. A blocking call parks the Worker
    // in here, so this covers the waiting as well as the work.
    let began = if unsafe { HEARTBEAT } != 0 { now_ns() } else { 0 };
    let stop = with_host(|h| {
        let mut gm = GuestMem::new(guest_bytes());
        let kstate = h.threads[cur()].kstate.clone();
        h.kernel.enter_thread(&kstate);
        // Rebuilding the snapshot costs more than most syscalls do, and only
        // a handful can observe it, so the rest skip it.
        let nr = gm
            .read_u64(kstate.ctx_addr + ctx::x(8) as u64)
            .unwrap_or(!0);
        if !gex_kernel::skips_proc_snapshot(nr) {
            let st = Arc::clone(&h.proc_state);
            h.kernel.refresh_proc(&st.0);
        }
        let outcome = h.kernel.syscall(&mut gm, pc as u64);
        h.kernel.leave_thread(&mut h.threads[cur()].kstate);
        match outcome {
            SyscallOutcome::Continue => 0,
            SyscallOutcome::Resume(target) => {
                set_resume(target);
                1
            }
            SyscallOutcome::Exit(code) => exit_process(h, code),
            // A signal with no handler kills the *process*, not the run.
            // Setting the run's exit here is what ended `apt update`: apt
            // stops its http method with `kill(pid, SIGINT)` -- ordinary
            // shutdown, and the method installs no handler -- and the whole
            // guest went down with it, after the download had succeeded.
            // Negative for "died of a signal"; see `exit_process`.
            SyscallOutcome::Killed { signal, .. } => exit_process(h, -(signal as i32)),
            SyscallOutcome::SpawnThread {
                ctx_addr,
                tid,
                clear_child_tid,
            } => {
                // The kernel has already written the child's context; it
                // starts at the pc that context carries.
                let start = gm.read_u64(ctx_addr + ctx::PC as u64).unwrap_or(0);
                // A Worker of its own, when the host can make one.
                //
                // Both shapes keep the same table entry: `join`, the futexes
                // and `live_threads` are all counted here whether the thread
                // runs on this stack or another. What differs is who picks it
                // up -- `Elsewhere` is the state that says "not me", and
                // without it two Workers would run the same context.
                let elsewhere = thread_workers();
                h.threads.push(Thread {
                    kstate: ThreadState {
                        ctx_addr,
                        tid,
                        clear_child_tid,
                    },
                    pc: start,
                    waiting: if elsewhere {
                        Waiting::Elsewhere
                    } else {
                        Waiting::Runnable
                    },
                    blocked_on: None,
                });
                if elsewhere {
                    stage_thread(tid, ctx_addr, start);
                    // The stack goes back so the host can see the record and
                    // start the Worker, exactly as a `fork` does. The parent
                    // resumes where it was: `clone` returns the tid to it.
                    set_resume(pc as u64 + 4);
                    set_resume_ctx(h.threads[cur()].kstate.ctx_addr);
                    return 1;
                }
                // The parent carries straight on, as it does on Linux. The
                // child waits for the first time the parent gives up the
                // stack, which is what a safepoint or a blocking call is.
                0
            }
            SyscallOutcome::ThreadExit => {
                h.threads[cur()].waiting = Waiting::Exited;
                3
            }
            SyscallOutcome::Blocking(op) => {
                let waiting = match &op {
                    gex_kernel::BlockOp::FutexWait {
                        table,
                        uaddr,
                        deadline,
                        ..
                    } => Waiting::Futex {
                        parked: table.park(*uaddr),
                        deadline: deadline.map(|d| now_ns() + d.as_nanos() as u64),
                    },
                    gex_kernel::BlockOp::Sleep(d) => Waiting::Sleep(now_ns() + d.as_nanos() as u64),
                    gex_kernel::BlockOp::ReadStream { handle, .. } => {
                        Waiting::Read(Arc::clone(handle))
                    }
                    gex_kernel::BlockOp::WriteStream { handle, .. } => {
                        Waiting::Write(Arc::clone(handle))
                    }
                    // Both of these wait on something outside the guest, so
                    // both give the stack back and are re-checked by the
                    // scheduler on its way round.
                    gex_kernel::BlockOp::Connect { .. } => Waiting::Net(None),
                    gex_kernel::BlockOp::Poll { deadline, .. } => {
                        Waiting::Net(deadline.map(|d| now_ns() + d.as_nanos() as u64))
                    }
                    // `WNOHANG` answers straight away; anything else waits
                    // for the child, which may not have started yet.
                    gex_kernel::BlockOp::WaitChild { nohang, .. } => {
                        if *nohang {
                            Waiting::Runnable
                        } else {
                            Waiting::Child
                        }
                    }
                    // A listener with nothing waiting sleeps until something
                    // does, rather than being asked again and answering
                    // EAGAIN to a caller that did not ask for it.
                    gex_kernel::BlockOp::AcceptWait { socket } => {
                        Waiting::Accept(Arc::clone(socket))
                    }
                    // `sigsuspend` waits for a signal and nothing else. The
                    // scheduler is where a signal is delivered, so coming
                    // back round each turn is the whole implementation: if
                    // one arrived, delivery happens before this thread runs
                    // its next instruction, and if not the turn is given up
                    // again.
                    gex_kernel::BlockOp::SigSuspend { .. } => Waiting::Runnable,
                };
                let t = &mut h.threads[cur()];
                t.pc = pc as u64 + 4;
                t.waiting = waiting;
                t.blocked_on = Some(op);
                3
            }
            SyscallOutcome::SpawnProcess {
                pid,
                child_stack,
                tls,
            } => {
                do_fork(h, pid, pc as u64, child_stack, tls);
                // Give the stack back, even though the parent is about to
                // carry straight on with it.
                //
                // Because what the child gets is a *copy of this memory*, and
                // the copy has to be taken now. The host takes it where the
                // stack comes back; letting the parent run on first meant the
                // child was made from the parent's memory several syscalls
                // later, with a context saved over by the parent's next
                // syscall -- a child whose registers were its parent's and
                // whose `rt_sigprocmask` was handed a pointer of 2.
                set_resume(pc as u64 + 4);
                set_resume_ctx(h.threads[cur()].kstate.ctx_addr);
                1
            }
            SyscallOutcome::Exec { path, argv, envp } => {
                let entry = do_exec(h, &path, &argv, &envp);
                if entry == 0 {
                    // The kernel checked the path before saying `Exec`, so a
                    // failure here is the loader's: not an ELF, or one this
                    // cannot place. `ENOEXEC` is what the guest expects.
                    let mut gm = GuestMem::new(guest_bytes());
                    let ctx_addr = h.threads[cur()].kstate.ctx_addr;
                    let _ = gm.write_u64(
                        ctx_addr + ctx::x(0) as u64,
                        Errno::ENOEXEC.as_return(),
                    );
                    0
                } else {
                    set_resume(entry);
                    set_resume_ctx(h.threads[cur()].kstate.ctx_addr);
                    1
                }
            }
            _ => {
                set_exit(h, 128 + 31);
                2
            }
        }
    });
    if began != 0 {
        let nr = LAST_SYSCALL.load(std::sync::atomic::Ordering::Relaxed) as usize;
        if nr < 512 {
            unsafe { SYS_NS[nr] = SYS_NS[nr].saturating_add(now_ns() - began) };
        }
    }
    // Whatever that call did, somebody else may have been waiting for it.
    //
    // After every syscall rather than after the ones that obviously matter.
    // A write into a socket, a write into a pipe, an exit, a signal, a futex
    // wake, a file appearing where another process is waiting for it -- the
    // list is long, it is not obviously closed, and being wrong about one
    // entry is a process that sleeps through its wake-up and a run that
    // stops. The notify costs nothing when nobody is parked.
    if unsafe { k_present() } != 0 {
        unsafe { k_wake() };
    }
    // Objects the guest has just mapped, statically recompiled in bulk
    // before it can branch into one.
    //
    // Here rather than inside the `with_host` above because recompiling
    // takes the host lock itself, and because it must happen after the
    // syscall that created the mapping has finished -- the loader maps a
    // library and then jumps into it, and a translation that arrives late is
    // a translation the dispatch miss got to first.
    // Objects the guest has just mapped, statically recompiled in bulk
    // before it can branch into one.
    //
    // The modules are only queued. Nothing is instantiated here and the
    // guest is not interrupted, because it cannot reach any of this yet: a
    // page's directory entry is published when its module is installed, and
    // until then every branch into it still finds the miss stub. JavaScript
    // picks the queue up at the next trap, which is the next miss.
    //
    // Interrupting was tried and is wrong. Trapping out with a resume of
    // `pc + 4` looks right -- that is the address the emitter records before
    // the `svc`, and `Term::Syscall` flushes the registers first -- and it
    // faulted every time, at exactly that instruction, on the `mmap` that
    // mapped the library:
    //
    //     0x1b854  mov x8, #222      (mmap)
    //     0x1b858  svc #0
    //     0x1b85c  cmn x0, #0x1000   <- memory access out of bounds
    //
    // Re-entering translated code at an arbitrary address is not the same as
    // falling out of the bottom of the block that called the syscall, and
    // the difference is not worth paying for something that did not need to
    // happen now.
    recompile_objects();
    // 3 means the running thread gave up the wasm stack: pick another.
    //
    // With a wait first, when another Worker is what it is waiting for. See
    // `park_until_ready`: the point of it is that `schedule` then finds the
    // thread ready and finishes the call, so nothing has to unwind at all.
    let stop = if stop == 3 {
        // Ask first, wait second.
        //
        // Most parked calls are not waiting at all by the time they get here
        // -- the pipe already has room, the child has already exited -- and
        // `schedule` answers those without anything sleeping. Waiting first
        // put a second readiness scan in front of every one of them, and that
        // scan is not free: it asks each handle, and a handle on a shared
        // socket is a call into the global kernel. Measured, that cost more
        // than the unwinding it was there to avoid.
        let mut picked = schedule();
        if picked == 2 {
            park_until_ready();
            picked = schedule();
        }
        // The thread that carried on is the one that made this call, and the
        // place it carries on from is the instruction after its `svc` -- which
        // is the bottom of this very block. Returning to it is the same thing
        // as being re-entered at it, minus the trap, the catch and the
        // re-entry, and minus a round of the page's event loop.
        //
        // Only when it is exactly that. A signal delivered as the call
        // finished resumes at a handler instead, and a different thread
        // resumes with a different context; both of those are re-entries and
        // have to go out through JavaScript the way they always did.
        let here = with_host(|h| h.threads[cur()].kstate.ctx_addr);
        if picked == 1
            && resume() == Some(pc as u64 + 4)
            && resume_ctx().unwrap_or(here) == here
        {
            set_resume_none();
            set_resume_ctx_none();
            0
        } else {
            picked
        }
    } else {
        stop
    };
    // What it answered, into the slot the entry took. The ring is written
    // before the call because a call that never returns is exactly the one
    // worth seeing; this fills the answer in for the ones that do.
    if unsafe { HEARTBEAT } != 0 {
        let x0 = with_host(|h| {
            let gm = GuestMem::new(guest_bytes());
            let at = h.threads[cur()].kstate.ctx_addr;
            gm.read_u64(at + ctx::x(0) as u64).unwrap_or(0)
        });
        unsafe {
            let last = (SYSLOG_AT + SYSLOG_N - 1) % SYSLOG_N;
            SYSLOG[last].3 = x0;
        }
    }
    if tracing() {
        let gm = GuestMem::new(guest_bytes());
        let (i, ctx_addr) = with_host(|h| (cur(), h.threads[cur()].kstate.ctx_addr));
        let x0 = gm.read_u64(ctx_addr + ctx::x(0) as u64).unwrap_or(0);
        trace_line(&format!("[gex]   t{i} = {}\n", x0 as i64));
    }
    if stop != 0 {
        trap();
    }
}


/// Everything below the guest's image: the register contexts, the exclusive
/// monitor scratch, and the dispatch directory. None of it is in the memory
/// map, and all of it is per-process.
const LOW_END: u64 = gex_abi::DISPATCH_DIR + gex_abi::DISPATCH_MAX_PAGES * 4;

// The snapshot switcher was here: `Region`, `snapshot_memory`,
// `restore_memory`, `clear_mapped`, `switch_process` and `install_process`.
//
// It existed because every guest process shared one address space, and that
// was forced by a detail rather than a design: the state processes are
// supposed to share -- the process table, pipes, sockets -- is made of
// pointers, and a pointer means a memory. Sharing them meant sharing an
// address space, which meant two processes' libraries landing on each other,
// which meant writing one out and reading the other in. 33 MiB a switch, and
// 205 MiB over a single `apt update`.
//
// That state now lives in the global kernel module, whose memory every Worker
// instantiates against, where a pipe is an id rather than an `Arc`. Processes
// no longer need to share an address space, so there is nothing to save and
// nothing to restore.
//
// What has to replace it, and does not exist yet:
//
//   * `fork` asks the host for a Worker with a copy of the parent's mapped
//     regions, rather than snapshotting the parent in place.
//   * `wait4`, `kill` and `exit` route through the global kernel's process
//     table (`k_spawn`, `k_wait`, `k_kill`, `k_exit`) instead of a local one.
//   * the scheduler stops choosing between processes, because each one is a
//     Worker the engine schedules.

fn do_fork(h: &mut Host, pid: i32, pc: u64, child_stack: u64, tls: Option<u64>) {
    let ctx_addr = h.threads[cur()].kstate.ctx_addr;
    // The parent carries straight on, as it does on Linux, and -- now that
    // there is no switcher -- without being written out first. It keeps its
    // memory, its threads and its dispatch tables, because the child is not
    // going to be using them.
    h.threads[cur()].pc = pc + 4;
    h.threads[cur()].waiting = Waiting::Runnable;
    h.threads[cur()].blocked_on = None;

    // The child is a *Worker*: its own memory, its own shim, its own tables.
    // What it needs from here is the parent's mapped regions copied into that
    // memory, and a context to start at -- `pc + 4`, with `x0` of zero, on
    // `child_stack` if `clone` named one.
    //
    // FORK_PENDING is what the host reads to build it. Nothing constructs the
    // Worker yet, which is the part of this that is unfinished: the queue
    // fills and nobody drains it, so a forking program now runs its parent
    // and never its child.
    //
    // The host reads these, builds the Worker, and that Worker calls
    // `gex_adopt_child` to become the process.
    stage_fork_fds(h);
    FORK[1].store(pc + 4, std::sync::atomic::Ordering::Relaxed);
    FORK[2].store(ctx_addr, std::sync::atomic::Ordering::Relaxed);
    FORK[3].store(child_stack, std::sync::atomic::Ordering::Relaxed);
    FORK[4].store(tls.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
    FORK[5].store(h.kernel.mem.brk_current(), std::sync::atomic::Ordering::Relaxed);
    // Last, because it is what the host tests: the rest must be readable by
    // the time a non-zero pid appears.
    FORK[0].store(pid as u64, std::sync::atomic::Ordering::Release);
}

/// The child's descriptor table, as text, staged for the host to carry.
///
/// A `fork` on Linux gives the child the parent's open-file descriptions --
/// the same offsets, the same pipe ends. `FdTable::clone_table` does exactly
/// that when the child shares the memory, and means nothing when it does not:
/// an `Arc<OpenFile>` is a pointer into a heap the child's Worker has never
/// seen.
///
/// So the child is sent a *description* of each descriptor and rebuilds it.
/// Only the ones that can be rebuilt travel, which is the honest limit of
/// this: a file in the shared root and a pipe in the global kernel both can,
/// because both are an id or a path that means the same thing in any process.
/// A device node or a host socket cannot, and is left closed rather than
/// silently replaced by something that is not it.
///
/// One line per descriptor:
///
///     <fd> <cloexec> <flags> <offset> <path>
///
/// Space-separated because a guest path may contain anything but a newline,
/// so the path goes last and everything before it is a number.
static FORK_FDS: SingleThreaded<String> = SingleThreaded(const { RefCell::new(String::new()) });

/// Describe the calling process's descriptors for the child.
fn stage_fork_fds(h: &Host) {
    let mut out = String::new();
    for (fd, file, cloexec) in h.kernel.fds.entries() {
        // A pipe end has to be opened again in the global kernel, or the
        // parent closing its copy would look like the last reference going
        // away and the reader downstream would see an end of file that has
        // not happened.
        if let Some(id) = pipe_id(&file.path) {
            let write = file.writable;
            unsafe { k_pipe_open_end(id, write as i32) };
        } else if !file.path.starts_with('/') {
            // Not a path and not a pipe: standard input and output, a device,
            // a socket. Nothing here can rebuild those from a name.
            continue;
        }
        out.push_str(&format!(
            "{} {} {} {} {}\n",
            fd,
            cloexec as i32,
            file.flags(),
            file.offset(),
            file.path
        ));
    }
    *FORK_FDS.0.borrow_mut() = out;
}

/// The pipe id in a path like `pipe:[7]`, if that is what it is.
fn pipe_id(path: &str) -> Option<i32> {
    path.strip_prefix("pipe:[")?.strip_suffix(']')?.parse().ok()
}

/// Where the child's descriptor table is staged, and how long it is.
#[no_mangle]
pub extern "C" fn gex_fork_fds_ptr() -> u32 {
    FORK_FDS.0.borrow().as_ptr() as u32
}

#[no_mangle]
pub extern "C" fn gex_fork_fds_len() -> u32 {
    FORK_FDS.0.borrow().len() as u32
}

/// Rebuild the descriptors the parent had, in the child.
///
/// Called once, by the child's own Worker, straight after `gex_adopt_child`.
/// Standard input, output and error are already there -- the kernel makes
/// them -- so a line naming one of those replaces it, which is what a shell
/// redirecting a child's output needs.
#[no_mangle]
pub extern "C" fn gex_adopt_fds(ptr: u32, len: u32) -> i32 {
    let Ok(text) = std::str::from_utf8(shim_slice(ptr, len)) else {
        return -1;
    };
    let text = text.to_string();
    with_host(|h| {
        let mut n = 0;
        for line in text.lines() {
            let mut parts = line.splitn(5, ' ');
            let Some(fd) = parts.next().and_then(|v| v.parse::<i32>().ok()) else {
                continue;
            };
            let Some(cloexec) = parts.next().and_then(|v| v.parse::<i32>().ok()) else {
                continue;
            };
            let Some(flags) = parts.next().and_then(|v| v.parse::<u64>().ok()) else {
                continue;
            };
            let Some(offset) = parts.next().and_then(|v| v.parse::<u64>().ok()) else {
                continue;
            };
            let Some(path) = parts.next() else { continue };
            let file = if let Some(id) = pipe_id(path) {
                let Some(r) = global_pipes() else { continue };
                // The parent already opened this end on the child's behalf,
                // before the copy was taken; nothing to open here.
                let write = flags & gex_kernel::fs::oflags::ACCMODE
                    != gex_kernel::fs::oflags::O_RDONLY;
                gex_kernel::file::OpenFile::new(
                    Arc::new(gex_kernel::pipe::RemotePipeEnd {
                        id,
                        write,
                        remote: r,
                    }),
                    path.to_string(),
                    flags,
                    !write,
                    write,
                )
            } else {
                use gex_kernel::vfs::FileSystem;
                let Some(root) = global_root() else { continue };
                let Ok(handle) = root.open(path.trim_start_matches('/'), flags, 0) else {
                    continue;
                };
                let acc = flags & gex_kernel::fs::oflags::ACCMODE;
                let f = gex_kernel::file::OpenFile::new(
                    handle,
                    path.to_string(),
                    flags,
                    acc != gex_kernel::fs::oflags::O_WRONLY,
                    acc != gex_kernel::fs::oflags::O_RDONLY,
                );
                f.set_offset(offset);
                f
            };
            if h.kernel.fds.install(fd, file, cloexec != 0).is_ok() {
                n += 1;
            }
        }
        n
    })
}

/// A child `fork` has made that the host has not built a Worker for:
/// pid, entry pc, context address, child stack, TLS.
///
/// Plain words rather than a queue. `fork` returns to the parent immediately
/// and the host drains this before the parent runs again, so there is only
/// ever one.
static FORK: [std::sync::atomic::AtomicU64; 6] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 6];

/// The pid of a child needing a Worker, or zero -- and claiming it.
///
/// Claimed rather than merely read, for the reason `gex_thread_pending`
/// spells out: a process with threads has several Workers looking at one
/// record, and a `fork` seen twice is a child started twice.
#[no_mangle]
pub extern "C" fn gex_fork_pending() -> i32 {
    let pid = FORK[0].swap(0, std::sync::atomic::Ordering::AcqRel) as i32;
    if pid != 0 {
        CLAIMED_FORK.with(|c| {
            let mut v = [0u64; 5];
            for (i, slot) in v.iter_mut().enumerate() {
                *slot = FORK[i + 1].load(std::sync::atomic::Ordering::Relaxed);
            }
            c.set(v);
        });
    }
    pid
}

/// The four words that go with a claimed pid, plus the break.
thread_local! {
    static CLAIMED_FORK: std::cell::Cell<[u64; 5]> = const { std::cell::Cell::new([0; 5]) };
}

fn claimed_fork(i: usize) -> i64 {
    CLAIMED_FORK.with(|c| c.get()[i]) as i64
}

#[no_mangle]
pub extern "C" fn gex_fork_pc() -> i64 {
    claimed_fork(0)
}

#[no_mangle]
pub extern "C" fn gex_fork_ctx() -> i64 {
    claimed_fork(1)
}

#[no_mangle]
pub extern "C" fn gex_fork_sp() -> i64 {
    claimed_fork(2)
}

#[no_mangle]
pub extern "C" fn gex_fork_tls() -> i64 {
    claimed_fork(3)
}

/// Where the parent's heap had reached.
///
/// A child inherits the break along with the memory behind it, and rebuilding
/// its kernel with a break of zero is not a smaller heap -- it is a heap that
/// starts at address zero and grows over the register contexts. apt's forked
/// helper called `brk` three instructions in, got an address in the reserved
/// low region, and the next `malloc` wrote its bookkeeping over the context
/// the child was running on: `x30=0, sp=0` and a branch to `pc=0`.
#[no_mangle]
pub extern "C" fn gex_fork_brk() -> i64 {
    claimed_fork(4)
}

/// Where this Worker re-enters the guest, and why it stopped.
///
/// Per Worker for the same reason as `CURRENT`: they describe what one
/// Worker is doing, and a field on the shared `Host` meant one thread's
/// resume address overwrote another's. What came out was a Worker resuming
/// at zero -- `nothing to translate at pc=0x0` with a perfectly good pc
/// sitting in its own context.
thread_local! {
    static RESUME: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    static RESUME_CTX: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    /// Why the guest gave the stack back when it was not an exit. 1: a thread
    /// wants a keystroke. 2: a thread wants the page's event loop to turn.
    static PAUSED: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

fn resume() -> Option<u64> {
    RESUME.with(|c| c.get())
}

fn set_resume(v: u64) {
    RESUME.with(|c| c.set(Some(v)));
}

fn set_resume_none() {
    RESUME.with(|c| c.set(None));
}

fn resume_ctx() -> Option<u64> {
    RESUME_CTX.with(|c| c.get())
}

fn set_resume_ctx(v: u64) {
    RESUME_CTX.with(|c| c.set(Some(v)));
}

fn set_resume_ctx_none() {
    RESUME_CTX.with(|c| c.set(None));
}

fn paused() -> u8 {
    PAUSED.with(|c| c.get())
}

fn set_paused_to(v: u8) {
    PAUSED.with(|c| c.set(v));
}

/// Which thread of this process *this Worker* is running.
///
/// Per Worker rather than per process, and that is the whole of what makes a
/// thread a Worker: the thread table is shared -- it is in the memory, next
/// to the kernel -- but which entry of it a Worker is on cannot be, or two
/// Workers would be running the same registers.
///
/// It is a thread-local in both builds. Against a plain memory there is one
/// Worker per process and this is exactly the field it replaces; against a
/// shared one there are several and a field would have been the bug.
thread_local! {
    static CURRENT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The thread this Worker is running.
fn cur() -> usize {
    CURRENT.with(|c| c.get())
}

/// Run thread `i` from here on. `h` is not read; it is taken to keep every
/// caller honest about holding the kernel when it switches threads.
fn set_current(_h: &mut Host, i: usize) {
    CURRENT.with(|c| c.set(i));
}

/// A thread `clone` has made that the host has not made a Worker for:
/// tid, context address, entry pc.
///
/// The same shape as `FORK` and for the same reason: `clone` returns to the
/// parent immediately and the host drains this before the parent runs again,
/// so there is only ever one.
static THREAD: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Whether the host will make a Worker per thread.
///
/// Off unless said otherwise, because it needs a memory every Worker can
/// share -- which is the shared build, and not what a page loads by default.
/// With it off, `clone` schedules on this Worker's stack as it always did.
static THREAD_WORKERS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn thread_workers() -> bool {
    THREAD_WORKERS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Say whether this process's threads get Workers of their own.
///
/// Told rather than guessed: only the host knows whether the memory it built
/// is shared and whether it can make a Worker at all.
#[no_mangle]
pub extern "C" fn gex_set_thread_workers(on: i32) {
    THREAD_WORKERS.store(on != 0, std::sync::atomic::Ordering::Relaxed);
}

fn stage_thread(tid: i32, ctx_addr: u64, pc: u64) {
    THREAD[1].store(ctx_addr, std::sync::atomic::Ordering::Relaxed);
    THREAD[2].store(pc, std::sync::atomic::Ordering::Relaxed);
    // Last, because it is what the host tests.
    THREAD[0].store(tid as u64, std::sync::atomic::Ordering::Release);
}

/// What this Worker claimed, so the three words it asks for afterwards are
/// the ones it took rather than whatever is staged by then.
thread_local! {
    static CLAIMED_THREAD: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

/// The tid of a thread needing a Worker, or zero -- and claiming it.
///
/// A claim rather than a look, because with threads a process has several
/// Workers and this record is in the memory all of them share: two Workers
/// that both saw the same staged thread would both start one, and two Workers
/// on one set of registers is the bug this whole design is arranged to
/// prevent. The swap is what makes exactly one of them win.
#[no_mangle]
pub extern "C" fn gex_thread_pending() -> i32 {
    let tid = THREAD[0].swap(0, std::sync::atomic::Ordering::AcqRel) as i32;
    if tid != 0 {
        CLAIMED_THREAD.with(|c| {
            c.set((
                THREAD[1].load(std::sync::atomic::Ordering::Relaxed),
                THREAD[2].load(std::sync::atomic::Ordering::Relaxed),
            ))
        });
    }
    tid
}

#[no_mangle]
pub extern "C" fn gex_thread_ctx() -> i64 {
    CLAIMED_THREAD.with(|c| c.get().0) as i64
}

#[no_mangle]
pub extern "C" fn gex_thread_pc() -> i64 {
    CLAIMED_THREAD.with(|c| c.get().1) as i64
}

/// Kept so a host built against the older shape still links. The claim above
/// has already cleared the record.
#[no_mangle]
pub extern "C" fn gex_thread_taken() {}

/// Become the thread `tid`, in a Worker that has attached to this process's
/// memory.
///
/// Nothing is built here and nothing is copied: a thread shares everything
/// with the process it belongs to, and all of it -- the kernel, the file
/// table, the memory map -- is already in the memory this Worker attached to.
/// What is per-Worker is which entry of the thread table this Worker runs,
/// and that is what this sets.
///
/// Returns the pc to enter at, or zero if there is no such thread.
#[no_mangle]
pub extern "C" fn gex_adopt_thread(tid: i32) -> i64 {
    with_host(|h| {
        let Some(i) = h.threads.iter().position(|t| t.kstate.tid == tid) else {
            return 0;
        };
        h.threads[i].waiting = Waiting::Runnable;
        set_current(h, i);
        h.threads[i].pc as i64
    })
}

/// Kept so a host built against the older shape still links; the claim in
/// `gex_fork_pending` has already cleared the record.
#[no_mangle]
pub extern "C" fn gex_fork_taken() {}

/// Become the forked child, in a Worker holding a copy of the parent's memory.
///
/// The host has already made this Worker's memory and copied every mapped
/// region of the parent into it, so the guest's image, stack and heap are all
/// here -- what is missing is a kernel that says this is process `pid`, and
/// the handful of context words that differ between a parent and its child.
///
/// Those words are written here rather than by the parent, because the parent
/// must not touch them: it is still using that context itself, and the copy
/// the child received was taken while both were the same process.
///
/// Returns the pc to enter at.
#[no_mangle]
pub extern "C" fn gex_adopt_child(
    pid: i32,
    ctx_addr: i64,
    pc: i64,
    sp: i64,
    tls: i64,
    brk: i64,
) -> i64 {
    let ctx_addr = ctx_addr as u64;
    {
        let mut gm = GuestMem::new(guest_bytes());
        // What `fork` returns in the child.
        let _ = gm.write_u64(ctx_addr + ctx::x(0) as u64, 0);
        if sp != 0 {
            // `posix_spawn` clones onto a stack it allocated; a plain `fork`
            // passes zero and keeps the parent's.
            let _ = gm.write_u64(ctx_addr + ctx::SP as u64, sp as u64);
        }
        if tls != 0 {
            let _ = gm.write_u64(ctx_addr + ctx::TPIDR as u64, tls as u64);
        }
        // A monitor is per-thread and the child's starts clear.
        let _ = gm.write_u64(ctx_addr + ctx::MON_SIZE as u64, 0);
    }
    // A kernel of its own. The child's *memory* arrived as a copy; its kernel
    // state did not, because that is Rust data in the parent's Worker and no
    // amount of copying guest bytes brings it over.
    //
    // What is rebuilt here is the part the child cannot run without: its
    // identity, its break, and -- through `gex_adopt_vma`, which the host
    // calls once per region straight after this -- its memory map. Without
    // the map a child's first `mmap` hands out addresses its image is already
    // using.
    //
    // What is *not* rebuilt, and is the next thing needed: the inherited file
    // descriptors. The child gets the three standard ones and nothing else,
    // so a shell pipeline's child still cannot read the pipe it was forked
    // to read. That wants the file table in the global kernel, next to the
    // pipes it already holds.
    // With the parent's break, not zero. The memory behind it came over in
    // the image; a kernel that thinks the heap is empty hands the guest's next
    // `malloc` an address at the bottom of the address space, which is where
    // the register contexts are.
    let mut kernel = Kernel::new(ctx_addr, brk as u64, String::new());
    kernel.mem.grows_on_demand(arena_grow, arena_back);
    // The same table the parent has, because it is not in either of their
    // memories. This is what makes the parent's `wait4` able to find a child
    // it can no longer see: the child publishes its exit into a table both
    // Workers are calling into, and the parent parked on exactly that.
    if let Some(procs) = global_procs() {
        kernel.procs = procs;
        // And the layout they share. Only with a global kernel: without one
        // this process is the whole system and has nobody to agree with.
        kernel.place = Some(std::sync::Arc::new(SharedPlaces));
    }
    kernel.pipes = global_pipes();
    kernel.unix_remote = global_sockets();
    install_console(&mut kernel);
    kernel.pid = pid;
    kernel.tid = pid;
    // The system's filesystem, which the child could not otherwise have: its
    // parent's `TmpFs` was Rust data in a memory this Worker cannot see, so
    // without this a forked child's root is empty and its `execve` cannot
    // find the program it was forked to run.
    if let Some(r) = global_root() {
        kernel.vfs.mount("/", r);
    }
    // And the mounts that are not files at all. A child inherits its parent's
    // whole namespace on Linux, and here every process builds its own -- so a
    // forked child that was given only the root had no `/dev/null` for a
    // shell to redirect into and no `/proc/self/maps` for libc to read. The
    // gears demo is a script that does both in its first two lines.
    let virgl = mount_devices(&mut kernel);
    let proc_state = mount_procfs(&mut kernel);
    kernel.clock = browser_clock;
    kernel.net = unsafe { HAVE_NET }.then(|| Arc::new(WispNet) as Arc<dyn net::NetStack>);
    set_host(Host {
        kernel,
        virgl,
        root: Arc::new(gex_kernel::vfs::TmpFs::new()),
        proc_state,
        // The child is one thread: `fork` does not carry the parent's others
        // across, and in this design they were never in this Worker anyway.
        threads: vec![Thread {
            kstate: ThreadState::new(ctx_addr, pid),
            pc: pc as u64,
            waiting: Waiting::Runnable,
            blocked_on: None,
        }],
        exit: None,
        // Built from the module's `gex.meta`, the same as `gex_load` does:
        // runtime translation carries on where the ahead-of-time build
        // stopped, and a child that started from an empty table would report
        // "nothing to translate" at the first address its parent had not
        // reached either -- which for a forked child is almost immediately,
        // because `fork` returns into libc.
        jit: {
            let meta = gex_xlate::Metadata::decode(&METADATA.0.borrow()).unwrap_or_default();
            Jit {
            described: meta.next_slot != 0,
            page_table: gex_xlate::PageTable::from_entries(&meta.dir_entries, meta.next_slot),
            translated: Default::default(),
            registry: Vec::new(),
            shared: Default::default(),
            installed_mods: Default::default(),
            reuse_have: 0,
            reuse_hit: 0,
            reuse_miss: 0,
            reuse_noident: 0,
            reuse_served: 0,
            intrinsics: Default::default(),
            intrinsics_scanned: Default::default(),
            candidates: Vec::new(),
            pending: Default::default(),
            blocks: 0,
            compiles: 0,
            nanos: 0,
        }},
    });
    pc
}

/// Record one of the parent's mappings in the adopted child's memory map.
///
/// Called once per region by the host, straight after `gex_adopt_child` and
/// before the child runs. The bytes are already there -- the host copied them
/// -- but the *map* is Rust state that did not travel, and a child whose map
/// is empty will `mmap` straight over its own image.
#[no_mangle]
pub extern "C" fn gex_adopt_vma(start: i64, end: i64, prot: i32) {
    with_host(|h| {
        h.kernel
            .mem
            .insert(start as u64, end as u64, prot as u32, "[forked]");
    });
}

/// This process has exited. Hand the stack to another, or end the run.
///
/// Returns 1 when another process took over, 2 when this was the last one.
/// End the running process and give the stack to another.
///
/// `code` is an exit status, or the negated signal number when the process
/// died of one -- the two are different things to `wait4`, which reports a
/// normal exit as `code << 8` and a signal death as the bare signal, and a
/// parent tells them apart with `WIFEXITED`/`WIFSIGNALED`. Carrying the sign
/// is enough to keep them apart without widening everything that passes a
/// status along.
fn exit_process(h: &mut Host, code: i32) -> i32 {
    // Published before anything else, because the table is shared between
    // every kernel and this is the record a parent's `wait4` will find --
    // and the parent may be parked on exactly that.
    let pid = h.kernel.pid;
    // The descriptors go first, and they have to go explicitly.
    //
    // A process exits here by trapping out of the wasm stack, and a trap does
    // not unwind: nothing is dropped, so a pipe end this process held stayed
    // held for the life of the tab. On one address space that was invisible
    // -- the reader was the same `Pipe` object and could see the writer had
    // gone -- but a pipe between two Workers lives in the global kernel and
    // only learns about a closed end from `Drop`. So `apt` forked `dpkg`,
    // `dpkg` ran and exited, and `apt` sat on `read(4, ...)` waiting for an
    // end-of-file that nothing was ever going to send.
    h.kernel.fds = gex_kernel::file::FdTable::new();
    h.kernel.remote_sockets.clear();
    h.kernel.procs.exit(pid, code);
    // This Worker ran one process and that process is over, so the Worker is
    // over. There is nothing to hand the stack to: the other processes are
    // other Workers, already running, and the parent's `wait4` finds the
    // status in the shared process table rather than by being switched to.
    //
    // That is the whole of what the switcher's exit path did, and all of it
    // is now somebody else's job.
    if tracing() {
        trace_line(&format!("[proc] exit {pid} with {code}\n"));
    }
    // A shell reports a signal death as 128 + the signal, and so does this.
    set_exit(h, if code < 0 { 128 - code } else { code });
    2
}

/// `execve`: replace this process's image with another program's.
///
/// Everything about the process that is not the image survives -- pid,
/// parent, descriptors that are not close-on-exec, the working directory --
/// which is what makes the shell's fork-then-exec work at all.
///
/// Returns the entry point, or zero if the program could not be loaded, in
/// which case the caller has already been given an errno.
fn do_exec(h: &mut Host, path: &str, argv: &[String], envp: &[String]) -> u64 {
    let Some(bytes) = h.kernel.read_file(path) else {
        return 0;
    };
    let read = |p: &str| h.kernel.read_file(p);
    let program = match gex_ld::load_program(&bytes, gex_abi::PIE_BASE, INTERP_BASE, &read) {
        Ok(p) => p,
        Err(_) => return 0,
    };
    h.kernel.close_on_exec();
    // Handlers are addresses in the image being replaced; see Signals::exec.
    h.kernel.signals.exec();
    // The old image goes, and not only from the memory map.
    //
    // On a real kernel `execve` gets a new address space and everything the
    // old program had is gone. Here there is one address space, so anything
    // not deliberately cleared is still sitting in it -- and the next program
    // maps its libraries at the same addresses, because a fresh map allocates
    // from the same place. What it finds there is the previous program's
    // *relocated* copy of the same library.
    //
    // That is not harmless. `sleep` after `dash` came up with its .got.plt
    // still holding link-time addresses: ld.so had decided there was nothing
    // to do, and the first call through the PLT branched to 0x15c0 -- the
    // unbiased PLT0 -- and died in unmapped memory.
    let old: Vec<(u64, u64)> = h.kernel.mem.vmas().iter().map(|v| (v.start, v.end)).collect();
    h.kernel.mem = gex_kernel::mem::MemoryMap::new(program.brk());
    // A fresh map for a fresh image, and it grows the same way.
    h.kernel.mem.grows_on_demand(arena_grow, arena_back);
    {
        let data = guest_bytes();
        for (start, end) in old {
            let (a, b) = (start as usize, (end as usize).min(data.len()));
            if a < b {
                data[a..b].fill(0);
            }
        }
    }
    h.kernel.exe_path = path.to_string();
    h.kernel.argv = argv.to_vec();
    h.kernel.envp = envp.to_vec();
    h.kernel.live_threads = 1;
    h.kernel.next_ctx_slot = 1;
    // Signal handlers do not survive an exec; the mask does.
    h.kernel.signals = gex_kernel::signal::Signals::new();

    let data = guest_bytes();
    // The new image goes where its own headers say, which for a static
    // executable is the same place the old one was.
    for seg in program.segments() {
        let (a, b) = (seg.vaddr as usize, (seg.vaddr + seg.mem_size) as usize);
        if b > data.len() {
            return 0;
        }
        data[a..b].fill(0);
        data[a..a + seg.bytes.len()].copy_from_slice(&seg.bytes);
        let mut prot = 0;
        if seg.readable {
            prot |= gex_kernel::mem::prot::READ;
        }
        if seg.writable {
            prot |= gex_kernel::mem::prot::WRITE;
        }
        if seg.executable {
            prot |= gex_kernel::mem::prot::EXEC;
        }
        // No origin. An image segment is *built* by the loader rather than
        // copied from a file at an offset -- `mem_size` runs past the file's
        // bytes and the tail is zeroed -- so it cannot be reconstructed by
        // re-reading, and a snapshot has to carry its bytes.
        //
        // Saying otherwise cost a run: the identity was recorded as the
        // segment's vaddr rather than a file offset, and putting the process
        // back reported `[initial] is gone; 0x20000000 cannot be restored`
        // and left the guest executing zeroes.
        h.kernel
            .mem
            .insert(seg.vaddr, seg.vaddr + seg.mem_size, prot, "[image]");
    }
    let layout = gex_ld::build_initial_stack(
        &program.exe,
        // `STACK_TOP`, not the top of the guest's addressable range.
        //
        // A stack at the top makes the top load-bearing, and a linear memory
        // has to be allocated up to the highest address anything touches --
        // so a process cost four gigabytes before running an instruction,
        // and six of them cost more than a browser would give.
        gex_abi::STACK_TOP,
        argv,
        envp,
        *b"gexdeterministic",
        gex_ld::Identity {
            uid: unsafe { IDENTITY.0 },
            gid: unsafe { IDENTITY.1 },
            interp_base: program.interp_base(),
        },
    );
    // Zero first, then lay the new stack out on top of it.
    //
    // `execve` gives a program a *fresh* stack, and on Linux fresh anonymous
    // pages read as zero. gex was inserting the vma and writing only the
    // layout -- argv, envp, auxv -- leaving everything below it exactly as
    // the previous image had left it. A program that reads an uninitialised
    // local therefore read the *last program's* data instead of the zero
    // Linux would have given it.
    //
    // That is not a theoretical difference. apt's `file` method does
    //
    //     ldr x0, [sp, #0xa8]
    //     bl  free
    //
    // on a slot it never wrote. On Linux that slot is zero and `free(NULL)`
    // returns at its first instruction; under gex it held 2, left over from
    // the shell that forked it, and `free(2)` dereferenced a chunk header
    // sixteen bytes below address two. In wasm that is not a signal, it is a
    // trap that takes the Worker with it -- which is how `apt-get update`
    // came to die with `memory access out of bounds` and no other evidence.
    let stack_lo = (gex_abi::STACK_TOP - gex_abi::STACK_SIZE) as usize;
    data[stack_lo..gex_abi::STACK_TOP as usize].fill(0);
    let start = layout.low as usize;
    data[start..start + layout.bytes.len()].copy_from_slice(&layout.bytes);
    h.kernel.mem.insert(
        gex_abi::STACK_TOP - gex_abi::STACK_SIZE,
        gex_abi::STACK_TOP,
        gex_kernel::mem::prot::READ | gex_kernel::mem::prot::WRITE,
        "[stack]",
    );

    // The new program's pages have nothing translated for them, and anything
    // the old one left in the directory is code at the same addresses doing
    // something else entirely. Clearing it sends every branch back through
    // the miss stub, which is where a fresh translation comes from.
    data[gex_abi::DISPATCH_DIR as usize..LOW_END as usize].fill(0);
    // Slots already handed out are never reused -- a page that comes back
    // through a `fork` checkpoint still points at its own -- so allocation
    // carries on from the high-water mark rather than restarting.
    h.jit.page_table = gex_xlate::PageTable::from_entries(&[], h.jit.page_table.slots());
    h.jit.translated.clear();
    // Except the closure's. Those modules are still in the dispatch tables --
    // `execve` rebuilds the image in this memory, not the wasm instance
    // around it -- so the new program can have every library the build
    // covered for nothing. See `CLOSURE_DIRS`.
    let closure = CLOSURE_DIRS.0.borrow();
    if !closure.is_empty() {
        for (page, base) in closure.iter() {
            h.jit.page_table.assign(*page, *base);
            let at = (gex_abi::DISPATCH_DIR + page * 4) as usize;
            if at + 4 <= data.len() {
                data[at..at + 4].copy_from_slice(&base.to_le_bytes());
            }
        }
        trace_line(&format!(
            "[gex] exec: {} closure pages kept, {} slots\n",
            closure.len(),
            h.jit.page_table.slots()
        ));
    }
    drop(closure);

    let ctx_addr = h.threads[cur()].kstate.ctx_addr;
    let c = ctx_addr as usize;
    for i in 0..31u8 {
        write_u64(data, c + ctx::x(i), 0);
    }
    write_u64(data, c + ctx::SP, layout.sp);
    write_u64(data, c + ctx::PC, program.entry());
    write_u64(data, c + ctx::MON_SIZE, 0);
    data[c + ctx::SAFEPOINT_COUNTER..c + ctx::SAFEPOINT_COUNTER + 4]
        .copy_from_slice(&1024i32.to_le_bytes());
    h.threads = vec![Thread {
        kstate: ThreadState::new(ctx_addr, h.kernel.pid),
        pc: program.entry(),
        waiting: Waiting::Runnable,
        blocked_on: None,
    }];
    set_current(h, 0);
    program.entry()
}

/// What identifies the code at `page`, when anything does.
///
/// `None` for a page that could change under us -- anything writable, and
/// anything with no file behind it. Sharing slots for those would be the
/// bug this is careful to avoid: a slot whose contents no longer match the
/// bytes it was translated from.
///
/// The length of the mapping is in the key as well as the offset, because a
/// translation can follow a branch past the end of the page it started in.
/// Same file, same offset and same length means the whole reachable extent is
/// the same, which is what makes the sharing sound rather than probable.
/// Where code of this identity was translated to, system-wide when there is
/// a global kernel and per Worker when there is not.
///
/// A run of one process has no kernel and needs none of this; a run of many
/// has one, and then the map has to be the kernel's or siblings cannot see
/// each other's work. Both are kept in step: the local map is still written,
/// so the fork record still carries something useful to a child.
fn shared_slot(h: &Host, page: u64, key: u64) -> Option<u32> {
    // SAFETY: a plain lookup in the kernel's own memory, taking no borrow of
    // anything this module owns.
    if unsafe { k_present() } != 0 {
        let base = unsafe { k_slot_get(page, key) };
        if base != 0 {
            return Some(base);
        }
    }
    h.jit.shared.get(&(page, key)).copied()
}

/// Record it in both, first writer winning in each.
fn record_shared_slot(h: &mut Host, page: u64, key: u64, base: u32) {
    h.jit.shared.entry((page, key)).or_insert(base);
    // SAFETY: as above.
    if unsafe { k_present() } != 0 {
        unsafe { k_slot_put(page, key, base) };
    }
}

/// Where every process puts a shared object, so a translation of it means
/// the same thing in all of them.
///
/// A guest address is baked into a translated block, so two processes only
/// share a module when they have the object at the same base -- and until
/// this existed they did not, because the arena hands out addresses in the
/// order things are mapped and no two processes map the same things in the
/// same order. The answer lives in the global kernel, keyed by a hash of the
/// path: the caller is a different wasm memory, and a scalar crosses that
/// boundary without a copy.
struct SharedPlaces;

impl gex_kernel::syscalls::ObjectPlace for SharedPlaces {
    fn place(&self, path: &str, len: u64) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut h);
        // SAFETY: a call into the global kernel taking two scalars.
        let base = unsafe { k_place(h.finish(), len) };
        (base != 0).then_some(base)
    }
}

/// Keep a translated module where every Worker can reach it. Zero if there is
/// no kernel, or it has no room left.
fn share_module(wasm: &[u8], slots: u64, dirs: &[(u64, u32)]) -> u32 {
    // SAFETY: `k_present` takes no borrow; both pointers name ranges of this
    // module's memory, which is the process memory the kernel copies across
    // from, and both slices outlive the call.
    unsafe {
        if k_present() == 0 {
            return 0;
        }
        let mut raw = Vec::with_capacity(dirs.len() * 12);
        for (page, base) in dirs {
            raw.extend_from_slice(&page.to_le_bytes());
            raw.extend_from_slice(&base.to_le_bytes());
        }
        k_mod_store(
            wasm.as_ptr() as u32,
            wasm.len() as u32,
            slots,
            raw.as_ptr() as u32,
            raw.len() as u32,
        )
    }
}

/// The shared module holding this page's code, if there is one and this
/// Worker has not installed it: `(id, slots, dirs, wasm)`.
fn shared_module_at(
    h: &Host,
    page: u64,
    key: u64,
) -> Option<(u32, u64, Vec<(u64, u32)>, Vec<u8>)> {
    // SAFETY: every call below either returns a scalar, or writes into a
    // buffer allocated here whose length was asked of the kernel first -- it
    // refuses rather than overruns when the two disagree.
    unsafe {
        if k_present() == 0 {
            return None;
        }
        let id = k_mod_for(page, key);
        if id == 0 || h.jit.installed_mods.contains(&id) {
            return None;
        }
        let len = k_mod_len(id) as usize;
        if len == 0 {
            return None;
        }
        let mut wasm = vec![0u8; len];
        if k_mod_read(id, wasm.as_mut_ptr() as u32, len as u32) as usize != len {
            return None;
        }
        let dlen = k_mod_dirs_len(id) as usize;
        let mut raw = vec![0u8; dlen];
        if dlen > 0 && k_mod_dirs_read(id, raw.as_mut_ptr() as u32, dlen as u32) as usize != dlen {
            return None;
        }
        let dirs = raw
            .chunks_exact(12)
            .map(|c| {
                (
                    u64::from_le_bytes(c[0..8].try_into().unwrap()),
                    u32::from_le_bytes(c[8..12].try_into().unwrap()),
                )
            })
            .collect();
        Some((id, k_mod_slots(id), dirs, wasm))
    }
}

fn page_identity(h: &Host, page: u64) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let v = h
        .kernel
        .mem
        .vmas()
        .iter()
        .find(|v| page >= v.start && page < v.end)?;
    if v.prot & gex_kernel::mem::prot::WRITE != 0 {
        return None;
    }
    let (id, off) = v.origin.as_ref()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    (off + (page - v.start)).hash(&mut hasher);
    (v.end - v.start).hash(&mut hasher);
    Some(hasher.finish())
}

/// Nanoseconds on the host clock.
fn now_ns() -> u64 {
    unsafe { now() as u64 }
}

/// Choose the next guest thread to run, and finish whatever syscall it was in
/// the middle of.

///
/// A Worker has one wasm stack and no way to block on it, so the *threads* of
/// this process are cooperative: one keeps the stack until it blocks, exits,
/// or spawns another. Every register a thread owns is in its context in guest
/// memory, so a switch between them is nothing more than re-entering the guest
/// module with a different context pointer.
///
/// It no longer chooses between *processes*. There is one process per Worker;
/// the others are running elsewhere and need nothing from this scheduler.
///
/// What the scheduler decided.
enum Pick {
    /// Run this thread, having timed out or not.
    Thread(usize, bool),
    /// Nothing in this process can run yet.
    Idle,
    /// This process has finished.
    Over,
}

/// Whether a thread could make progress if it were given the stack.
///
/// Asked of threads whose process is not loaded as well as of the running
/// one, which is what lets the scheduler cross a process boundary. Everything
/// it consults -- handles, sockets, futexes, the clock -- is reference-counted
/// and lives outside guest memory, so the answer does not depend on whose
/// image is currently in it.
fn thread_ready(t: &Thread, now: u64) -> bool {
    match &t.waiting {
        Waiting::Runnable => true,
        Waiting::Futex { parked, deadline } => {
            parked.woken() || deadline.is_some_and(|d| now >= d)
        }
        Waiting::Sleep(d) => now >= *d,
        Waiting::Read(handle) => handle.poll_readable(),
        Waiting::Write(handle) => handle.poll_writable(),
        Waiting::Net(deadline) => {
            deadline.is_some_and(|d| now >= d)
                || match &t.blocked_on {
                    Some(gex_kernel::BlockOp::Connect { socket }) => {
                        socket.status() != gex_kernel::net::Status::Connecting
                    }
                    Some(gex_kernel::BlockOp::Poll { fds, .. }) => {
                        gex_kernel::syscalls::poll_ready(fds)
                    }
                    _ => true,
                }
        }
        // Ready when one of the children this call can collect has a status
        // waiting, and also when there are none at all -- that is `ECHILD`,
        // which is an answer and not a reason to sleep forever.
        Waiting::Child => match &t.blocked_on {
            Some(gex_kernel::BlockOp::WaitChild {
                children,
                procs,
                ppid,
                want,
                ..
            }) => {
                // The child is another Worker, so its status is not in this
                // memory and there is no handle here to look at: the table
                // that holds it is asked instead. Asking must not reap --
                // this runs on every turn of the run queue.
                if procs.is_shared() {
                    return procs.wait_ready(*ppid, *want);
                }
                let mut any = false;
                for c in children.iter().filter(|c| *want <= 0 || c.pid == *want) {
                    any = true;
                    if c.status.lock().map(|st| st.is_some()).unwrap_or(true) {
                        return true;
                    }
                }
                !any
            }
            _ => true,
        },
        Waiting::Accept(socket) => socket.poll_readable(),
        Waiting::Exited => false,
        // Somebody else's to run, however ready it is.
        Waiting::Elsewhere => false,
    }
}

/// Returns 2 when the run is over and 1 to re-enter the guest.
/// What a parked thread is waiting for, in one word, for a census.
///
/// Written out rather than derived: `Waiting` carries file handles and futex
/// parks, and none of those are worth printing -- what matters when a run
/// stops is the shape of the wait, not its contents.
fn waiting_name(w: &Waiting) -> &'static str {
    match w {
        Waiting::Runnable => "runnable",
        Waiting::Elsewhere => "another worker",
        Waiting::Futex { deadline: Some(_), .. } => "futex+timeout",
        Waiting::Futex { .. } => "futex",
        Waiting::Sleep(_) => "sleep",
        Waiting::Read(_) => "read",
        Waiting::Write(_) => "write",
        Waiting::Accept(_) => "accept",
        Waiting::Child => "wait4",
        Waiting::Net(Some(_)) => "net+timeout",
        Waiting::Net(None) => "net",
        Waiting::Exited => "exited",
    }
}

/// How many scheduler turns in a row have found nothing to run.
///
/// Outside the function because each turn is its own call: the scheduler
/// hands the stack back to the page and is re-entered, so a local would
/// start from zero every time and never notice it was getting nowhere.
static IDLE_SPINS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// One turn of the scheduler found nothing to run. Say so, occasionally.
///
/// On a curve rather than every time: a guest genuinely waiting for a
/// keystroke passes through here a thousand times a second quite
/// legitimately, and a line for each would bury the run it is meant to
/// explain. The powers of two mean a wait that resolves says nothing at all,
/// and a wait that does not gets louder in proportion to how wrong it is.
fn report_idle(why: u8) {
    let n = IDLE_SPINS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if !n.is_power_of_two() || n < 1024 {
        return;
    }
    let census = with_host(|h| {
        let mut out = String::new();
        // Each process with its own descriptor table, so a polled fd can be
        // named. A number on its own says which slot a process is waiting
        // on and nothing about what is at the other end, which is exactly
        // the question when two processes are waiting for each other.
        // This Worker's process only. When two processes are waiting for each
        // other the other half of the picture is in the other Worker's own
        // census, which is a loss this has to get back another way -- by
        // asking the global kernel, which can see every process's blocked
        // descriptors because the pipes and sockets are all in its memory.
        for (pid, p, k) in std::iter::once((h.kernel.pid as usize, &h.threads, &h.kernel)) {
            for (i, t) in p.iter().enumerate() {
                out.push_str(&format!(" {pid}.{i}={}", waiting_name(&t.waiting)));
                // And, for a poll, exactly which descriptors and how each
                // one answers right now. "net" says a thread is waiting on
                // something outside itself; it does not say whether the
                // thing it is waiting for is already true, which is the
                // difference between a wait and a deadlock.
                if let Some(gex_kernel::BlockOp::Poll { fds, .. }) = &t.blocked_on {
                    out.push('(');
                    for f in fds {
                        let what = k
                            .fds
                            .get(f.fd)
                            .map(|o| {
                                let d = o.handle.debug_state();
                                if d.is_empty() { o.path.clone() } else { d }
                            })
                            .unwrap_or_else(|_| "?".into());
                        out.push_str(&format!(
                            "[{} {what} want{:#x} have{:#x}] ",
                            f.fd,
                            f.events,
                            gex_kernel::syscalls::poll_revents(f)
                        ));
                    }
                    out.push(')');
                }
                // And, for a plain blocking read or write, what it is on.
                //
                // "read" on its own is the least useful thing this can say
                // when the question is which of two processes is waiting for
                // the other. The handle knows -- it is the same
                // `debug_state` a polled descriptor prints -- and a pipe that
                // says "0 buffered" is a different bug from a terminal that
                // is waiting for a key nobody will press.
                match &t.waiting {
                    Waiting::Read(h) | Waiting::Write(h) | Waiting::Accept(h) => {
                        let d = h.debug_state();
                        if !d.is_empty() {
                            out.push_str(&format!("({d})"));
                        }
                    }
                    _ => {}
                }
            }
        }
        out
    });
    // Built from the bits rather than matched on the whole.
    //
    // The match had arms for 0, 1 and 2 and a catch-all that said "a
    // keystroke and the event loop". Once there was a third bit every mask
    // carrying it fell into that arm, so a wait that had nothing to do with
    // the keyboard was reported as one -- and it was read as a
    // misclassification that was not there.
    let mut asked = String::new();
    if why == 0 {
        asked.push_str("nothing the page can supply");
    } else {
        if why & 1 != 0 {
            asked.push_str("a keystroke");
        }
        if why & 2 != 0 {
            if !asked.is_empty() {
                asked.push_str(" and ");
            }
            asked.push_str("the event loop to turn");
        }
        if why & 4 != 0 {
            asked.push_str(" (only the page can end it)");
        }
    }
    trace_line(&format!(
        "[gex] {n} turns with nothing runnable, wanting {asked}, threads:{census}\n"
    ));
}

fn schedule() -> i32 {
    // What the wake counter read *before* anything was asked about what is
    // runnable, so that a wake arriving while this makes up its mind cannot
    // be slept through. `gex_park` sleeps only while the counter is still
    // this, and a bump between here and there makes the sleep return at once.
    if unsafe { k_present() } != 0 {
        WAKE_SEEN.store(unsafe { k_wake_seq() }, std::sync::atomic::Ordering::Relaxed);
    }
    // Unconditional, and at the top, because every other counter in this file
    // sits on a path that can be missed. A run has now spent seven minutes
    // pinning a core while producing no safepoint, no dispatch miss, no
    // syscall, no wake and no idle turn -- which says only that whatever it
    // is doing is not any of the things that are instrumented.
    //
    // This is the last place left that everything passes through. If it
    // climbs, the loop is out here; if it does not, the guest is inside a
    // single stretch of translated code and the problem is a back-edge that
    // never polls.
    if unsafe { HEARTBEAT } != 0 {
        static mut ENTRIES: u64 = 0;
        let n = unsafe {
            ENTRIES = ENTRIES.wrapping_add(1);
            ENTRIES
        };
        if n.is_power_of_two() && n >= 65536 {
            trace_line(&format!("[gex] {n} scheduler entries\n"));
        }
    }
    loop {
        // Before anything is chosen. A vtest client writes a request and then
        // blocks reading the reply, and both are syscalls, so the renderer
        // has to run in between -- here, which is the one place every thread
        // passes through on its way to waiting. Nothing is ready to run until
        // it has.
        with_host(|h| h.virgl.clone()).map(|v| v.pump());
        let pick = with_host(|h| {
            // Every thread gone ends this process, and this process is all
            // this Worker has. Whoever is waiting for it finds the status in
            // the shared process table.
            if h.threads
                .iter()
                .all(|t| matches!(t.waiting, Waiting::Exited))
            {
                let code = h.exit.take().unwrap_or(0);
                exit_process(h, code);
                h.exit.get_or_insert(code);
                return Pick::Over;
            }
            let now = now_ns();
            // When every thread is a Worker, there is nothing to choose.
            //
            // This Worker owns exactly one entry of the table and the engine
            // schedules the rest; picking any *other* runnable thread would
            // put two Workers on one set of registers, which is what it did
            // -- a thread came back from a futex, took the main thread's
            // context because it happened to be runnable, and the pc that
            // came out was two half-words of two different threads.
            if thread_workers() {
                let i = cur();
                return if thread_ready(&h.threads[i], now) {
                    let timed_out = match &h.threads[i].waiting {
                        Waiting::Futex { parked, .. } => !parked.woken(),
                        _ => false,
                    };
                    Pick::Thread(i, timed_out)
                } else if matches!(h.threads[i].waiting, Waiting::Exited) {
                    // This Worker's thread is done. The process is not: the
                    // others are Workers of their own, and whoever joins this
                    // one is already parked on its futex.
                    Pick::Over
                } else {
                    Pick::Idle
                };
            }
            // Round-robin from the one after the current, so a thread that
            // yields does not immediately get the stack back.
            let n = h.threads.len();
            for step in 1..=n {
                let i = (cur() + step) % n;
                if thread_ready(&h.threads[i], now) {
                    let timed_out = match &h.threads[i].waiting {
                        Waiting::Futex { parked, .. } => !parked.woken(),
                        _ => false,
                    };
                    return Pick::Thread(i, timed_out);
                }
            }
            // Nothing in this process can run. Another process might, and
            // that is no longer this scheduler's problem: every other process
            // is a Worker the engine is already running, and it will make its
            // own progress without being switched to.
            // Everything is asleep or parked with a deadline. There is no
            // other thread to advance the clock, so spinning is the only way
            // to reach the earliest one.
            Pick::Idle
        });
        // A switch installs a process but chooses nothing. Going round again
        // is what picks a thread out of it -- and, crucially, what finishes
        // the syscall that thread parked in. Resuming its saved pc directly
        // skipped that, so it carried on with no return value in x0 and
        // branched through whatever was there: `nothing to translate at
        // pc=0xffff0`, on every pipeline.
        if matches!(pick, Pick::Over) {
            return 2;
        }
        let Pick::Thread(i, timed_out) = pick else {
            // A thread waiting for input is not stuck: the page will deliver
            // some. Give the stack back to JS so it can, and come back here
            // when it has.
            // Nothing can run, but not everything that cannot run is stuck.
            // A thread waiting for a keystroke needs the page to supply one;
            // a thread waiting on the network needs the page's event loop to
            // turn, so the bytes it is waiting for can arrive. Both mean
            // giving the wasm stack back, and the page is told which.
            // A read parks the same way whatever it is reading, so which
            // kind of wait this is comes from the handle. A socket read is a
            // network wait -- telling the page to ask for a keystroke instead
            // means it never turns its event loop, and the bytes it is
            // waiting for never arrive.
            // The two are not exclusive, so this is a pair of bits and not a
            // choice: bit 0 says a thread wants a keystroke, bit 1 says a
            // thread wants the page's event loop to turn. A guest with a
            // shell in one process and a download in another wants both, and
            // reporting only one of them starves the other.
            let why = with_host(|h| {
                // Only a terminal is a keystroke.
                //
                // This asked "is it a socket?" and called everything else a
                // wait for the person at the keyboard. A *pipe* is everything
                // else, and a pipe is how one guest process talks to another:
                // apt to its http method, a shell to the program in a
                // pipeline. Every one of those reads told the page to wait
                // for a keystroke, so a message between two processes was
                // delivered on whatever timer the page raced the keyboard
                // with -- five milliseconds each, on a protocol that is
                // hundreds of small messages. `apt-get update` fetched
                // 14.9 MB in four minutes at 60 kB/s that way.
                //
                // A fifo, like a socket, is written by somebody else in this
                // run; both want the event loop and neither wants a key.
                let by_kind = |handle: &Arc<dyn gex_kernel::vfs::FileHandle>| {
                    use gex_kernel::vfs::FileType;
                    match handle.metadata().map(|m| m.file_type) {
                        Ok(FileType::Socket) | Ok(FileType::Fifo) => 2,
                        _ => 1,
                    }
                };
                let mut mask = 0u8;
                // Bit 2: the page, and only the page, can end this wait.
                //
                // Bit 1 has always meant "give the stack back and turn the
                // event loop", and it covers two quite different things: a
                // wait another Worker will satisfy, and a wait the page will.
                // The first can be slept on -- the Worker that satisfies it
                // bumps a word in the shared memory and the sleeper wakes at
                // once. The second cannot: the bytes arrive on this Worker's
                // own event loop, and a Worker asleep is a Worker not running
                // the callback that delivers them.
                //
                // Wisp is the whole of that second case here. A process with
                // no network has nothing the page delivers except keystrokes,
                // which are bit 0 and have their own path.
                // Bit 1 with it, always. Bit 2 is an annotation on "give the
                // stack back", not a reason of its own, and on its own it is
                // a silent death: the loop in JavaScript asks whether the
                // guest is paused for input or for the event loop, and a
                // mask of bit 2 alone answers no to both -- so the loop body
                // never runs, nothing ever wakes, and the run ends reporting
                // whatever exit code it happened to hold. Which is zero. A
                // guest that stopped for a reason nobody recognised looked
                // exactly like a program that had finished successfully.
                // Per wait, not per process. Asking only whether the process
                // has a network at all condemned every wait it makes: apt has
                // one, so its pipe to the http method -- which another Worker
                // fills, and which a notify could end in microseconds -- was
                // sent round the page's event loop like the download was.
                if h.threads.iter().any(|t| wait_needs_page(t, &h.kernel)) {
                    mask |= 2 | 4;
                }
                // Every thread in the run, not just the ones in the process
                // that happens to be loaded. A shell blocked in `wait4` for a
                // child in another process contributes nothing here, and the
                // child -- the one actually waiting on the page -- was not
                // being looked at, so the answer came out zero and the loop
                // below spun at full speed for as long as the run lasted.
                for t in h.threads.iter() {
                    match &t.waiting {
                        // The same question as a read: a pipe or a socket is
                        // drained by another process, a terminal by the page.
                        Waiting::Write(handle) => mask |= by_kind(handle),
                        // A listener waiting for a client. Whoever connects
                        // is outside this guest -- the page, or another
                        // Worker -- so the page's event loop has to turn for
                        // it to arrive. Without saying so the scheduler spun:
                        // nothing was ready, nothing was reported, and the
                        // loop went round for ever.
                        Waiting::Accept(_) => mask |= 2,
                        Waiting::Read(handle) => {
                            mask |= by_kind(handle);
                        }
                        // Every timed wait and every poll parks as `Net`, but
                        // most of them are not the network. An interactive
                        // shell polls its terminal rather than blocking in
                        // `read` -- busybox's does -- and calling that a
                        // network wait told the page to turn its event loop
                        // and never ask for a keystroke. The terminal then
                        // took input and dropped it on the floor, forever.
                        //
                        // So a poll is classified by what it is polling.
                        Waiting::Net(deadline) => {
                            match &t.blocked_on {
                                Some(gex_kernel::BlockOp::Poll { fds, .. }) => {
                                    let mut saw = false;
                                    for p in fds {
                                        if let Some(handle) = &p.handle {
                                            mask |= by_kind(handle);
                                            saw = true;
                                        }
                                    }
                                    if !saw {
                                        mask |= 2;
                                    }
                                }
                                _ => mask |= 2,
                            }
                            // A wait that can time out needs the clock to
                            // move, and only the page can move it, so it
                            // wants the event loop whatever else it wants.
                            if deadline.is_some() {
                                mask |= 2;
                            }
                        }
                        _ => {}
                    }
                }
                mask
            });
            if why != 0 {
                // Also counted, and this is the case that hides. Handing the
                // stack back for a wait the page can satisfy is exactly
                // right; doing it a thousand times a second for a wait
                // nothing will ever satisfy is a hang that costs no CPU and
                // says nothing, which is how the first frame of glxgears was
                // followed by silence rather than by a second frame.
                report_idle(why);
                set_paused_to(why);
                return 2;
            }
            if with_host(|h| {
                h.threads
                    .iter()
                    .all(|t| matches!(t.waiting, Waiting::Futex { deadline: None, .. }))
            }) {
                // Every remaining thread is parked on a futex nothing will
                // ever wake. On Linux this hangs; saying so is more useful.
                trace_line("gex: every thread is blocked on a futex\n");
                with_host(|h| set_exit(h, 128 + 31));
                return 2;
            }
            // Nothing is runnable and nothing above recognised what anything
            // is waiting for. Going round again would ask the same question
            // of the same unchanged state, at whatever speed the CPU allows.
            //
            // That is what happened: 105% of a core, for as long as anyone
            // let it run, with no output at all -- the heartbeat only beats
            // from a safepoint or a dispatch miss, and a guest that is not
            // executing reaches neither. A hang and a hard-working run were
            // the same picture.
            //
            // So say what everything is waiting for, once, and then hand the
            // stack back. Yielding cannot be wrong when nothing can run: at
            // worst the page turns its event loop and gives it straight back.
            report_idle(0);
            set_paused_to(2);
            return 2;
        };
        IDLE_SPINS.store(0, std::sync::atomic::Ordering::Relaxed);
        if tracing() {
            let (pc, blocked) = with_host(|h| (h.threads[i].pc, h.threads[i].blocked_on.is_some()));
            trace_line(&format!(
                "[gex] switch to thread {i} at pc={pc:#x} blocked={blocked}
"
            ));
        }
        let resume = with_host(|h| {
            set_current(h, i);
            let t = &mut h.threads[i];
            t.waiting = Waiting::Runnable;
            let op = t.blocked_on.take();
            let (mut pc, kstate) = (t.pc, t.kstate.clone());
            if let Some(op) = op {
                // The syscall that parked this thread still owes it a return
                // value, and the kernel writes that into the context of
                // whichever thread it thinks is running.
                let mut gm = GuestMem::new(guest_bytes());
                h.kernel.enter_thread(&kstate);
                let r = finish_parked(&op, timed_out, &mut gm);
                let outcome = h.kernel.finish_blocking(&mut gm, r);
                h.kernel.leave_thread(&mut h.threads[cur()].kstate);
                match outcome {
                    // One process, not the run -- the same distinction the
                    // syscall path makes. This is the site that ended `apt
                    // update`: apt's gpgv method exits from a syscall it had
                    // been parked in, and the whole guest went with it, after
                    // the download had already succeeded and with apt's own
                    // process still live and holding a child. `exit_process`
                    // publishes the status the parent's `wait4` collects and
                    // hands the stack to whoever is next.
                    SyscallOutcome::Exit(code) => {
                        exit_process(h, code);
                        return None;
                    }
                    // Finishing a blocked syscall can *deliver a signal*.
                    // Delivery builds the handler's frame and says where to
                    // go, and this is the only place that can honour it: the
                    // thread's saved pc is the instruction after its `svc`,
                    // which is not where a handler starts.
                    //
                    // Dropping it is why every `fork` from a shell broke. A
                    // child exiting is SIGCHLD and a shell handles SIGCHLD,
                    // so `(echo sub); echo after` printed `sub`, ran the
                    // instruction after the `svc` on top of the handler's
                    // frame, and branched to address 0x80 with 17 -- SIGCHLD
                    // -- still sitting in x0 where the handler's first
                    // argument goes.
                    SyscallOutcome::Resume(target) => {
                        if tracing() {
                            trace_line(&format!("[sig] resume at {target:#x}\n"));
                        }
                        pc = target;
                    }
                    // Same as the syscall path: this ends one process. The
                    // difference is that there is no stack to return to, so
                    // the caller is told there is no thread to resume and the
                    // scheduler picks the next one.
                    SyscallOutcome::Killed { signal, .. } => {
                        exit_process(h, -(signal as i32));
                        return None;
                    }
                    _ => {}
                }
            }
            Some((kstate.ctx_addr, pc))
        });
        match resume {
            Some((ctx_addr, pc)) => {
                with_host(|h| {
                    set_resume(pc);
                    set_resume_ctx(ctx_addr);
                });
                return 1;
            }
            None => return 2,
        }
    }
}

/// What a parked syscall should return now that its thread is running again.
///
/// A woken futex returns zero and the guest re-checks its own condition, the
/// same as on Linux, where a wake is never a promise about the word.
fn finish_parked(
    op: &gex_kernel::BlockOp,
    timed_out: bool,
    gm: &mut GuestMem<'_>,
) -> Result<u64, Errno> {
    match op {
        gex_kernel::BlockOp::FutexWait { .. } if timed_out => Err(Errno::ETIMEDOUT),
        gex_kernel::BlockOp::FutexWait { .. } | gex_kernel::BlockOp::Sleep(_) => Ok(0),
        // The read the guest asked for happens now, into the buffer it named.
        // Doing it here rather than re-executing the instruction is what keeps
        // the guest's pc moving forward: a syscall that parks is finished by
        // the scheduler, never retried.
        gex_kernel::BlockOp::ReadStream { handle, buf, len } => {
            let mut tmp = vec![0u8; *len];
            let n = handle.read_at(0, &mut tmp)?;
            gm.write(*buf, &tmp[..n as usize])?;
            Ok(n)
        }
        gex_kernel::BlockOp::WriteStream { handle, buf, len } => {
            // One turn's worth; the scheduler brings the thread back while
            // anything is left, because the descriptor stays unwritable until
            // the reader has drained.
            let data = gm.read(*buf, *len)?.to_vec();
            handle.write_at(0, &data)
        }
        // `accept` is finished by the kernel, which is the only thing with a
        // descriptor table; the wait here is only for a connection to arrive.
        gex_kernel::BlockOp::AcceptWait { socket } => {
            if socket.poll_readable() {
                Ok(0)
            } else {
                Err(Errno::EAGAIN)
            }
        }
        gex_kernel::BlockOp::Connect { socket } => match socket.status() {
            gex_kernel::net::Status::Open => Ok(0),
            gex_kernel::net::Status::Refused => Err(Errno::ECONNREFUSED),
            gex_kernel::net::Status::Closed => Err(Errno::ECONNRESET),
            // Woken by a deadline that a connect does not have; nothing else
            // could have woken it.
            gex_kernel::net::Status::Connecting => Err(Errno::EINPROGRESS),
        },
        gex_kernel::BlockOp::Poll { fds, report, .. } => {
            gex_kernel::syscalls::poll_finish(fds, report, gm)
        }
        // `sigsuspend` only ever ends one way, whether or not a handler ran.
        gex_kernel::BlockOp::SigSuspend { .. } => Err(Errno::EINTR),
        gex_kernel::BlockOp::WaitChild {
            children,
            procs,
            ppid,
            want,
            wstatus,
            nohang,
            ..
        } => {
            // A shared table already holds the status in the form `wait4`
            // reports it, because the child's own Worker published it there
            // as it exited. Nothing here has to reconstruct it.
            if procs.is_shared() {
                if let Some((pid, word)) = procs.collect(*ppid, *want) {
                    if *wstatus != 0 {
                        gm.write_u32(*wstatus, word as u32)?;
                    }
                    return Ok(pid as u64);
                }
                // Nothing ready is two different answers, and Linux is
                // careful about which: `ECHILD` when there is no such child
                // at all, and zero when there is one that has not finished.
                //
                // Only `WNOHANG` can tell them apart -- a blocking wait gets
                // here only once the scheduler has said a child is
                // collectable. Giving `ECHILD` to both is what ended the
                // gears demo with "error waiting for command: No child
                // processes" while the program being timed drew happily in
                // the next Worker along.
                let kids = procs.has_children(*ppid);
                if !*nohang {
                    trace_line(&format!(
                        "[gex] wait4 blocking by {ppid} for {want}: nothing to collect, \
                         has_children={kids}, ready={}\n",
                        procs.wait_ready(*ppid, *want)
                    ));
                }
                return if *nohang && kids {
                    Ok(0)
                } else {
                    Err(Errno::ECHILD)
                };
            }
            for c in children {
                if *want > 0 && c.pid != *want {
                    continue;
                }
                let mut st = c.status.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(code) = st.take() {
                    c.reaped.store(true, std::sync::atomic::Ordering::Release);
                    procs.reap(c.pid);
                    if *wstatus != 0 {
                        // `WIFEXITED`: the code in the second byte, low bits
                        // clear. `WIFSIGNALED`: the signal in the low seven,
                        // which is what a negative code means here.
                        let word = if code < 0 {
                            (-code) as u32 & 0x7f
                        } else {
                            ((code as u32) & 0xff) << 8
                        };
                        gm.write_u32(*wstatus, word)?;
                    }
                    return Ok(c.pid as u64);
                }
            }
            Err(Errno::ECHILD)
        }
    }
}

/// Abandon the guest's wasm stack and return control to JS.
///
/// Deliberate, and says so. Every way out of this module is the same
/// `unreachable` to the engine -- this handoff, a Rust panic, a failed
/// allocation, a bad access inside the kernel -- and JavaScript could not
/// tell them apart. So a shim that fell over mid-translation was indexed as
/// a guest that exited cleanly: `apt update` printed nothing, wrote no
/// lists, and returned success. The flag is what makes the difference
/// visible; `gex_handoff` reads and clears it.
fn trap() -> ! {
    HANDOFF.with(|c| c.set(true));
    warch::unreachable()
}

/// The syscall number the guest is inside, or the last one it was inside.
///
/// A plain static, deliberately: it is read after a crash, when the kernel
/// may still be borrowed by whatever crashed, and anything that took the
/// kernel to answer would panic instead of answering.
static LAST_SYSCALL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(!0);

/// Which syscall the guest was in. `0xffffffff` if it has made none.
#[no_mangle]
pub extern "C" fn gex_last_syscall() -> u32 {
    LAST_SYSCALL.load(std::sync::atomic::Ordering::Relaxed)
}

/// Set by [`trap`], cleared by [`gex_handoff`].
/// Per Worker, not per process. It says "*this* Worker meant to give the
/// stack back", and a static in a shared memory meant one Worker's trap flag
/// was consumed by another's run loop: the first thread of a threaded program
/// handed the stack back and was told it had crashed.
thread_local! {
    static HANDOFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Did the shim mean to give the stack back? Clears the flag.
///
/// Not `with_host`: this is asked on the way out of a trap, which is exactly
/// when the kernel may still be borrowed by whatever trapped.
#[no_mangle]
pub extern "C" fn gex_handoff() -> i32 {
    HANDOFF.with(|c| c.replace(false)) as i32
}

#[no_mangle]
pub extern "C" fn gex_resume_pc() -> i64 {
    RESUME.with(|c| c.take()).unwrap_or(0) as i64
}

/// The context to re-enter with, which changes when the scheduler switches
/// threads. Zero means "the same one as before".
#[no_mangle]
pub extern "C" fn gex_resume_ctx() -> i32 {
    RESUME_CTX.with(|c| c.take()).unwrap_or(0) as i32
}

#[no_mangle]
pub extern "C" fn gex_exit_code() -> i32 {
    with_host(|h| h.exit.unwrap_or(0))
}

fn write_u64(data: &mut [u8], off: usize, v: u64) {
    data[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// Fused multiply-add, which wasm cannot express. Rust's `mul_add` is the
/// hardware instruction where there is one and libm's exact software fallback
/// where there is not, which is what the guest needs either way.
#[no_mangle]
pub extern "C" fn gex_fma(_ctx: i32, a: i64, b: i64, c: i64, double: i32) -> i64 {
    // Counted, because "the guest does a lot of fused multiply-adds" is a
    // guess until it is a number. wasm has no FMA instruction, so every one
    // of these leaves the guest module for the shim's -- and a cross-module
    // call in the middle of a floating-point inner loop is the sort of thing
    // that costs more than the arithmetic.
    FMA_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if double != 0 {
        f64::from_bits(a as u64)
            .mul_add(f64::from_bits(b as u64), f64::from_bits(c as u64))
            .to_bits() as i64
    } else {
        f32::from_bits(a as u32)
            .mul_add(f32::from_bits(b as u32), f32::from_bits(c as u32))
            .to_bits() as i64
    }
}

/// How many times the guest has asked for a fused multiply-add.
pub static FMA_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Read and reset the count, for a harness that wants it per phase.
#[no_mangle]
pub extern "C" fn gex_fma_calls() -> i64 {
    FMA_CALLS.swap(0, std::sync::atomic::Ordering::Relaxed) as i64
}

#[no_mangle]
pub extern "C" fn gex_illegal(_ctx: i32, pc: i64, raw: i32) {
    trace_line(&format!(
        "gex: illegal instruction {:#010x} at pc={:#x}\n",
        raw as u32, pc
    ));
    with_host(|h| set_exit(h, 128 + 4));
    trap();
}
#[no_mangle]
pub extern "C" fn gex_breakpoint(_ctx: i32, pc: i64, raw: i32) {
    // The stack-pointer check borrows this import rather than adding one, so
    // a shim built before the check existed still links. It is worth saying
    // which object the block is in: the whole point of the check is that the
    // block that *writes* a bad `sp` is somewhere else entirely from the one
    // that dies on it, and naming the library is most of finding it.
    if raw == gex_abi::imports::BRK_BAD_SP {
        let line = with_host(|h| {
            let gm = GuestMem::new(guest_bytes());
            let c = h.threads[cur()].kstate.ctx_addr;
            let sp = gm.read_u64(c + ctx::SP as u64).unwrap_or(0);
            let x30 = gm.read_u64(c + ctx::x(30) as u64).unwrap_or(0);
            // `x30` is the link register, so it names whoever called into
            // this block -- which, for a block reached by `ret`, is the last
            // place the guest was somewhere sane.
            let object = |a: u64| {
                h.kernel
                    .mem
                    .vmas()
                    .iter()
                    .find(|v| a >= v.start && a < v.end)
                    .map_or(String::new(), |v| format!(" in {} +{:#x}", v.name, a - v.start))
            };
            format!(
                "gex: wild sp {sp:#x} on entry to {pc:#x}{}, x30={x30:#x}{}\n",
                object(pc as u64),
                object(x30),
            )
        });
        trace_line(&line);
        with_host(|h| set_exit(h, 128 + 11));
        trap();
    }
    trace_line(&format!("gex: breakpoint at pc={pc:#x}\n"));
    with_host(|h| set_exit(h, 128 + 5));
    trap();
}
#[no_mangle]
pub extern "C" fn gex_sysreg_read(_ctx: i32, _pc: i64, reg: i32) -> i64 {
    match reg {
        // CNTVCT_EL0, at the frequency the lifter tells the guest CNTFRQ is,
        // so the guest's own arithmetic on it comes out exact.
        -3 => unsafe { now() },
        // MIDR_EL1: a plausible Cortex-A72, the same one the native host
        // claims to be.
        -7 => 0x410f_d083,
        _ => 0,
    }
}
#[no_mangle]
pub extern "C" fn gex_sysreg_write(_ctx: i32, _pc: i64, _reg: i32, _val: i64) {}

/// Cache maintenance.
///
/// Only `dc zva` does anything, and it has to: the lifter reports a 64-byte
/// block through `DCZID_EL0`, and musl's `memset` zeroes whole blocks with it
/// rather than storing. Leaving it a no-op means large `memset`s silently
/// don't zero -- which showed up as Lua printing stale bytes where it had
/// formatted a number, three layers down inside `vsnprintf`.
#[no_mangle]
/// `dc zva` and friends.
///
/// Zeroing a block is a call out of wasm, which looks like it should be the
/// same kind of prize inlining the fused multiply-add was -- musl's `memset`
/// zeroes whole blocks with it, so on paper that is a host call every 64
/// bytes. Measured, it is not: 1.6 GiB of `memset` runs at about 6 GiB/s and
/// beats qemu-user 0.26s to 0.47s, which is far too fast for twenty-six
/// million host calls. Whatever musl's `memset` selects for these sizes, it
/// is not this path. Worth re-checking on a workload that provably reaches
/// it before lowering this to `memory.fill`.
pub extern "C" fn gex_cache_op(_ctx: i32, _pc: i64, op: i32, addr: i64) {
    if op != gex_abi::cache_op::DC_ZVA {
        // Clean, invalidate and barrier are all no-ops: the memory is
        // coherent and nothing here translates at runtime yet.
        return;
    }
    let block = gex_abi::DCZID_BLOCK_BYTES as usize;
    let start = (addr as u64 & !(gex_abi::DCZID_BLOCK_BYTES - 1)) as usize;
    let mem = guest_bytes();
    if let Some(dst) = mem.get_mut(start..start + block) {
        dst.fill(0);
    }
}
/// A poll on a loop back-edge, and the browser host's only preemption point.
///
/// With one wasm stack shared by every guest thread, a thread that neither
/// blocks nor exits would otherwise keep the machine to itself for as long as
/// it liked -- and a spin loop waiting on another thread would hang the
/// process outright. The guest's registers are all flushed to its context
/// before this call and the pc it is given is the branch target, so this is
/// exactly the point at which the stack can be handed to someone else.
#[no_mangle]
pub extern "C" fn gex_safepoint(ctx: i32, pc: i64) {
    // Rearm, as the native host does. Translated code counts down a word in
    // the context and calls here when it reaches zero; leaving it there means
    // it stays at zero and every later back-edge calls out. That is a
    // thousand-fold difference in how often this runs, which is worth having
    // right whether or not anything switches.
    {
        let mut gm = GuestMem::new(guest_bytes());
        let _ = gm.write_u32(
            ctx as u32 as u64 + ctx::SAFEPOINT_COUNTER as u64,
            SAFEPOINT_INTERVAL,
        );
    }
    // A heartbeat, when tracing, and this is the only place one can go.
    //
    // Nothing else runs while the guest executes translated code: no syscall,
    // no dispatch miss, no JavaScript. A page watching a run therefore sees
    // nothing at all during a long stretch of guest execution, and cannot
    // tell it from a hang -- which is exactly the state glxgears was in, at
    // 100% of a core, for six minutes, with the last word from it being
    // "loading Mesa and drawing".
    //
    // Translated code calls here on back-edges whether or not it makes a
    // syscall, so this says where the guest is when nothing else can.
    let every = unsafe { HEARTBEAT };
    // Counting is its own switch, not a side effect of the heartbeat.
    //
    // It used to live inside `if every != 0`, so `GEX_HOT` on its own counted
    // nothing and printed nothing -- and the reader then fell over formatting
    // the empty answer. Two different questions were sharing one flag: "say
    // where the guest is every N safepoints", which is for a run that appears
    // stuck, and "which back edges are hot", which is for a run that works
    // and is slow. Wanting the second almost never means wanting the first.
    if every != 0 || unsafe { HOT_ON } {
        static mut BEAT: u64 = 0;
        let beat = unsafe {
            BEAT = BEAT.wrapping_add(1);
            BEAT
        };
        // Where the guest's back edges are, counted.
        //
        // The native host has had `--sample` for a while and a page has had
        // nothing: the heartbeat prints whichever pc happened to be current
        // when it fired, which for a regular loop is the same two or three
        // addresses over and over and says nothing about the shape of the
        // work. A count over every safepoint does.
        unsafe {
            let slot = (pc as u64 as usize >> 2) % HOT_SLOTS;
            if HOT_PC[slot] == pc as u64 {
                HOT_N[slot] += 1;
            } else if HOT_N[slot] == 0 {
                HOT_PC[slot] = pc as u64;
                HOT_N[slot] = 1;
            } else {
                // Someone else's slot. Age it rather than fight over it, so
                // a genuinely hot address wins the slot within a few
                // thousand samples instead of never.
                HOT_N[slot] -= 1;
            }
        }
        if every != 0 && beat.is_multiple_of(every) {
            // With the threads, because a spin says nothing on its own. A
            // guest looping over three addresses for two million safepoints
            // is either the only thread there is -- in which case it is
            // waiting for something outside itself -- or it is waiting for a
            // sibling that is not being given a turn, and those want
            // completely different fixes.
            // The three hottest back edges, symbolised, alongside the one
            // that happened to be current.
            //
            // A single sample per beat answers "is it stuck" and nothing
            // else. The counters underneath have always held the shape of
            // the work -- `gex_hot_blocks` reads them, and only the native
            // runner ever called it. A page has no way to run a command
            // after the guest exits, so the beat carries it instead, and a
            // run that is merely slow says which object it is slow in.
            let mut top: Vec<(u64, u64)> = unsafe {
                HOT_N
                    .iter()
                    .enumerate()
                    .filter(|(_, &n)| n > 0)
                    .map(|(i, &n)| (n, HOT_PC[i]))
                    .collect()
            };
            top.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            top.truncate(3);
            let (blocks, census, whose) = with_host(|h| {
                let mut out = String::new();
                for (i, t) in h.threads.iter().enumerate() {
                    out.push_str(&format!(" {i}={}", waiting_name(&t.waiting)));
                }
                let name = |addr: u64| {
                    h.kernel
                        .mem
                        .vmas()
                        .iter()
                        .find(|v| addr >= v.start && addr < v.end)
                        .map(|v| format!("{}+{:#x}", v.name, addr - v.start))
                        .unwrap_or_else(|| format!("{addr:#x}"))
                };
                for (n, addr) in &top {
                    out.push_str(&format!(" hot[{}]={n}", name(*addr)));
                }
                // And which object the pc is in. A bare address says a run
                // is spinning; the library and the offset say where to look,
                // and turn it into a disassembly rather than a mystery.
                let whose = h
                    .kernel
                    .mem
                    .vmas()
                    .iter()
                    .find(|v| (pc as u64) >= v.start && (pc as u64) < v.end)
                    .map(|v| format!("{}+{:#x}", v.name, pc as u64 - v.start))
                    .unwrap_or_default();
                (h.jit.blocks, out, whose)
            });
            trace_line(&format!(
                "[beat] {beat} safepoints, pc={:#x} ({whose}), {blocks} blocks, \
                 threads:{census}\n",
                pc as u64
            ));
        }
    }
    // Leaving wasm is not free -- it is a trap, a JS catch and a re-entry --
    // so a thread gets several intervals' worth of machine before it is asked
    // to give it up. The slice is still short enough that a spin loop waiting
    // on another thread makes progress promptly.
    const SLICES_PER_SWITCH: u32 = 4;
    static mut TICK: u32 = 0;
    let due = unsafe {
        TICK = TICK.wrapping_add(1);
        TICK.is_multiple_of(SLICES_PER_SWITCH)
    };
    if !due {
        return;
    }
    // A Worker that owns one thread has nobody to hand the stack to.
    //
    // The check below asks whether another thread of this process could run,
    // which is exactly the wrong question once each of them is a Worker: the
    // answer is yes, and taking it means two Workers on one context. The
    // engine does the preempting now.
    if thread_workers() {
        return;
    }
    let switch = with_host(|h| {
        if h.threads.len() < 2 {
            return false;
        }
        let now = now_ns();
        let runnable = h.threads.iter().enumerate().any(|(i, t)| {
            i != cur()
                && match &t.waiting {
                    Waiting::Runnable => true,
                    Waiting::Futex { parked, deadline } => {
                        parked.woken() || deadline.is_some_and(|d| now >= d)
                    }
                    Waiting::Sleep(d) => now >= *d,
                    Waiting::Read(handle) => handle.poll_readable(),
                    Waiting::Write(handle) => handle.poll_writable(),
                    // Only the scheduler proper re-checks these; a safepoint
                    // switch does not need to.
                    Waiting::Net(deadline) => deadline.is_some_and(|d| now >= d),
                    // Running in a Worker of its own, so there is nothing
                    // here to hand the stack to.
                    Waiting::Elsewhere => false,
                    Waiting::Child => false,
                    Waiting::Accept(socket) => socket.poll_readable(),
                    Waiting::Exited => false,
                }
        });
        if runnable {
            if tracing() {
                trace_line(&format!(
                    "[gex] preempt t{} at pc={:#x}\n",
                    cur(), pc as u64
                ));
            }
            // The context's own PC has to be exact too: a plain branch does
            // not store it, and the next thing to read it -- a signal frame,
            // a `clone` copying this context -- would otherwise see wherever
            // the guest last happened to leave it.
            let a = h.threads[cur()].kstate.ctx_addr;
            let mut gm = GuestMem::new(guest_bytes());
            let _ = gm.write_u64(a + ctx::PC as u64, pc as u64);
            h.threads[cur()].pc = pc as u64;
            h.threads[cur()].waiting = Waiting::Runnable;
        }
        runnable
    });
    if switch && schedule() != 0 {
        trap();
    }
}

/// A dispatch miss: translate the block at `pc`, and everything reachable
/// from it, into a module for JS to instantiate.
///
/// This is what lets a page ship a small module. Translating a whole program
/// ahead of time costs 62 MiB of wasm for SQLite; discovering only what the
/// entry point reaches costs 14 MiB, and the rest arrives here as it is
/// needed.
///
/// The shim can lift and encode but not compile -- nothing inside wasm can
/// hand bytes to the engine -- so it leaves the module behind and traps out,
/// and JS instantiates it against the same memory and dispatch table before
/// resuming the guest at exactly the address that missed.
#[no_mangle]
pub extern "C" fn gex_dispatch_miss(_ctx: i32, pc: i64) -> i32 {
    let pc = pc as u64;
    if tracing() || unsafe { TRACE_MISSES } {
        // Printed *before* the translation, deliberately. The question this
        // answers is which block the translator did not come back from, and
        // a line printed afterwards is a line that never appears.
        trace_line(&format!("[gex] miss at pc={pc:#x}\n"));
    }
    // The other half of the heartbeat, and the half that matters when the
    // guest is not running at all.
    //
    // A safepoint beat says where the *guest* is; it says nothing while the
    // host is lifting and the engine is compiling, which for a program the
    // size of Mesa is most of the time. A run can then go minutes at a full
    // core with no beat, no syscall and no output, and there is no way to
    // tell translating from wedged -- which is exactly where glxgears sat.
    if unsafe { HEARTBEAT } != 0 {
        static mut MISSES: u64 = 0;
        let n = unsafe {
            MISSES = MISSES.wrapping_add(1);
            MISSES
        };
        if n.is_multiple_of(200) {
            let (blocks, ms, intr) = with_host(|h| {
                (h.jit.blocks, h.jit.nanos / 1_000_000, h.jit.intrinsics.len())
            });
            trace_line(&format!(
                "[beat] miss {n} at pc={pc:#x}, {blocks} blocks, {ms}ms lifting, \
                 {intr} intrinsics\n"
            ));
        }
    }
    // A signal handler returning. When the guest gave no `sa_restorer` the
    // kernel puts `SIGRETURN_TRAMPOLINE` in x30, an address inside the
    // reserved region that deliberately has nothing translated at it -- so
    // the handler's `ret` arrives here, which is the whole mechanism.
    //
    // The native host has always done this. The browser did not, and the
    // failure was spectacular rather than quiet: any handler at all ended the
    // run with `nothing to translate at pc=0xffff0`. What made it look like a
    // fork bug is that a shell's first handler is SIGCHLD, so it took a child
    // exiting to reach one.
    if pc == gex_kernel::signal::SIGRETURN_TRAMPOLINE {
        let resumed = with_host(|h| {
            let mut gm = GuestMem::new(guest_bytes());
            let kstate = h.threads[cur()].kstate.clone();
            h.kernel.enter_thread(&kstate);
            let r = h.kernel.sigreturn(&mut gm);
            h.kernel.leave_thread(&mut h.threads[cur()].kstate);
            match r {
                Ok(target) => {
                    if tracing() {
                        trace_line(&format!("[sig] sigreturn to {target:#x}\n"));
                    }
                    h.threads[cur()].pc = target;
                    set_resume(target);
                    set_resume_ctx(kstate.ctx_addr);
                    true
                }
                Err(_) => {
                    trace_line("gex: corrupt signal frame on sigreturn\n");
                    set_exit(h, 128 + 11);
                    false
                }
            }
        });
        let _ = resumed;
        trap();
    }
    // Before translating one block: has this object been missed often
    // Which of this object's functions the backend can serve itself. Before
    // either path translates anything.
    note_intrinsics(pc);
    // enough to be worth translating whole? If so it is now queued, and the
    // block this miss asked for is in it.
    if maybe_recompile_at(pc) {
        // The same resume the ordinary path uses: come back at the address
        // that missed, once JavaScript has instantiated what was queued and
        // the address resolves.
        with_host(|h| {
            set_resume(pc);
            set_resume_ctx(h.threads[cur()].kstate.ctx_addr);
        });
        trap();
    }
    let translated = with_host(|h| {
        if !h.jit.described {
            return false;
        }
        let exec: Vec<(u64, u64)> = h
            .kernel
            .mem
            .vmas()
            .iter()
            .filter(|v| v.prot & gex_kernel::mem::prot::EXEC != 0)
            .map(|v| (v.start, v.end))
            .collect();
        if !exec.iter().any(|(a, b)| pc >= *a && pc + 4 <= *b) {
            return false;
        }
        // Before anything is translated: if this page's code has been
        // translated already, by another process or by this one before an
        // `execve`, point the page at those slots instead of claiming more.
        // The page table is keyed by dispatch page, which is an index; the
        // identity is asked about an address, because that is what names a
        // mapping.
        let page = gex_abi::dispatch_page(pc);
        if h.jit.page_table.lookup(page).is_some() {
            h.jit.reuse_have += 1;
        }
        if h.jit.page_table.lookup(page).is_none() {
            let ident = page_identity(h, page * gex_abi::DISPATCH_PAGE);
            if ident.is_none() {
                h.jit.reuse_noident += 1;
            }
            if let Some(key) = ident {
                if shared_slot(h, page, key).is_some() {
                    h.jit.reuse_hit += 1;
                } else {
                    h.jit.reuse_miss += 1;
                }
                // Somebody has translated this page and this Worker's
                // tables do not hold it. Take their module rather than
                // lifting the same code again: it goes onto the very queue a
                // fresh translation uses, so JavaScript installs it by the
                // path that already exists, and the branch is retried once it
                // resolves.
                if let Some((id, slots, dirs, wasm)) = shared_module_at(h, page, key) {
                    h.jit.installed_mods.insert(id);
                    // Every page it covers, before the install rather than
                    // after: installing asks the shim which slot each block
                    // belongs in, and the answer comes from here. A Worker
                    // that assigned only the page it faulted on would place
                    // one block and silently drop the rest.
                    for (p, b) in &dirs {
                        h.jit.page_table.assign(*p, *b);
                    }
                    h.jit.reuse_served += 1;
                    if tracing() {
                        trace_line(&format!(
                            "[gex] serve mod {id} for page {page:#x} pc={pc:#x}: {} dirs, {} slots, {} bytes\n",
                            dirs.len(),
                            slots,
                            wasm.len()
                        ));
                    }
                    h.jit.pending.push_back(PendingModule {
                        dirs,
                        slots,
                        wasm,
                    });
                    publish_pending(h);
                    set_resume(pc);
                    set_resume_ctx(h.threads[cur()].kstate.ctx_addr);
                    return true;
                }
                if let Some(base) = shared_slot(h, page, key) {
                    h.jit.page_table.assign(page, base);
                    // And the guest's directory, which `execve` cleared.
                    // Only *freshly allocated* pages get an entry from the
                    // translation unit, and this page is not fresh -- it has
                    // a base already. Without writing it here the branch
                    // misses again on the very next instruction, translating
                    // one block per turn forever.
                    let mut gm = GuestMem::new(guest_bytes());
                    let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, base);
                }
            }
        }
        let started = now_ns();
        let known = std::mem::take(&mut h.jit.translated);
        let (unit, stats) = gex_xlate::translate_runtime(
            guest_bytes(),
            pc,
            &|a| exec.iter().any(|(s, e)| a >= *s && a + 4 <= *e),
            &|a| known.contains(&a),
            h.jit.page_table.clone(),
            Default::default(),
            // A cap keeps one miss from translating a whole library at once;
            // the next miss picks up where this stopped.
            2048,
            // Must match the memory this shim was built against: an import
            // that disagrees about sharing does not link, and a module the
            // translator emits is instantiated against the same memory the
            // guest runs in. The plain build says no and the shared build
            // says yes, which is exactly what the feature means.
            cfg!(feature = "shared-memory"),
            &h.jit.intrinsics,
        );
        h.jit.translated = known;
        if stats.blocks == 0 {
            return false;
        }
        h.jit.page_table = unit.page_table.clone();
        // And what this translation claimed, so the next process to run the
        // same code finds it. Recorded rather than overwritten: a page that
        // already has an owner keeps it, because the code just emitted names
        // whichever base it was given.
        // Into the system-wide store, so a sibling installs it rather than
        // lifting the same code again. The slot count travels with it: it is
        // a fact about the process that produced the module, and the one
        // installing it has a different high-water mark.
        let mod_id = share_module(&unit.wasm, unit.table_slots, &unit.dir_entries);
        for (page, base) in &unit.dir_entries {
            if let Some(key) = page_identity(h, *page * gex_abi::DISPATCH_PAGE) {
                record_shared_slot(h, *page, key, *base);
                if mod_id != 0 {
                    // SAFETY: a call into the kernel taking no borrow here.
                    unsafe { k_mod_claim(*page, key, mod_id) };
                }
            }
        }
        if mod_id != 0 {
            h.jit.installed_mods.insert(mod_id);
        }
        for (addr, _) in &unit.exported_blocks {
            h.jit.translated.insert(*addr);
        }
        h.jit.pending.push_back(PendingModule {
            dirs: unit.dir_entries.clone(),
            slots: unit.table_slots,
            wasm: unit.wasm,
        });
        publish_pending(h);
        h.jit.blocks += stats.blocks as u64;
        h.jit.compiles += 1;
        h.jit.nanos += now_ns().saturating_sub(started);
        if tracing() {
            trace_line(&format!(
                "[gex]   translated {} blocks in {} ms, {} vmas, {} slots, {} pages\n",
                stats.blocks,
                (now_ns().saturating_sub(started)) / 1_000_000,
                exec.len(),
                unit.table_slots,
                unit.dir_entries.len()
            ));
        }
        // Come back at the address that missed, now that it will resolve.
        set_resume(pc);
        set_resume_ctx(h.threads[cur()].kstate.ctx_addr);
        true
    });
    if !translated {
        if !with_host(|h| h.jit.described) {
            trace_line("gex: the guest module carries no gex.meta section, so there is\n");
            trace_line("gex: nowhere for runtime translation to carry on from. Rebuild it.\n");
        }
        with_host(|h| {
            let gm = GuestMem::new(guest_bytes());
            let c = h.threads[cur()].kstate.ctx_addr;
            let r = |i: u8| gm.read_u64(c + ctx::x(i) as u64).unwrap_or(0);
            trace_line(&format!(
                "gex: pid {} thread {} ctx {c:#x} sp={:#x} x30={:#x} x0={:#x} x1={:#x}\n",
                h.kernel.pid,
                cur(),
                gm.read_u64(c + ctx::SP as u64).unwrap_or(0),
                r(30),
                r(0),
                r(1)
            ));
            // And what this process *is*, which is the question a zeroed
            // context raises: a child that entered at zero with nothing on
            // its stack has either lost its image or never had one, and the
            // program it was told to become names which.
            trace_line(&format!(
                "gex: running {:?}, {} vmas, last syscall {}, ctx pc={:#x}\n",
                h.kernel.exe_path,
                h.kernel.mem.vmas().len(),
                LAST_SYSCALL.load(std::sync::atomic::Ordering::Relaxed),
                gm.read_u64(c + ctx::PC as u64).unwrap_or(0),
            ));
        });
        trace_line(&format!("gex: nothing to translate at pc={pc:#x}\n"));
        with_host(|h| set_exit(h, 128 + 11));
    }
    trap();
}


/// Statically recompile a whole object the guest has just mapped.
///
/// This is what the native host does with `--recompile-objects`, and what a
/// page never had. Without it the browser meets a library one dispatch miss
/// at a time: eleven thousand of them to bring up Mesa and an X server, each
/// one lifting at most a couple of thousand blocks and then stopping to have
/// a module compiled. Sixteen seconds of lifting for a demo that draws one
/// frame.
///
/// Doing it in bulk is not merely fewer round trips. It is *static*: the
/// whole object is translated from its own bytes, so what comes out depends
/// on the library and where it was loaded and on nothing else. The
/// demand-driven path produces a different module depending on which address
/// the guest happened to reach first, which is why its output never settled
/// and its cache never quite hit.
///
/// The runtime translator stays behind this. Seeding from symbols and from
/// the object's own data finds what anything can call and what any of its
/// own words point at; it cannot find a target that is only ever computed,
/// and that residue is what a dispatch miss is still for.
/// Drain what the guest has mapped since the last syscall, and recompile it.
///
/// Returns whether anything was emitted, which is the caller's cue to hand
/// the stack back so JavaScript can instantiate it.
///
/// Off unless the page asks. A demo that wants to show the runtime translator
/// working should get the runtime translator; this is a different claim about
/// what the system is, and it should be made deliberately.
fn recompile_objects() {
    if unsafe { RECOMPILE_OBJECTS } == 0 {
        return;
    }
    let mapped = with_host(|h| std::mem::take(&mut h.kernel.new_code));
    with_host(|h| {
        for m in mapped {
            h.jit.candidates.push((m, 0));
        }
    });
}

/// How many dispatch misses an object takes before the rest of it is
/// translated in bulk.
///
/// # What this is worth, measured
///
/// Waiting is much better than not waiting. On the gears demo, against
/// recompiling every object the moment it is mapped:
///
///     objects recompiled     57  ->  31
///     dispatch slots      6.26M  ->  3.89M   (V8 caps a table at 10M)
///     first GL client       25s  ->  14s
///     second                56s  ->  41s
///
/// But neither is better than leaving it off. Pure runtime translation
/// reaches the same two places at 10s and 29s, and CPython -- a sustained
/// workload rather than a startup-dominated one, five interleaved rounds
/// under V8 -- comes out slower with it on:
///
///     fannkuch  off 1.150s   on 1.242s   1.080x
///     nbody     off 1.578s   on 1.652s   1.047x
///     dicts     off 0.534s   on 0.558s   1.045x
///
/// Four to eight per cent, the same direction on all three, which is worth
/// more than the size of it. The reason is not mysterious: CPython's hot
/// loop is in the executable, which `gexc build --exhaustive` already
/// translated, and the parts of libc it leans on -- malloc, memcpy -- are
/// reached early and translated by the demand path within a few misses
/// anyway. Translating the rest of libc buys nothing and costs V8 a much
/// larger module to compile.
///
/// So this stays off by default. The mechanism is right and the case for it
/// has not been demonstrated: it wants a run that is long *and* spends its
/// time in a library, which neither workload here is.
const RECOMPILE_AFTER_MISSES: u32 = 24;

/// A dispatch miss landed at `pc`. If it is in an object that has now been
/// missed often enough, translate the rest of that object.
///
/// Returns whether anything was queued, which the caller takes as "the work
/// is done, do not also translate one block".
fn maybe_recompile_at(pc: u64) -> bool {
    if unsafe { RECOMPILE_OBJECTS } == 0 {
        return false;
    }
    let due = with_host(|h| {
        let at = h
            .jit
            .candidates
            .iter()
            .position(|(m, _)| pc >= m.start && pc < m.end)?;
        h.jit.candidates[at].1 += 1;
        if h.jit.candidates[at].1 < RECOMPILE_AFTER_MISSES {
            return None;
        }
        Some(h.jit.candidates.remove(at).0)
    });
    let Some(map) = due else { return false };
    let began = now_ns();
    let blocks = recompile_object(&map);
    if blocks == 0 {
        return false;
    }
    if unsafe { HEARTBEAT } != 0 {
        let ms = (now_ns() - began) / 1_000_000;
        let (slots, mods, bytes) = with_host(|h| {
            (
                h.jit.page_table.slots(),
                h.jit.pending.len(),
                h.jit.pending.iter().map(|p| p.wasm.len()).sum::<usize>(),
            )
        });
        trace_line(&format!(
            "[gex] recompiled {} statically: {blocks} blocks in {ms}ms, \
             {mods} module(s) of {} KiB waiting, {slots} slots claimed in all\n",
            map.path,
            bytes / 1024
        ));
    }
    true
}

/// Whether to statically recompile each object as it is mapped, and how big
/// an object to attempt.
///
/// The size is a bisection handle rather than a policy: with static
/// recompilation on, the graphics rootfs faults inside `ld.so`, and the
/// quickest way to find out which library's translation is wrong is to raise
/// the cap until it breaks. Zero is off; anything else is a limit in KiB of
/// mapped text, with `i32::MAX` meaning no limit worth mentioning.
static mut RECOMPILE_OBJECTS: u64 = 0;

#[no_mangle]
pub extern "C" fn gex_set_recompile_objects(kib: i32) {
    unsafe { RECOMPILE_OBJECTS = if kib <= 0 { 0 } else { kib as u64 } };
}

fn recompile_object(map: &gex_kernel::CodeMap) -> usize {
    let span = map.end.saturating_sub(map.start);
    let cap = (unsafe { RECOMPILE_OBJECTS }).saturating_mul(1024).min(MAX_OBJECT_BYTES);
    if span == 0 || span > cap {
        return 0;
    }
    // The same guard the miss path has. Until the guest module has said how
    // many slots it already uses, claiming from zero would hand out slots
    // the image module's own blocks are sitting in.
    if !with_host(|h| h.jit.described) {
        return 0;
    }
    // Has another process already paid for this library?
    //
    // Two processes map the same Mesa and the same libc at the same
    // addresses, and the slots one of them translated into answer perfectly
    // well for the other -- that is what the identity map is for, and the
    // miss path has always used it. Recompiling an object without asking
    // meant the second process translated all of it again and claimed a
    // second full set of slots: 2,439,742 blocks over 8,395,776 slots for
    // the gears demo, which is how a run reached V8's ceiling of ten million
    // and died with the dispatch table refusing to grow.
    //
    // Whole pages, because that is the unit the identity map is keyed by. An
    // object whose every page is already answered for is skipped outright.
    let adopted = with_host(|h| {
        let first = gex_abi::dispatch_page(map.start);
        let last = gex_abi::dispatch_page(map.end.saturating_sub(1));
        let mut missing = 0usize;
        for page in first..=last {
            if h.jit.page_table.lookup(page).is_some() {
                continue;
            }
            let Some(key) = page_identity(h, page * gex_abi::DISPATCH_PAGE) else {
                missing += 1;
                continue;
            };
            match h.jit.shared.get(&(page, key)) {
                Some(&base) => {
                    h.jit.page_table.assign(page, base);
                    // And the guest's directory, which `execve` cleared.
                    let mut gm = GuestMem::new(guest_bytes());
                    let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, base);
                }
                None => missing += 1,
            }
        }
        missing
    });
    if adopted == 0 {
        return 0;
    }
    let seeds = object_seeds(map);
    let intrinsics = with_host(|h| h.jit.intrinsics.clone());
    let (exec, page_table, known) = with_host(|h| {
        let exec: Vec<(u64, u64)> = h
            .kernel
            .mem
            .vmas()
            .iter()
            .filter(|v| v.prot & gex_kernel::mem::prot::EXEC != 0)
            .map(|v| (v.start, v.end))
            .collect();
        (exec, h.jit.page_table.clone(), std::mem::take(&mut h.jit.translated))
    });
    let (modules, stats) = gex_xlate::translate_object(
        guest_bytes(),
        map.start,
        map.end,
        &seeds,
        &|a| exec.iter().any(|(s, e)| a >= *s && a + 4 <= *e),
        &|a| known.contains(&a),
        page_table,
        Default::default(),
        MAX_OBJECT_BLOCKS,
        cfg!(feature = "shared-memory"),
        &intrinsics,
    );
    with_host(|h| {
        h.jit.translated = known;
        if modules.iter().all(|m| m.exported_blocks.is_empty()) {
            return;
        }
        // In emitted order. Each module's page assignments are the next
        // one's starting point, so installing them out of order hands two
        // modules the same slots.
        for m in &modules {
            h.jit.page_table = m.page_table.clone();
            for b in &m.exported_blocks {
                h.jit.translated.insert(b.0);
            }
        }
        h.jit.blocks += stats.blocks as u64;
        // And what this claimed, so the next process to map the same library
        // finds it rather than translating it again. The miss path does this
        // for what it translates; without the same here, the second process
        // to load libc pays for all 62,085 blocks of it a second time.
        for m in &modules {
            for (page, base) in &m.dir_entries {
                if let Some(key) = page_identity(h, *page * gex_abi::DISPATCH_PAGE) {
                    h.jit.shared.entry((*page, key)).or_insert(*base);
                }
            }
        }
        for m in modules {
            h.jit.pending.push_back(PendingModule {
                dirs: m.dir_entries,
                slots: m.table_slots,
                wasm: m.wasm,
            });
        }
        publish_pending(h);
    });
    stats.blocks
}

/// A mapping larger than this is not a shared library, and recompiling it
/// would be a long pause for nothing.
const MAX_OBJECT_BYTES: u64 = 64 << 20;

/// A runaway guard, not a budget. Objects are split across as many modules as
/// they need, so this only stops a range that decodes as code but is not.
const MAX_OBJECT_BLOCKS: usize = 4_000_000;

/// Where to start translating an object: everything it says about itself.
///
/// Its exported symbols are what anything outside can call, and every word in
/// its own data that points into its own text is the target of an indirect
/// branch that discovery cannot reach -- a vtable, a jump table, an
/// initialiser array, a callback in a struct.
///
/// The alternative is starting a block at every decodable address, which is
/// the only complete seeding and is quadratic: starting a block at each of
/// the L instructions of a basic block emits L blocks totalling L(L+1)/2
/// instructions. Empty when the file cannot be read, which tells
/// `translate_object` to scan instead.
/// Note the library functions in whichever object `addr` falls in, so the
/// translator can serve them itself.
///
/// Called before either path translates anything, and outside `with_host`
/// because reading the object takes the host itself.
/// Whether library substitution is on. See `gex_set_intrinsics`.
static mut INTRINSICS_ON: bool = true;

/// Turn library substitution off, to price it.
///
/// The same shape as `GEX_UNSAFE_NO_GUARD`: the only way to know what
/// replacing a library function is worth is to run the same program both
/// ways, and the two builds have to be one binary or the comparison is
/// measuring the compiler as well. Unlike the guard switch this one is safe
/// -- it translates glibc's own code instead, which is what it did before.
#[no_mangle]
pub extern "C" fn gex_set_intrinsics(on: u32) {
    unsafe { INTRINSICS_ON = on != 0 }
}

fn note_intrinsics(addr: u64) {
    if !unsafe { INTRINSICS_ON } {
        return;
    }
    let Some((path, start)) = with_host(|h| {
        let v = h.kernel.mem.vmas().iter().find(|v| {
            addr >= v.start && addr < v.end && v.prot & gex_kernel::mem::prot::EXEC != 0
        })?;
        let (path, _) = v.origin.as_ref()?;
        // Retried rather than answered once, because the answer does not
        // exist yet the first time: `memcpy`'s slot is filled by the dynamic
        // linker, and a miss inside `ld.so` itself happens before that.
        // Bounded, so an object whose slots are never bound stops being
        // re-parsed.
        if h.jit.intrinsics_scanned.get(path).copied().unwrap_or(0) >= 8 {
            return None;
        }
        Some((path.clone(), v.start))
    }) else {
        return;
    };
    let (found, pending) = scan_intrinsics(&path, start);
    let added = with_host(|h| {
        // A scan with nothing left outstanding need never run again.
        let n = if pending { 1 } else { 8 };
        *h.jit.intrinsics_scanned.entry(path.clone()).or_insert(0) += n;
        let before = h.jit.intrinsics.len();
        for (a, k) in found {
            h.jit.intrinsics.insert(a, k);
        }
        h.jit.intrinsics.len() - before
    });
    // Only when it learned something, so this is a handful of lines a run
    // rather than one per miss -- and it is on the heartbeat rather than
    // `strace` because "did the substitution happen at all" is the first
    // question asked of it, and answering that should not need every syscall
    // narrated.
    if added > 0 {
        let n = with_host(|h| h.jit.intrinsics.len());
        trace_line(&format!(
            "[gex] intrinsics: +{added} from {path}, {n} total\n"
        ));
    }
}

/// Which object's executable mapping a guest address falls in, if any.
fn executable_object_at(a: u64) -> Option<String> {
    with_host(|h| {
        let v = h
            .kernel
            .mem
            .vmas()
            .iter()
            .find(|v| a >= v.start && a < v.end && v.prot & gex_kernel::mem::prot::EXEC != 0)?;
        v.origin.as_ref().map(|(p, _)| p.clone())
    })
}

/// The library functions one object offers or calls, and whether any answer
/// is still outstanding.
///
/// Two sources, and the second is the one that matters on Debian. A plain
/// `STT_FUNC` symbol names its own code, so its address is its address --
/// which is the whole story for a static binary. But glibc exports `memcpy`,
/// `memmove` and `memset` as `STT_GNU_IFUNC`, whose symbol value is a
/// *resolver*: which implementation the machine gets is chosen at startup and
/// written into the calling object's GOT. So the address is not in any symbol
/// table at all, and the only way to learn it is to read the slot the dynamic
/// linker wrote, out of live guest memory, after it has run.
fn scan_intrinsics(path: &str, start: u64) -> (Vec<(u64, gex_xlate::Intrinsic)>, bool) {
    let Some(bytes) = with_host(|h| h.kernel.read_file(path)) else {
        return (Vec::new(), false);
    };
    let Ok(image) = gex_ld::elf::load(&bytes, 0) else {
        return (Vec::new(), false);
    };
    let Some(text) = image.segments.iter().find(|s| s.executable) else {
        return (Vec::new(), false);
    };
    // The same bias `object_seeds` computes, and for the same reason: the
    // image is parsed at its own addresses and mapped at others.
    let bias = start.wrapping_sub(text.vaddr & !0xfff);
    let (lo, hi) = (text.vaddr, text.vaddr + text.bytes.len() as u64);
    let gm = GuestMem::new(guest_bytes());
    let mut out = Vec::new();
    let mut pending = false;
    for s in &image.symbols {
        if s.ifunc || s.value < lo || s.value >= hi {
            continue;
        }
        if let Some(kind) = gex_xlate::intrinsic::recognise(&s.name) {
            out.push((s.value.wrapping_add(bias), kind));
        }
    }
    for (slot, name) in &image.named_relocs {
        let Some(kind) = gex_xlate::intrinsic::recognise(name) else {
            continue;
        };
        let Ok(target) = gm.read_u64(slot.wrapping_add(bias)) else {
            pending = true;
            continue;
        };
        // A slot that is not bound yet points at this object's own PLT
        // resolver stub, and an untouched one is zero. The stub is real
        // executable code in a real mapping, so "is this an address" does not
        // separate the two -- but *whose* code it is does: a resolver stub is
        // always in the object whose GOT this is, and the `memcpy` being
        // called is always somebody else's.
        //
        // Getting this wrong is not a missed optimisation. Binding the
        // resolver stub to `memcpy` makes every lazily-resolved call in the
        // object copy x1 to x0 for x2 bytes and return, which is how `ls`
        // came to die of an out-of-bounds access inside `close`.
        match executable_object_at(target) {
            Some(obj) if obj != path => out.push((target, kind)),
            _ => pending = true,
        }
    }
    (out, pending)
}

fn object_seeds(map: &gex_kernel::CodeMap) -> Vec<u64> {
    let Some(bytes) = with_host(|h| h.kernel.read_file(&map.path)) else {
        return Vec::new();
    };
    let Ok(image) = gex_ld::elf::load(&bytes, 0) else {
        return Vec::new();
    };
    let Some(text) = image.segments.iter().find(|s| s.executable) else {
        return Vec::new();
    };
    // Parsed at its own addresses; the bias is where the executable segment
    // was asked to go against where it ended up.
    let bias = map.start.wrapping_sub(text.vaddr & !0xfff);
    let (lo, hi) = (text.vaddr, text.vaddr + text.bytes.len() as u64);
    let mut seeds: Vec<u64> = image
        .symbols
        .iter()
        .filter(|s| s.value >= lo && s.value < hi)
        .map(|s| s.value.wrapping_add(bias))
        .collect();
    seeds.push(image.entry.wrapping_add(bias));
    for seg in &image.segments {
        if seg.executable {
            continue;
        }
        for (i, chunk) in seg.bytes.chunks_exact(8).enumerate() {
            if !(seg.vaddr as usize + i * 8).is_multiple_of(8) {
                continue;
            }
            let v = u64::from_le_bytes(chunk.try_into().unwrap());
            if v >= lo && v < hi && v % 4 == 0 {
                seeds.push(v.wrapping_add(bias));
            }
        }
    }
    seeds.sort_unstable();
    seeds.dedup();
    seeds
}

/// Where the running thread is, right now.
///
/// For the report when a trap escapes: a `RuntimeError` out of `enter` says
/// what went wrong and nothing at all about where, and "memory access out of
/// bounds" from translated code means the guest computed a wild pointer,
/// which is a question about the guest's own program counter.
/// The context the running thread is using. Wild register state usually
/// means a wild context pointer, and the two are told apart by looking.
#[no_mangle]
pub extern "C" fn gex_current_ctx() -> i64 {
    with_host(|h| h.threads[cur()].kstate.ctx_addr as i64)
}

#[no_mangle]
pub extern "C" fn gex_current_pc() -> i64 {
    with_host(|h| {
        let gm = GuestMem::new(guest_bytes());
        let at = h.threads[cur()].kstate.ctx_addr;
        gm.read_u64(at + ctx::PC as u64).unwrap_or(0) as i64
    })
}

/// Which mapped object an address belongs to, written into `out` as bytes.
/// Returns the length, or zero when nothing is mapped there.
#[no_mangle]
pub extern "C" fn gex_object_at(addr: i64, out: u32, max: u32) -> u32 {
    let name = with_host(|h| {
        h.kernel
            .mem
            .vmas()
            .iter()
            .find(|v| (addr as u64) >= v.start && (addr as u64) < v.end)
            .map(|v| format!("{} +{:#x}", v.name, addr as u64 - v.start))
    });
    let Some(name) = name else { return 0 };
    let n = name.len().min(max as usize);
    let dst = shim_slice_mut(out, n as u32);
    dst.copy_from_slice(&name.as_bytes()[..n]);
    n as u32
}

/// The module JS has to instantiate, or zero length if there is none.
#[no_mangle]
pub extern "C" fn gex_jit_pending_ptr() -> u32 {
    PENDING[0].load(std::sync::atomic::Ordering::Relaxed)
}

#[no_mangle]
pub extern "C" fn gex_jit_pending_len() -> u32 {
    PENDING[1].load(std::sync::atomic::Ordering::Relaxed)
}

/// How many slots the dispatch table must hold for the pending module.
#[no_mangle]
pub extern "C" fn gex_jit_slots() -> u32 {
    PENDING[2].load(std::sync::atomic::Ordering::Relaxed)
}

/// Where a translated block belongs: the table in the top eight bits, the
/// slot within it in the rest. -1 when its page has no slots.
///
/// Packed into one return rather than split across two calls because this is
/// called once per block installed by name, and a block-per-function
/// translation installs a great many.
///
/// The packing is the directory entry's own -- see `gex_abi::dispatch_entry`
/// -- so this is the entry with the page offset already added in, and it fits
/// because a table holds fewer than 2^24 slots.
#[no_mangle]
pub extern "C" fn gex_dispatch_slot(addr: i64) -> i32 {
    with_host(|h| {
        let page = gex_abi::dispatch_page(addr as u64);
        match h.jit.page_table.lookup(page) {
            Some(entry) => gex_abi::dispatch_entry(
                gex_abi::dispatch_entry_table(entry),
                gex_abi::dispatch_index(addr as u64, entry) as u32,
            ) as i32,
            None => -1,
        }
    })
}

/// How many guest pages this process has dispatch slots for.
///
/// Paired with [`gex_page_at`]. The page table is the map from a guest page
/// to the slots its translated blocks live in, and it is Rust data in this
/// Worker's heap -- so a forked child, which gets a copy of the *memory* and
/// nothing else, arrives without it. What the child does get is the dispatch
/// directory, copied with the low region, already pointing at those slots.
/// A child that did not adopt the map would translate a page the directory
/// says is already translated, into fresh slots, and its next branch into
/// the old ones would find an empty table entry.
#[no_mangle]
pub extern "C" fn gex_page_count() -> u32 {
    with_host(|h| h.jit.page_table.entries().len() as u32)
}

/// Write every page assignment to `out` as a `u64` page and a `u32` entry,
/// twelve bytes each, and return how many were written.
///
/// -1 if `cap` bytes will not hold them; ask [`gex_page_count`] first. One
/// call rather than one per page because the table is a `BTreeMap` and
/// indexing it costs a walk -- a per-page export made adopting a real
/// process's map quadratic in the pages it had translated.
#[no_mangle]
pub extern "C" fn gex_page_dump(out: u32, cap: u32) -> i32 {
    with_host(|h| {
        let entries = h.jit.page_table.entries();
        if (entries.len() * 12) as u32 > cap {
            return -1;
        }
        let dst = shim_slice_mut(out, (entries.len() * 12) as u32);
        for (i, (page, entry)) in entries.iter().enumerate() {
            dst[i * 12..i * 12 + 8].copy_from_slice(&page.to_le_bytes());
            dst[i * 12 + 8..i * 12 + 12].copy_from_slice(&entry.to_le_bytes());
        }
        entries.len() as i32
    })
}

/// Take a page's slots from the process this one was forked from.
///
/// The claim the caller is making -- that the slots already hold code for
/// this page -- is true for a fork by construction: the child's text is a
/// byte-for-byte copy of the parent's at the same addresses, which is the
/// same claim the cross-process identity map makes for a shared library.
#[no_mangle]
pub extern "C" fn gex_adopt_page(page: i64, entry: u32) {
    with_host(|h| h.jit.page_table.assign(page as u64, entry));
}

/// How many pages the identity map answers for.
///
/// The page table says where *this* process's translated pages are; the
/// identity map says which pages of which mapped object those slots hold
/// code for, and it is the only one of the two that survives an `execve`.
/// See [`gex_shared_dump`] for why the child needs it.
#[no_mangle]
pub extern "C" fn gex_shared_count() -> u32 {
    with_host(|h| h.jit.shared.len() as u32)
}

/// Write the identity map to `out`: a `u64` page, a `u64` key and a `u32`
/// base, twenty bytes each, returning how many were written. -1 if `cap` will
/// not hold them.
///
/// A forked child adopts the parent's page table, which is what stops it
/// translating a page the inherited directory already points at. But `execve`
/// throws that table away -- it must, because the addresses in it describe
/// the image being replaced -- and the identity map is what would have said
/// that the slots it just discarded still hold libc, mapped from the same
/// file at the same offset, and answer perfectly well for the new image's
/// copy of it. A child that arrives without the map has no way to know that,
/// so every `fork`+`execve` -- which is every command a shell runs --
/// re-translated the whole of libc into fresh slots.
///
/// Handed over as identities rather than as page assignments because that is
/// what makes it safe to give to a process that is *not* a copy of this one:
/// the key names the object, the offset in it and its size, so a page that
/// holds different code has a different key, and the miss path that reads
/// this compares the key against what the receiving process has actually
/// mapped before it believes a word of it.
#[no_mangle]
pub extern "C" fn gex_shared_dump(out: u32, cap: u32) -> i32 {
    with_host(|h| {
        let n = h.jit.shared.len();
        if (n * 20) as u32 > cap {
            return -1;
        }
        let dst = shim_slice_mut(out, (n * 20) as u32);
        for (i, ((page, key), base)) in h.jit.shared.iter().enumerate() {
            dst[i * 20..i * 20 + 8].copy_from_slice(&page.to_le_bytes());
            dst[i * 20 + 8..i * 20 + 16].copy_from_slice(&key.to_le_bytes());
            dst[i * 20 + 16..i * 20 + 20].copy_from_slice(&base.to_le_bytes());
        }
        n as i32
    })
}

/// Take the identity map from the process this one was forked from.
///
/// `count` records in the layout [`gex_shared_dump`] writes. Recorded rather
/// than overwritten, like every other way this map is filled: a page that
/// already has an owner in this process keeps it, because code already
/// emitted names whichever base it was given.
#[no_mangle]
pub extern "C" fn gex_shared_seed(ptr: u32, count: u32) -> u32 {
    let src = shim_slice(ptr, count.saturating_mul(20));
    with_host(|h| {
        let mut n = 0u32;
        for r in src.chunks_exact(20) {
            let page = u64::from_le_bytes(r[0..8].try_into().unwrap());
            let key = u64::from_le_bytes(r[8..16].try_into().unwrap());
            let base = u32::from_le_bytes(r[16..20].try_into().unwrap());
            if let std::collections::hash_map::Entry::Vacant(v) =
                h.jit.shared.entry((page, key))
            {
                v.insert(base);
                n += 1;
            }
        }
        n
    })
}

/// Slots one dispatch table holds, so the glue knows where each one ends.
///
/// Reported rather than written down twice: it is V8's ten-million cap
/// rounded down to a whole number of dispatch pages, and a glue that
/// disagreed with the emitter about where table 1 starts would put blocks in
/// the wrong table and dispatch would find the wrong code.
#[no_mangle]
pub extern "C" fn gex_slots_per_table() -> u32 {
    gex_xlate::PageTable::SLOTS_PER_TABLE as u32
}

/// How many dispatch tables the emitter was built for.
#[no_mangle]
pub extern "C" fn gex_dispatch_tables() -> u32 {
    gex_abi::DISPATCH_TABLES as u32
}

/// Publish the pending module's pages, now that its blocks are in the table.
///
/// Deliberately last: until the directory entry points at real slots, a guest
/// branch into one of these pages must keep reaching the miss stub.
#[no_mangle]
pub extern "C" fn gex_jit_installed() {
    with_host(|h| {
        let Some(done) = h.jit.pending.pop_front() else {
            return;
        };
        let mut gm = GuestMem::new(guest_bytes());
        for (page, base) in done.dirs {
            let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, base);
        }
        // Kept, not dropped. A dispatch table belongs to a module *instance*
        // and cannot be shared between Workers, so a thread joining this
        // process has none of what has already been translated -- and the
        // page directory, which *is* shared, will send it to slots its own
        // table has never had filled. The bytes stay here so a joining Worker
        // can instantiate the same modules into its own tables and catch up.
        //
        // This is the same registry-and-replay the native host keeps for its
        // stores. It costs the size of every module translated so far, which
        // is the price of a thread being able to run at all.
        // Only where a thread might have to replay it. A dispatch table
        // belongs to a module instance, so a Worker joining this process
        // needs every module the process has translated -- but against a
        // plain memory there are no other Workers, and keeping the bytes is
        // pure loss. It is a large loss: the shim's heap is a fixed 2 GiB,
        // and `apt update` filled it, which surfaced as a 1 MiB allocation
        // failing 2037 MiB in.
        //
        // The shared build still keeps them all, which is a ceiling on how
        // much a threaded process may translate and the next thing to bound
        // -- by dropping a module once every live Worker has installed it.
        #[cfg(feature = "shared-memory")]
        h.jit.registry.push(done.wasm);
        publish_pending(h);
    });
}

/// Say where the guest was when everything stopped.
///
/// A trap out of the shim reaches JavaScript as `RuntimeError` with no wasm
/// frames and no address, which is not enough to act on -- and the guest's
/// own registers are the only other evidence there is. A wild pointer in a
/// `free` and a stack that has run off its mapping look identical from
/// outside and completely different here.
///
/// Deliberately tolerant: this runs on the way out of a crash, so anything
/// it cannot read it prints as zero rather than failing a second time.
#[no_mangle]
pub extern "C" fn gex_dump_state() {
    if gex_has_host() == 0 {
        return;
    }
    with_host(|h| {
        let gm = GuestMem::new(guest_bytes());
        let c = h.threads[cur()].kstate.ctx_addr;
        let r = |i: u8| gm.read_u64(c + ctx::x(i) as u64).unwrap_or(0);
        let sp = gm.read_u64(c + ctx::SP as u64).unwrap_or(0);
        trace_line(&format!(
            "gex: pid {} thread {} sp={sp:#x} x0={:#x} x1={:#x} x2={:#x} x30={:#x}\n",
            h.kernel.pid,
            cur(),
            r(0),
            r(1),
            r(2),
            r(30),
        ));
        // Which mapping each of those addresses is in, if any. An address
        // with no vma is the whole diagnosis: the guest followed a pointer
        // into nothing, which on Linux is `SIGSEGV` and here is a trap that
        // takes the Worker with it.
        let where_is = |a: u64| -> String {
            match h.kernel.mem.vmas().iter().find(|v| a >= v.start && a < v.end) {
                Some(v) => format!("{:#x}..{:#x} {}", v.start, v.end, v.name),
                None => "UNMAPPED".to_string(),
            }
        };
        for (name, a) in [("sp", sp), ("x0", r(0)), ("x1", r(1))] {
            trace_line(&format!("gex:   {name}={a:#x} in {}\n", where_is(a)));
        }
    });
}

/// Publish an exit for a process whose Worker is stopping without one.
///
/// A process that stops existing must become collectable, however it stopped.
/// A guest that calls `exit_group` goes through `do_exit`, which tells the
/// process table; a guest that *traps* -- a lifter gap, a bad access, a panic
/// in the shim -- never gets there, and its record stays in the table
/// unreaped and with no status. Its parent then parks in `wait4` on a child
/// that will never report, and the run wedges with nothing runnable: not
/// `ECHILD`, which at least ends the wait, but silence.
///
/// That is #69. The status reads as death by `SIGKILL`, which is the closest
/// true thing: the process did not choose to stop.
///
/// Idempotent by way of the table: `exit` on a pid that has already published
/// finds the record, sets the cell again to the same shape, and wakes nobody
/// new. Calling it after a clean exit is therefore harmless, which is what
/// lets the Worker call it unconditionally on its way out.
#[no_mangle]
pub extern "C" fn gex_abandon(code: i32) {
    if gex_has_host() == 0 {
        return;
    }
    with_host(|h| {
        let pid = h.kernel.pid;
        if h.exit.is_some() {
            return;
        }
        trace_line(&format!("[proc] abandon {pid}: the Worker stopped without an exit\n"));
        h.kernel.procs.exit(pid, code);
    });
}

/// Record a guest exit, and say so where a trap handler can see it.
fn set_exit(h: &mut Host, code: i32) {
    h.exit = Some(code);
    EXITED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the guest actually finished, as opposed to the shim falling over.
///
/// `trap()` is how a guest hands the wasm stack back, and it is also what a
/// panic or a failed allocation inside the shim comes out as. Both arrive in
/// JavaScript as `RuntimeError: unreachable` and nothing distinguishes them,
/// so a shim that crashed mid-translation was reported as a program that
/// exited cleanly with status 0 -- which is how `apt update` came to say
/// nothing, write no lists, and return success.
#[no_mangle]
pub extern "C" fn gex_exited() -> i32 {
    // Deliberately not `with_host`: this is asked on the way out of a trap,
    // which is exactly when the kernel may still be borrowed by whatever
    // trapped.
    EXITED.load(std::sync::atomic::Ordering::Relaxed) as i32
}

/// Set when a guest exit is recorded, so `gex_exited` need not borrow.
static EXITED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The head of the pending queue, as three plain words.
///
/// JavaScript reads these between handing the wasm stack back and giving it
/// again, and it must not need the kernel to do it. It used to: each of
/// `gex_jit_pending_ptr`, `_len` and `gex_jit_slots` took `with_host`, and
/// the ordering that makes that safe -- the guest is never running while JS
/// asks -- is not one this side can enforce. When it was violated the
/// symptom was `RefCell already borrowed` in the plain build and a silent
/// hang in the shared one, which is the worst pair of failure modes for the
/// same bug.
///
/// Written wherever the queue is, read without a borrow. Three words that
/// only the kernel writes and only JS reads need no more than that.
static PENDING: [std::sync::atomic::AtomicU32; 3] =
    [const { std::sync::atomic::AtomicU32::new(0) }; 3];

/// Republish the head of the queue. Call after every push and pop.
fn publish_pending(h: &Host) {
    let (p, l, s) = h.jit.pending.front().map_or((0, 0, 0), |m| {
        (m.wasm.as_ptr() as u32, m.wasm.len() as u32, m.slots as u32)
    });
    PENDING[0].store(p, std::sync::atomic::Ordering::Relaxed);
    PENDING[1].store(l, std::sync::atomic::Ordering::Relaxed);
    PENDING[2].store(s, std::sync::atomic::Ordering::Relaxed);
}

/// How many translated modules this process has, for a Worker catching up.
#[no_mangle]
pub extern "C" fn gex_module_count() -> u32 {
    with_host(|h| h.jit.registry.len() as u32)
}

/// Where module `i`'s bytes are, in the memory every thread shares.
///
/// A pointer rather than a copy: the registry lives in the process memory and
/// a joining Worker is already looking at it.
#[no_mangle]
pub extern "C" fn gex_module_ptr(i: u32) -> u32 {
    with_host(|h| {
        h.jit
            .registry
            .get(i as usize)
            .map_or(0, |m| m.as_ptr() as u32)
    })
}

/// Every slot this process has claimed, across every module.
///
/// What a Worker catching up must grow its tables to. `gex_jit_slots` reports
/// the *pending* module's high-water mark, which is the right answer only for
/// the Worker that is installing it; a thread replaying the registry has to
/// reach the whole process's.
#[no_mangle]
pub extern "C" fn gex_jit_slots_total() -> u32 {
    with_host(|h| h.jit.page_table.slots() as u32)
}

/// How long module `i` is.
#[no_mangle]
pub extern "C" fn gex_module_len(i: u32) -> u32 {
    with_host(|h| h.jit.registry.get(i as usize).map_or(0, |m| m.len() as u32))
}

/// A direct-mapped table of back-edge counts, keyed by guest pc.
///
/// Direct-mapped and lossy on purpose: this runs on every safepoint, so it
/// has to be a shift, a compare and an add. Collisions age each other out,
/// which costs accuracy on a flat profile and none at all on the peaked one
/// this exists to find.
const HOT_SLOTS: usize = 4096;
/// Whether to count back edges at all. See `gex_set_hot`.
static mut HOT_ON: bool = false;

/// Start counting back edges, without asking for a heartbeat as well.
#[no_mangle]
pub extern "C" fn gex_set_hot(on: u32) {
    unsafe { HOT_ON = on != 0 }
}

static mut HOT_PC: [u64; HOT_SLOTS] = [0; HOT_SLOTS];
static mut HOT_N: [u64; HOT_SLOTS] = [0; HOT_SLOTS];

/// The hottest back edges, as `pc count` lines, most first.
///
/// Deliberately without `with_host`. This is read from inside the guest's
/// own `write`, which is already inside it, and re-entering panics -- the
/// trap comes back as a bare "unreachable" and unwinds the guest rather
/// than the reporter. Symbolising the addresses needs the VMA list and so
/// needs the lock, so it is not done here; `gex_object_at` does it when the
/// caller is somewhere safe, and the raw addresses are useful without it.
#[no_mangle]
pub extern "C" fn gex_hot_blocks(out: u32, max: u32) -> u32 {
    let mut top: Vec<(u64, u64)> = unsafe {
        HOT_N.iter()
            .enumerate()
            .filter(|(_, &n)| n > 0)
            .map(|(i, &n)| (n, HOT_PC[i]))
            .collect()
    };
    top.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    top.truncate(12);
    let mut text = String::new();
    for (n, pc) in top {
        text.push_str(&format!("{pc:#x} {n}\n"));
    }
    let n = text.len().min(max as usize);
    shim_slice_mut(out, n as u32).copy_from_slice(&text.as_bytes()[..n]);
    n as u32
}

/// How often the safepoint says where the guest is; zero is off.
static mut HEARTBEAT: u64 = 0;

/// Say which block is about to be translated, for every miss.
///
/// Loud -- a cold Mesa run makes eleven thousand of these -- and worth it for
/// one question: when a run stops with the wasm stack held and no beat, no
/// syscall and no scheduler entry, the last line is the block the translator
/// went into and did not come out of.
static mut TRACE_MISSES: bool = false;

#[no_mangle]
pub extern "C" fn gex_set_trace_misses(on: i32) {
    unsafe { TRACE_MISSES = on != 0 };
}

/// Check the guest's stack pointer on entry to every block translated after
/// this, and stop at the first one that has a wild one.
///
/// The native side reads `GEX_CHECK_SP` from the environment; a page has no
/// environment, so it says so here instead. Only blocks translated afterwards
/// carry the check -- turning it on partway through a run leaves everything
/// already in the dispatch table unchecked, which is usually what is wanted:
/// it keeps the check off the startup path and on the object under suspicion.
///
/// Costs a load, a compare and a never-taken branch per block, so it is for
/// finding a bug rather than for shipping.
#[no_mangle]
pub extern "C" fn gex_set_check_sp(on: i32) {
    gex_xlate::set_check_sp(on != 0);
}

/// Ask for a heartbeat every `every` safepoints.
///
/// Its own switch rather than a level of `gex_set_trace`, because the two
/// answer different questions and one drowns the other: a syscall trace of a
/// run that translates Mesa is hundreds of megabytes, and the thing worth
/// knowing -- is the guest moving at all -- is a line a second.
#[no_mangle]
pub extern "C" fn gex_set_heartbeat(every: i64) {
    unsafe { HEARTBEAT = every.max(0) as u64 };
}

/// Blocks translated at run time, and how many times the translator ran.
#[no_mangle]
pub extern "C" fn gex_jit_blocks() -> i64 {
    with_host(|h| h.jit.blocks as i64)
}

#[no_mangle]
pub extern "C" fn gex_jit_compiles() -> i64 {
    with_host(|h| h.jit.compiles as i64)
}

/// How the shared-slot lookup went, packed so one call answers.
///
/// `(have, hit, miss, noident)` in four 16-bit fields is not enough for a
/// program that misses a hundred thousand times, so this returns one of the
/// four by index instead.
#[no_mangle]
pub extern "C" fn gex_reuse_stat(which: u32) -> i64 {
    with_host(|h| {
        (match which {
            0 => h.jit.reuse_have,
            1 => h.jit.reuse_hit,
            2 => h.jit.reuse_miss,
            4 => h.jit.reuse_served,
            _ => h.jit.reuse_noident,
        }) as i64
    })
}

/// Milliseconds the shim itself spent translating.
#[no_mangle]
pub extern "C" fn gex_jit_millis() -> i64 {
    with_host(|h| (h.jit.nanos / 1_000_000) as i64)
}
