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
    /// The shim's own narration, to the host's debug console.
    ///
    /// Not fd 2. Everything this file traces used to go to the guest's
    /// standard error, which is the terminal a person is reading -- so a
    /// closure diagnostic, a syscall trace and the program's own output
    /// arrived interleaved on the same stream, and turning the noise off
    /// meant finding and gating each site. The guest's stderr belongs to the
    /// guest; this is a channel of our own.
    fn debug(ptr: u32, len: u32);
    /// The same channel at error level, for the few things a person must see
    /// whatever else is switched off: a misplaced pin, a corrupt signal
    /// frame, a process that crashed.
    fn warn(ptr: u32, len: u32);
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
    fn k_kill_group(caller: i32, pgid: i32, signo: u32) -> i32;
    /// How many signals have been sent to anyone. One relaxed load, so a
    /// safepoint can ask "has anything happened" without taking the process
    /// table's lock a thousand times a second.
    fn k_signal_seq() -> i32;
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
    fn k_fs_dup(handle: i32) -> i32;
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
    fn k_slot_len() -> u32;
    /// Standard input, shared. Whoever has a keystroke writes it here and
    /// wakes a parked reader, rather than posting a message to a Worker that
    /// is asleep inside wasm and cannot receive one.
    fn k_stdin_write(at: u32, len: u32) -> i32;
    fn k_stdin_read(at: u32, len: u32) -> i32;
    fn k_stdin_ready() -> i32;
    fn k_stdin_eof();
    /// The terminal: one `struct termios`, one size, one foreground process
    /// group and one line discipline, for every process at once. The page
    /// pushes keystrokes straight into it; these are the guest's half.
    fn k_tty_get_termios(at: u32, len: u32) -> i32;
    fn k_tty_set_termios(at: u32, len: u32) -> i32;
    fn k_tty_get_locked(at: u32, len: u32) -> i32;
    fn k_tty_set_locked(at: u32, len: u32) -> i32;
    fn k_tty_winsize() -> u64;
    fn k_tty_set_winsize(packed: u64) -> i32;
    fn k_tty_fg() -> i32;
    fn k_tty_set_fg(pgrp: i32);
    fn k_tty_sid() -> i32;
    fn k_tty_set_sid(sid: i32);
    fn k_tty_ldisc() -> i32;
    fn k_tty_set_ldisc(n: i32) -> i32;
    fn k_tty_bits() -> i32;
    fn k_tty_set_bits(which: i32, on: i32);
    fn k_tty_flush(which: i32) -> i32;
    fn k_tty_queued() -> i32;
    fn k_tty_push(at: u32, len: u32) -> i32;
    /// Slots nobody else will be given, and the mark that keeps runtime
    /// allocation clear of what static recompilation already assigned.
    fn k_slot_alloc(count: u64) -> u64;
    fn k_slot_floor(floor: u64);
    /// Translated modules, once for the whole system. Knowing which slots a
    /// page's code is in does not let a Worker reach it -- a dispatch table
    /// belongs to a module instance -- so the module itself has to be
    /// shareable too, and between two wasm modules that means bytes.
    fn k_mod_store(ptr: u32, len: u32, slots: u64, dirs_ptr: u32, dirs_len: u32) -> u32;
    fn k_mod_claim(page: u64, key: u64, id: u32);
    fn k_mod_for(page: u64, key: u64, n: u32) -> u32;
    fn k_mod_claim_object(obj: u64, id: u32);
    fn k_obj_translating(obj: u64, pid: u32) -> u32;
    fn k_obj_translated(obj: u64, produced: u32);
    fn k_obj_is_translated(obj: u64) -> u32;
    fn k_mod_for_object(obj: u64, n: u32) -> u32;
    fn k_mod_slots(id: u32) -> u64;
    fn k_mod_len(id: u32) -> u32;
    fn k_mod_dirs_len(id: u32) -> u32;
    fn k_mod_read(id: u32, out: u32, cap: u32) -> u32;
    fn k_mod_dirs_read(id: u32, out: u32, cap: u32) -> u32;
    /// Has some process already filled the shared root? Only the first
    /// should: the rest would unpack the same rootfs over the top of it.
    fn k_place(key: u64, len: u64) -> u64;
    fn k_place_set(key: u64, base: u64, len: u64);
    /// The linked address space of a program, kept so the next process of it
    /// does not have to run the dynamic linker again.
    fn k_snap_store(key: u64, ptr: u32, len: u32);
    fn k_snap_len(key: u64) -> u32;
    fn k_snap_get(key: u64, at: u32, cap: u32) -> u32;
    fn k_fs_populated() -> i32;
    fn k_fs_populate_done();

    // Pipes. A shell pipeline is two processes and a pipe, and apt is a
    // process and four method workers on pairs of them; with a Worker per
    // process the other end is always in another memory.
    fn k_pipe_new() -> i32;
    fn k_pipe_open_end(id: i32, write: i32, pid: i32);
    fn k_pipe_ends(id: i32) -> i32;
    fn k_pipe_write_stats(id: i32) -> i32;
    fn k_pipe_writes(id: i32) -> i32;
    fn k_pipe_last_message(id: i32, at: u32, cap: u32) -> i32;
    fn k_pipe_close_end(id: i32, write: i32, pid: i32);
    fn k_pipe_forget(pid: i32) -> u32;
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

/// A page table whose runtime allocations come from the system-wide counter.
///
/// Every process gets one. The slots a statically recompiled image was built
/// with stay exactly as built -- it is the same code in every process, so the
/// same numbers are right -- and the mark is raised above them so nothing
/// translated at runtime can land there. Everything after that is drawn in
/// chunks from the kernel, which is what lets one process install a module
/// another one translated: the range it occupies was issued once, to its
/// translator, and no other process holds it.
fn shared_page_table(
    entries: &[(u64, u32)],
    next_slot: u64,
    n_blocks: usize,
) -> gex_xlate::PageTable {
    let mut table = gex_xlate::PageTable::from_entries(entries, next_slot);
    // Without a global kernel there is one process, and its own counter is
    // already unique.
    // SAFETY: both take scalars and borrow nothing from this side.
    if unsafe { k_present() } == 0 {
        return table;
    }
    unsafe { k_slot_floor(next_slot) };
    table.set_refill(std::sync::Arc::new(gex_xlate::Refill(Box::new(|n| {
        // SAFETY: as above.
        unsafe { k_slot_alloc(n) }
    }))));
    // And now that allocation comes from the kernel, forget how long
    // `from_entries` thought the tables had to be.
    //
    // It recorded two things, and with a refill in place neither is this
    // process's to pay for. The allocation mark is where *the build* stopped
    // numbering, and this table does not allocate there any more -- every
    // chunk comes from `k_slot_alloc`. The build's entry for each image page
    // is where that page sits only if somebody publishes it there, and a
    // claiming process publishes none of them: it numbers its own.
    //
    // Left in, the mark alone made `gex_table_need(0)` report the whole
    // closure's 3,800,064 slots from the moment a Worker started, so the
    // first install grew table 0 to hold them -- two `Table.grow`s, 790ms of
    // an 815ms install, for a process that goes on to use fifty thousand.
    //
    // The entries are kept -- a page published at the build's base needs its
    // table to reach it -- so this is safe whatever the image looks like. The
    // first attempt gated on `n_blocks > BIG_BLOCKS`, reasoning that a small
    // image writes absolute slots at instantiation; but the metadata here is
    // the *seed* module's, which is small, so the gate was never true and the
    // number never moved. The entries carry that requirement anyway.
    if claim_pages() {
        table.forget_mark_high();
    }
    let _ = n_blocks;
    table
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
extern "C" fn r_kill_group(caller: i32, pgid: i32, signo: u32) -> i32 {
    unsafe { k_kill_group(caller, pgid, signo) }
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
            kill_group: r_kill_group,
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
extern "C" fn rf_dup(handle: i32) -> i32 {
    unsafe { k_fs_dup(handle) }
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
/// This process's pid, for the calls that have to name it.
///
/// A plain static, because the shim's memory belongs to one guest process --
/// one Worker per process is the whole design. The first attempt put this in
/// the *global* kernel, which every Worker shares, so an end opened under one
/// pid was closed under whichever pid happened to have made a syscall most
/// recently. The removal missed, `k_pipe_forget` closed the end a second
/// time, a reader saw end-of-file early and `bash` died of SIGABRT.
static MY_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

fn my_pid() -> i32 {
    MY_PID.load(std::sync::atomic::Ordering::Relaxed)
}

extern "C" fn rp_open_end(id: i32, write: i32) {
    unsafe { k_pipe_open_end(id, write, my_pid()) }
}
extern "C" fn rp_ends(id: i32) -> i32 {
    unsafe { k_pipe_ends(id) }
}
extern "C" fn rp_write_stats(id: i32) -> i32 {
    unsafe { k_pipe_write_stats(id) }
}
extern "C" fn rp_writes(id: i32) -> i32 {
    unsafe { k_pipe_writes(id) }
}
extern "C" fn rp_last_message(id: i32, at: u32, cap: u32) -> i32 {
    unsafe { k_pipe_last_message(id, at, cap) }
}
extern "C" fn rp_close_end(id: i32, write: i32) {
    unsafe { k_pipe_close_end(id, write, my_pid()) }
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

// The terminal, which is one object for the whole system and not one per
// process. See `gex_kernel::tty`.
extern "C" fn rt_get_termios(at: u32, len: u32) -> i32 {
    unsafe { k_tty_get_termios(at, len) }
}
extern "C" fn rt_set_termios(at: u32, len: u32) -> i32 {
    unsafe { k_tty_set_termios(at, len) }
}
extern "C" fn rt_get_locked(at: u32, len: u32) -> i32 {
    unsafe { k_tty_get_locked(at, len) }
}
extern "C" fn rt_set_locked(at: u32, len: u32) -> i32 {
    unsafe { k_tty_set_locked(at, len) }
}
extern "C" fn rt_winsize() -> u64 {
    unsafe { k_tty_winsize() }
}
extern "C" fn rt_set_winsize(packed: u64) -> i32 {
    unsafe { k_tty_set_winsize(packed) }
}
extern "C" fn rt_fg() -> i32 {
    unsafe { k_tty_fg() }
}
extern "C" fn rt_set_fg(pgrp: i32) {
    unsafe { k_tty_set_fg(pgrp) }
}
extern "C" fn rt_sid() -> i32 {
    unsafe { k_tty_sid() }
}
extern "C" fn rt_set_sid(sid: i32) {
    unsafe { k_tty_set_sid(sid) }
}
extern "C" fn rt_ldisc() -> i32 {
    unsafe { k_tty_ldisc() }
}
extern "C" fn rt_set_ldisc(n: i32) -> i32 {
    unsafe { k_tty_set_ldisc(n) }
}
extern "C" fn rt_bits() -> i32 {
    unsafe { k_tty_bits() }
}
extern "C" fn rt_set_bits(which: i32, on: i32) {
    unsafe { k_tty_set_bits(which, on) }
}
extern "C" fn rt_flush(which: i32) -> i32 {
    unsafe { k_tty_flush(which) }
}
extern "C" fn rt_queued() -> i32 {
    unsafe { k_tty_queued() }
}
extern "C" fn rt_push(at: u32, len: u32) -> i32 {
    unsafe { k_tty_push(at, len) }
}

/// The terminal, reachable without a kernel in hand.
///
/// `Console::write_at` needs the output flags and is called from *inside* the
/// kernel, so it cannot borrow the host to find them. The same object either
/// way: `global_tty` hands this out rather than making a second one, because
/// two terminals disagreeing about `ONLCR` is a bug nobody would look for.
struct TtyHandle(std::sync::LazyLock<Arc<gex_kernel::tty::TtyRef>>);
unsafe impl Sync for TtyHandle {}
static CONSOLE_TTY: TtyHandle = TtyHandle(std::sync::LazyLock::new(make_tty));

fn console_tty() -> &'static gex_kernel::tty::TtyRef {
    &CONSOLE_TTY.0
}

/// The terminal every process shares, if the host gave us one.
///
/// Without a global kernel a process keeps a terminal of its own, which is
/// exactly right for the one case that has no global kernel: a page running a
/// single program that never forks, where "the system's terminal" and "this
/// process's terminal" are the same object.
fn global_tty() -> Arc<gex_kernel::tty::TtyRef> {
    Arc::clone(&CONSOLE_TTY.0)
}

fn make_tty() -> Arc<gex_kernel::tty::TtyRef> {
    if unsafe { k_present() } == 0 {
        return Arc::new(gex_kernel::tty::TtyRef::new());
    }
    Arc::new(gex_kernel::tty::TtyRef::shared(gex_kernel::tty::Remote {
        get_termios: rt_get_termios,
        set_termios: rt_set_termios,
        get_locked: rt_get_locked,
        set_locked: rt_set_locked,
        winsize: rt_winsize,
        set_winsize: rt_set_winsize,
        fg: rt_fg,
        set_fg: rt_set_fg,
        sid: rt_sid,
        set_sid: rt_set_sid,
        ldisc: rt_ldisc,
        set_ldisc: rt_set_ldisc,
        bits: rt_bits,
        set_bits: rt_set_bits,
        flush: rt_flush,
        queued: rt_queued,
        push: rt_push,
    }))
}

/// Where a pipe's buffer goes, when there is a global kernel to put it in.
fn global_pipes() -> Option<gex_kernel::pipe::Remote> {
    if unsafe { k_present() } == 0 {
        return None;
    }
    Some(gex_kernel::pipe::Remote {
        new: rp_new,
        open_end: rp_open_end,
        ends: rp_ends,
        write_stats: rp_write_stats,
        writes: rp_writes,
        last_message: rp_last_message,
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
            dup: rf_dup,
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
    DIRS.0.borrow_mut().clear();
    MODES.0.borrow_mut().clear();
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
        // What this Worker was handed directly first -- a host that gives the
        // whole of stdin up front still does, and a run of one process needs
        // no kernel at all.
        {
            let mut q = self.bytes.borrow_mut();
            let n = buf.len().min(q.len());
            if n > 0 {
                for (i, b) in q.drain(..n).enumerate() {
                    buf[i] = b;
                }
                return Ok(n as u64);
            }
        }
        // Then the shared queue, which is where a keystroke goes now. It is
        // in the kernel rather than here so that whoever has it can write it
        // and wake a *parked* reader: a Worker asleep inside wasm is not
        // running the event loop a `postMessage` would arrive on, which is
        // the whole reason waits used to be sorted into "the page ends this"
        // and "another Worker ends this".
        // SAFETY: scalars, and the kernel refuses a range outside this
        // process's memory rather than writing it.
        if unsafe { k_present() } != 0 {
            let n = unsafe { k_stdin_read(buf.as_mut_ptr() as u32, buf.len() as u32) };
            if n > 0 {
                return Ok(n as u64);
            }
            if n == 0 {
                return Ok(0); // end of input
            }
        }
        Ok(0)
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
        if self.eof.get() || !self.bytes.borrow().is_empty() {
            return true;
        }
        // SAFETY: takes and returns scalars.
        unsafe { k_present() != 0 && k_stdin_ready() != 0 }
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
    devfs.add("tty", Arc::new(Tty));
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

/// `/dev/tty`: the process's controlling terminal, readable and writable.
///
/// A shell with job control opens this before it will run anything. dash's
/// `setjobctl` opens `_PATH_TTY`, and when the open fails it falls back to
/// whichever of the first three descriptors is a terminal and dups that out
/// of the way -- and *that* is the descriptor a forked child could not
/// rebuild. Providing the device is the honest fix as well as the cheaper
/// one: `/dev/tty` is what `less`, `ssh` and `sudo` reach for too, and none
/// of them accepts a substitute.
#[derive(Debug)]
struct Tty;

impl FileHandle for Tty {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> SysResult {
        STDIN.read_at(offset, buf)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> SysResult {
        Console { fd: 1 }.write_at(offset, buf)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: gex_kernel::vfs::FileType::CharDevice,
            size: 0,
            mode: 0o620,
            ino: 3,
            mtime: 0,
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// The whole point of the device: the kernel answers `TIOCSPGRP` and the
    /// rest only for a descriptor that says it is a terminal.
    fn is_tty(&self) -> bool {
        unsafe { STDIN_TTY }
    }

    fn poll_readable(&self) -> bool {
        STDIN.poll_readable()
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
        // `c_oflag` first, when this console is a terminal. `ONLCR` is why a
        // Unix program can write `\n` and have the cursor come back to the
        // left margin; without it every line starts one column further right
        // than the last, which is the staircase a serial terminal gives you.
        // A program that turns `OPOST` off -- `less`, `vi`, anything that
        // places its own cursor -- gets its bytes through untouched, which is
        // exactly why this cannot be hard-coded in the page.
        if unsafe { STDIN_TTY } {
            if let Some(cooked) = console_tty().output(buf) {
                unsafe { write(self.fd, cooked.as_ptr() as u32, cooked.len() as u32) };
                // The count returned is what the guest asked to write, not
                // what the translation made of it: a `write` that says it
                // wrote more bytes than it was given sends libc into a loop
                // over a buffer that has already gone.
                return Ok(buf.len() as u64);
            }
        }
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
/// How many of an object's stored units one dispatch miss may take, merged
/// into a single module. See the use site in the miss path.
static PER_MISS_N: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(16);

/// Set that, from the host.
#[no_mangle]
pub extern "C" fn gex_set_per_miss(n: u32) {
    if n > 0 {
        PER_MISS_N.store(n as usize, std::sync::atomic::Ordering::Relaxed);
    }
}

/// What the arena has been spent on, by whoever asked.
///
/// Three consumers share one cursor and only one of them goes through the
/// memory map, which is how a fix aimed at the map turned out to be inert on
/// the path that actually fails. These say which is which before anything is
/// changed again: `[0]` the guest's mappings, `[1]` this module's heap, `[2]`
/// the per-Worker stacks. `[3]` is the largest single claim.
static ARENA_SPENT: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Bytes in `Jit::pending`, and the longest that queue has ever been.
static PENDING_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PENDING_MAX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many times talc has asked the arena for another span, and the largest
/// allocation that forced one. See #140.
static HEAP_REFILLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HEAP_BIGGEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bytes held by `Jit::registry`, for the arena report. See #140.
static REGISTRY_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn arena_note(who: usize, len: u64) {
    ARENA_SPENT[who].fetch_add(len, std::sync::atomic::Ordering::Relaxed);
    ARENA_SPENT[3].fetch_max(len, std::sync::atomic::Ordering::Relaxed);
}

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
    //
    // And above whatever the memory already reaches, which is the part that
    // took three days to find. This cursor is a static of *this module
    // instance*, so it is zero in every freshly instantiated shim -- but a
    // forked child instantiates a fresh shim over a memory that already holds
    // its parent's entire address space, every byte of it handed out from
    // `ARENA_BOTTOM` upwards. Starting at `ARENA_BOTTOM` there hands the
    // child's first claim a span the parent is still using, and nothing
    // notices: `MemoryMap::mmap` inserts an arena-extended range without
    // asking whether it is free, because the arena is meant to be the one
    // allocator that cannot collide.
    //
    // What claims it first is usually this module's own heap, so the damage
    // lands wherever the parent's mappings happen to be -- and the guest
    // meets it later as heap corruption somewhere else entirely. `apt update`
    // died in `malloc_consolidate(): invalid chunk size` in a forked child,
    // moved when an unrelated string changed length, and did not happen under
    // a closure at all: a child running code somebody else translated barely
    // allocates, so it never claimed a span. See #96.
    //
    // The memory is grown to cover the whole inherited image before this
    // module is instantiated, so its size is exactly the floor wanted. For a
    // process that was not forked it is below `ARENA_BOTTOM` and this is the
    // behaviour it always had.
    let at = if at == 0 {
        gex_abi::ARENA_BOTTOM.max(warch::memory_size(0) as u64 * 65536)
    } else {
        at
    };
    let end = at.checked_add(len)?;
    if end > gex_abi::MMAP_TOP || !grow_memory_to(end) {
        // Out loud, because the caller cannot say it.
        //
        // Both callers turn `None` into an allocation failure, and Rust's
        // allocation failure is not a panic: `handle_alloc_error` aborts
        // without going near the panic hook, so the run ends as a bare
        // `unreachable` with nothing printed. `apt-get update` reported only
        //
        //     trapped inside the shim (unreachable, after syscall 63)
        //
        // which names the guest's pc and says nothing about the arena being
        // the thing that ran out. This is the only place that knows.
        // Without allocating, which is the whole difficulty.
        //
        // This *is* the allocator's refill path: `ShimRegion::acquire` calls
        // it, and `format!` asks the allocator for a string. Written the
        // obvious way, a failure formatted a message, which allocated, which
        // refilled, which failed, which formatted -- and `apt-get update`
        // went from aborting with no reason to hanging with no reason, which
        // is worse. A fixed buffer on the stack and `core::fmt` into it
        // touches no allocator at all.
        use core::fmt::Write as _;
        struct Buf([u8; 768], usize);
        impl core::fmt::Write for Buf {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let room = self.0.len() - self.1;
                let n = s.len().min(room);
                self.0[self.1..self.1 + n].copy_from_slice(&s.as_bytes()[..n]);
                self.1 += n;
                Ok(())
            }
        }
        // 768, because the report has grown. `write!` into a full buffer
        // stops silently, and the last thing added is the first thing lost --
        // the queue high-water mark vanished off the end of a 448-byte one
        // and read as absent rather than as truncated.
        let mut b = Buf([0; 768], 0);
        let _ = write!(
            b,
            "[gex] the arena cannot grow: wanted {len} at {at:#x}, ceiling \
             {:#x}. Spent: {} MiB on guest mappings, {} MiB on this module's \
             heap, {} MiB on Worker stacks; largest single claim {}. Live in \
             the heap right now: {} MiB, of which {} MiB is kept module \
             bytes and {} MiB is files the kernel holds. {} refills, \
             biggest allocation {} (-1 KiB means the counters are off; build \
             with GEX_HEAP_COUNTERS=1). {} MiB queued for the host in {} \
             modules at most. An abort here is not a panic and prints \
             nothing of its own.\n",
            gex_abi::MMAP_TOP,
            ARENA_SPENT[0].load(std::sync::atomic::Ordering::Relaxed) >> 20,
            ARENA_SPENT[1].load(std::sync::atomic::Ordering::Relaxed) >> 20,
            ARENA_SPENT[2].load(std::sync::atomic::Ordering::Relaxed) >> 20,
            ARENA_SPENT[3].load(std::sync::atomic::Ordering::Relaxed),
            gex_heap_used_kib() / 1024,
            REGISTRY_BYTES.load(std::sync::atomic::Ordering::Relaxed) >> 20,
            // A static in the kernel, read directly: this runs inside the
            // allocator's refill path and must not borrow the host.
            gex_kernel::vfs::tmpfs::TMPFS_BYTES
                .load(std::sync::atomic::Ordering::Relaxed)
                >> 20,
            HEAP_REFILLS.load(std::sync::atomic::Ordering::Relaxed),
            HEAP_BIGGEST.load(std::sync::atomic::Ordering::Relaxed),
            PENDING_BYTES.load(std::sync::atomic::Ordering::Relaxed) >> 20,
            PENDING_MAX.load(std::sync::atomic::Ordering::Relaxed),
        );
        // SAFETY: `warn` reads the bytes and keeps none of them.
        unsafe { warn(b.0.as_ptr() as u32, b.1 as u32) };
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
    arena_note(0, len);
    arena_take(len).unwrap_or(0)
}

// SAFETY: `acquire` claims a region of this module's own linear memory and
// touches no other allocator.
/// Where this module's heap currently ends, so a new span can extend it.
///
/// `talc` keeps each claimed span separate; only `extend` joins new memory to
/// one it already owns, and that needs the old end. Zero means nothing has
/// been claimed yet.
static HEAP_END: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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
        arena_note(1, len);
        // How many refills, and the biggest single allocation that forced
        // one. 1,880 MiB in 16 MiB chunks is about 117 refills whatever the
        // shape, so the count alone says little -- but the largest *layout*
        // separates one buffer growing by doubling from many live objects.
        // See #140.
        HEAP_REFILLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        HEAP_BIGGEST.fetch_max(_layout.size() as u64, std::sync::atomic::Ordering::Relaxed);
        let base = arena_take(len).ok_or(())? as *mut u8;
        // Extend the last span when this one abuts it, rather than claiming a
        // second one beside it.
        //
        // `claim` gives talc a span it keeps separate from every other, so a
        // request larger than any single span cannot be served from two of
        // them however adjacent they are -- and `arena_take` hands out one
        // cursor that only goes up, so they usually *are* adjacent. A buffer
        // that grows by doubling therefore walked the arena upwards, taking a
        // fresh chunk each time and stranding everything behind it.
        //
        // Measured on `apt-get update`: 1,882 MiB of a 2 GiB arena spent by
        // this heap, in about 117 chunks, with the largest single request
        // 33.9 MiB. The guest's own mappings were 8 MiB of it.
        //
        // Not assumed adjacent: the guest's mappings and the per-Worker
        // stacks draw from the same cursor, so a claim can land above one of
        // theirs. The pointers are compared rather than trusted.
        let end = HEAP_END.load(std::sync::atomic::Ordering::Relaxed);
        let new_end = unsafe {
            if end != 0 && end == base as usize {
                // SAFETY: the span starts exactly where the heap ended, the
                // arena handed it to nobody else, and the memory behind it
                // has been grown.
                let old = std::ptr::NonNull::new_unchecked(end as *mut u8);
                talc.extend(old, base.add(len as usize))
            } else {
                // SAFETY: as above, minus the abutment.
                talc.claim(base, len as usize).ok_or(())?
            }
        };
        HEAP_END.store(new_end.as_ptr() as usize, std::sync::atomic::Ordering::Relaxed);
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
    /// Translation units the system-wide store accepted, and units it turned
    /// away. A unit it refuses can never be shared, however well the identity
    /// of its pages matches.
    store_ok: u64,
    store_full: u64,
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
    /// Blocks lifted, by the object they were lifted from.
    ///
    /// The total alone cannot say whether a closure is missing something. A
    /// `/bin/true` against a closure with libc whole still lifts 4,308
    /// blocks, and its own text is 714 of them -- so 3,600 come from
    /// somewhere the closure was supposed to cover, and nothing said where.
    /// Blocks lifted on a page a closure library already claims.
    lift_in_closure: u64,
    /// Blocks lifted per object, with the address range they came from.
    ///
    /// The range is what says whether a closure is missing an object or
    /// missing part of one: `libc.so.6=10239` reads the same whether the
    /// closure covers none of libc or all of it but the last page.
    by_object: std::collections::BTreeMap<String, (u64, u64, u64)>,
    compiles: u64,
    /// Nanoseconds spent lifting and encoding, which is the shim's own share
    /// of a miss -- the engine's compile of what comes out is JS's.
    nanos: u64,
}

/// Written at both ends of [`Host`] and checked on the way into every
/// syscall. See [`HOST_MAGIC_HEAD`].
const HOST_MAGIC: u64 = 0x6765_785f_484f_5354;

struct Host {
    /// A canary, because the failures this shim has are memory corruption and
    /// they are always found somewhere other than where they happened.
    ///
    /// The arena overlap in 02fd80b was a forked child's own heap handed
    /// addresses its parent was using: it wrote through the guest's heap and
    /// surfaced, minutes later, as `malloc_consolidate(): invalid chunk size`
    /// in a program that had done nothing wrong. The one the demo site still
    /// dies of is `table index is out of bounds` inside `execve`, with a
    /// dispatch directory that verifies clean in all 524,288 entries -- so
    /// the index is not the guest's, it is one of this module's own funcref
    /// calls, and the only way to get one of those wrong is for the `Host`
    /// holding it to have been written over. `kernel.net` is an
    /// `Arc<dyn NetStack>`, `kernel.place` an `Arc<dyn ...>`, `kernel.clock` a
    /// function pointer; all three live in here.
    ///
    /// Two words, one at each end, so a write that lands anywhere across the
    /// structure is caught by one of them. Checked at syscall entry, which is
    /// a load and a compare against a constant and costs nothing measurable,
    /// and which turns "something is wrong, elsewhere, later" into "the call
    /// before this one did it".
    magic_head: u64,
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
    /// The other end of the canary. See `magic_head`.
    magic_tail: u64,
}

/// Check the canary, and say so loudly once if it has gone.
///
/// Reported rather than trapped: the run is already wrong by the time this
/// notices, and a trap here would replace the crash that is being diagnosed
/// with a different one at a different place. Once, because a corrupted
/// `Host` will fail this on every syscall afterwards and a flood is not a
/// diagnosis.
fn check_host_magic(where_: &str) {
    if gex_has_host() == 0 {
        return;
    }
    let (head, tail) = with_host(|h| (h.magic_head, h.magic_tail));
    if head == HOST_MAGIC && tail == HOST_MAGIC {
        return;
    }
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    trace_line(&format!(
        "gex: the host structure has been written over, seen at {where_}: \
         head={head:#x} tail={tail:#x}, both should be {HOST_MAGIC:#x}. \
         Whatever runs after this is running on a corrupt kernel.\n"
    ));
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
            // Straight to the host's console, not through fd 2.
            //
            // `write` is a *syscall*: it takes the kernel. Almost every panic
            // worth reading happens with the kernel already taken -- that is
            // what `with_host` panics about -- so the hook took the borrow a
            // second time, panicked inside the panic, and the run ended as a
            // bare `unreachable` with nothing said. Which is how
            // `apt-get update` reported only
            //
            //     trapped inside the shim (unreachable, after syscall 63)
            //
            // for a panic that knew its own file, line and message.
            //
            // `warn` is a direct import to `console.error` and touches no
            // kernel, so it cannot re-enter. Nothing here may take a borrow.
            warn_line(&text);
            // And fd 2 as well, so a run whose output is the terminal still
            // shows it -- but only after the console has it, because this is
            // the call that can die.
            // SAFETY: the bytes are read, not kept.
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
        // `try_borrow`, because this is asked *after* a crash and a wasm trap
        // does not unwind: whatever trapped still holds the borrow. `borrow`
        // panics there, and the panic replaces the crash report with
        // "RefCell already mutably borrowed" -- which is what every trace of
        // the nested-subshell wedge ended with, one line before the part
        // that would have said where the guest was.
        //
        // A host that cannot be borrowed is a host that exists, so the
        // honest answer to "is there one" is yes.
        HOST.with(|h| match h.try_borrow() {
            Ok(b) => b.is_some(),
            Err(_) => true,
        }) as i32
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

/// The last few syscalls this process made, for the report when it dies.
///
/// A process that dies of a signal it raised itself says nothing about why:
/// `abort` is `raise(SIGABRT)` and glibc prints a diagnostic only for the
/// cases that have one, so `dpkg-deb`'s decompressing child aborting after
/// exactly 157 pipe-fulls left the whole run with one line of evidence and
/// it was written by the *parent*. What led there is the last thing the
/// child asked the kernel for -- a short read, a failed mapping, a write
/// that returned something it did not expect -- and that was not recorded
/// anywhere.
///
/// Four words an entry, packed into one atomic each: number, first two
/// arguments, and what came back. Costs four stores on a path that already
/// does far more than that, so it is always on -- the run worth recording is
/// never the one you thought to ask about beforehand.
///
/// Long enough to hold a process's whole conversation with the kernel when
/// the process is short, which the ones that die usually are.
const SYS_RING: usize = 256;
static SYS_NR: [std::sync::atomic::AtomicU64; SYS_RING] =
    [const { std::sync::atomic::AtomicU64::new(!0) }; SYS_RING];
static SYS_A0: [std::sync::atomic::AtomicU64; SYS_RING] =
    [const { std::sync::atomic::AtomicU64::new(0) }; SYS_RING];
static SYS_A1: [std::sync::atomic::AtomicU64; SYS_RING] =
    [const { std::sync::atomic::AtomicU64::new(0) }; SYS_RING];
static SYS_RET: [std::sync::atomic::AtomicU64; SYS_RING] =
    [const { std::sync::atomic::AtomicU64::new(0) }; SYS_RING];
static SYS_AT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The last few things a process did to its address space.
///
/// A ring of everything is the wrong instrument for a memory question: a
/// decompressing child's last 256 calls are 147 futexes and 102 writes, and
/// the `mmap` that decided its fate was thousands of calls earlier.
///
/// The question this exists for is `free(): invalid pointer`, which is what
/// glibc says when a chunk's address does not resolve to a heap it knows.
/// A non-main arena is a 64 MiB-aligned region that glibc builds by mapping
/// twice that and unmapping the misaligned ends, and `heap_for_ptr` is a
/// mask -- so an `mmap` that ignores the length it was asked for, or a
/// `munmap` that will not release part of a mapping, produces exactly that
/// message and nothing else.
const MEM_RING: usize = 32;
static MEM_NR: [std::sync::atomic::AtomicU64; MEM_RING] =
    [const { std::sync::atomic::AtomicU64::new(!0) }; MEM_RING];
static MEM_A0: [std::sync::atomic::AtomicU64; MEM_RING] =
    [const { std::sync::atomic::AtomicU64::new(0) }; MEM_RING];
static MEM_A1: [std::sync::atomic::AtomicU64; MEM_RING] =
    [const { std::sync::atomic::AtomicU64::new(0) }; MEM_RING];
static MEM_RET: [std::sync::atomic::AtomicU64; MEM_RING] =
    [const { std::sync::atomic::AtomicU64::new(0) }; MEM_RING];
static MEM_AT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Every futex, with what it was told.
///
/// glibc's `futex_wait` tolerates 0, EAGAIN, EINTR and ETIMEDOUT and calls
/// `futex_fatal_error` for everything else -- EFAULT, EINVAL and ENOSYS
/// included -- so "the futex facility returned an unexpected error code" can
/// come from four different answers and the process aborts without saying
/// which. Guessing them one at a time excluded three and found nothing;
/// this records all of them, resumptions included, and prints them when a
/// process dies.
const FTX_RING: usize = 24;
static FTX_OP: [std::sync::atomic::AtomicU64; FTX_RING] =
    [const { std::sync::atomic::AtomicU64::new(!0) }; FTX_RING];
static FTX_ADDR: [std::sync::atomic::AtomicU64; FTX_RING] =
    [const { std::sync::atomic::AtomicU64::new(0) }; FTX_RING];
static FTX_RET: [std::sync::atomic::AtomicI64; FTX_RING] =
    [const { std::sync::atomic::AtomicI64::new(0) }; FTX_RING];
static FTX_AT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `op` is the raw futex operation, or `!0` for the resumption of a wait
/// that had parked -- the answer the syscall itself never recorded.
fn ftx_note(op: u64, addr: u64, ret: i64) {
    use std::sync::atomic::Ordering::Relaxed;
    let i = FTX_AT.fetch_add(1, Relaxed) % FTX_RING;
    FTX_OP[i].store(op, Relaxed);
    FTX_ADDR[i].store(addr, Relaxed);
    FTX_RET[i].store(ret, Relaxed);
}

fn ftx_recent() -> String {
    use std::sync::atomic::Ordering::Relaxed;
    let n = FTX_AT.load(Relaxed);
    let mut out = String::new();
    for j in n.saturating_sub(FTX_RING)..n {
        let i = j % FTX_RING;
        let op = FTX_OP[i].load(Relaxed);
        let (addr, ret) = (FTX_ADDR[i].load(Relaxed), FTX_RET[i].load(Relaxed));
        if op == !0 {
            out.push_str(&format!(" resume({addr:#x})={ret}"));
        } else {
            out.push_str(&format!(" {addr:#x} op={op:#x} = {ret}"));
        }
    }
    out
}

/// `brk`, `munmap`, `mremap`, `mmap`.
fn is_mem_call(nr: u64) -> bool {
    matches!(nr, 214 | 215 | 216 | 222)
}

fn mem_note(nr: u64, a0: u64, a1: u64, ret: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let i = MEM_AT.fetch_add(1, Relaxed) % MEM_RING;
    MEM_NR[i].store(nr, Relaxed);
    MEM_A0[i].store(a0, Relaxed);
    MEM_A1[i].store(a1, Relaxed);
    MEM_RET[i].store(ret, Relaxed);
}

fn mem_recent() -> String {
    use std::sync::atomic::Ordering::Relaxed;
    let n = MEM_AT.load(Relaxed);
    let mut out = String::new();
    for j in n.saturating_sub(MEM_RING)..n {
        let i = j % MEM_RING;
        let nr = MEM_NR[i].load(Relaxed);
        if nr == !0 {
            continue;
        }
        out.push_str(&format!(
            " {}({:#x}, {:#x})={:#x}",
            gex_kernel::syscalls::syscall_name(nr),
            MEM_A0[i].load(Relaxed),
            MEM_A1[i].load(Relaxed),
            MEM_RET[i].load(Relaxed),
        ));
    }
    out
}

/// Record a syscall on entry, and return where to put its result.
fn sys_note(nr: u64, a0: u64, a1: u64) -> usize {
    use std::sync::atomic::Ordering::Relaxed;
    let i = SYS_AT.fetch_add(1, Relaxed) % SYS_RING;
    SYS_NR[i].store(nr, Relaxed);
    SYS_A0[i].store(a0, Relaxed);
    SYS_A1[i].store(a1, Relaxed);
    SYS_RET[i].store(!0, Relaxed);
    i
}

fn sys_ret(i: usize, ret: u64) {
    SYS_RET[i].store(ret, std::sync::atomic::Ordering::Relaxed);
}

/// The ring, oldest first, as `name(a0, a1) = ret`.
fn sys_recent() -> String {
    use std::sync::atomic::Ordering::Relaxed;
    let n = SYS_AT.load(Relaxed);
    let mut out = String::new();
    // From the oldest entry still held, which is not `n % RING` until the
    // ring has wrapped: a process that made three calls has them at 0, 1, 2
    // and starting at 3 reads nothing but empty slots. The syscall ring hid
    // that because its count is always far past its size; the memory ring
    // beside it printed nothing at all and that is how it was found.
    for j in n.saturating_sub(SYS_RING)..n {
        let i = j % SYS_RING;
        let nr = SYS_NR[i].load(Relaxed);
        if nr == !0 {
            continue;
        }
        out.push_str(&format!(
            " {}({:#x}, {:#x})={:#x}",
            gex_kernel::syscalls::syscall_name(nr),
            SYS_A0[i].load(Relaxed),
            SYS_A1[i].load(Relaxed),
            SYS_RET[i].load(Relaxed),
        ));
    }
    out
}

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

/// The host, but only if nobody is inside it.
///
/// For the questions a crash reporter asks. Everything else must use
/// `with_host` and be told loudly when it re-enters, because re-entry is a
/// real bug; a reporter is the one caller for which re-entry is *expected* --
/// it runs after a trap, and a trap leaves whatever it interrupted holding
/// the borrow forever. `None` means "cannot say", which is a worse answer
/// than the address and a far better one than a panic that replaces the
/// crash with a complaint about the reporter.
#[cfg(not(feature = "shared-memory"))]
fn with_host_free<R>(f: impl FnOnce(&mut Host) -> R) -> Option<R> {
    HOST.with(|h| match h.try_borrow_mut() {
        Ok(mut b) => b.as_mut().map(f),
        Err(_) => None,
    })
}

/// As above. The shared build has no `RefCell` to ask -- it borrows a static
/// in place under `KLock` -- so the honest answer is the same one `with_host`
/// gives, minus the initialisation panic.
#[cfg(feature = "shared-memory")]
fn with_host_free<R>(f: impl FnOnce(&mut Host) -> R) -> Option<R> {
    // SAFETY: as `with_host`. Read-mostly and only from a reporter, which
    // runs when the guest has already stopped.
    unsafe { (*core::ptr::addr_of_mut!(HOST)).as_mut().map(f) }
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
    arena_note(2, (slots::STACK + slots::TLS) as u64);
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

/// The whole memory map as bytes, for a child to rebuild from.
///
/// `gex_vma_at` gives three numbers per region, and three numbers is not a
/// mapping. What a forked child needs to know is also what each region *is*:
/// its name, and the file and offset its code came from. Without those the
/// child's map is a single anonymous span -- it was literally called
/// `[forked]` -- and nothing in it can be identified, so the child cannot
/// find a page in the shared store and re-translates its parent's code from
/// cold. In Chromium that was every miss of every process: `hit=0`,
/// `served=0`, eight thousand blocks lifted again per command.
///
/// One blob rather than a call per field, because parent and child are
/// different instances in different Workers and everything between them is
/// bytes anyway.
///
///   u32 count, then per region:
///   u64 start, u64 end, u32 prot,
///   u32 name_len, name, u32 code_len, code, u64 code_off
///
/// A zero `code_len` means no code identity. `origin` is deliberately absent:
/// it says a region can be rebuilt by re-reading a file, which is a claim
/// about *this* process's mappings and not something to inherit.
fn vma_blob(h: &Host) -> Vec<u8> {
    let vmas = h.kernel.mem.vmas();
    let mut out = Vec::with_capacity(64 * vmas.len());
    out.extend_from_slice(&(vmas.len() as u32).to_le_bytes());
    for v in vmas {
        out.extend_from_slice(&v.start.to_le_bytes());
        out.extend_from_slice(&v.end.to_le_bytes());
        out.extend_from_slice(&v.prot.to_le_bytes());
        out.extend_from_slice(&(v.name.len() as u32).to_le_bytes());
        out.extend_from_slice(v.name.as_bytes());
        match &v.code {
            Some((id, off)) => {
                out.extend_from_slice(&(id.len() as u32).to_le_bytes());
                out.extend_from_slice(id.as_bytes());
                out.extend_from_slice(&off.to_le_bytes());
            }
            None => {
                out.extend_from_slice(&0u32.to_le_bytes());
                out.extend_from_slice(&0u64.to_le_bytes());
            }
        }
    }
    out
}

#[no_mangle]
pub extern "C" fn gex_vma_blob_len() -> u32 {
    with_host(|h| vma_blob(h).len() as u32)
}

#[no_mangle]
pub extern "C" fn gex_vma_blob_read(out: u32, cap: u32) -> u32 {
    with_host(|h| {
        let blob = vma_blob(h);
        if blob.len() as u32 > cap {
            return 0;
        }
        shim_slice_mut(out, blob.len() as u32).copy_from_slice(&blob);
        blob.len() as u32
    })
}

/// Rebuild the child's map from what [`vma_blob`] wrote.
#[no_mangle]
pub extern "C" fn gex_adopt_vma_blob(ptr: u32, len: u32) -> i32 {
    let raw = shim_slice(ptr, len);
    let mut p = 0usize;
    let mut u32at = |p: &mut usize| -> u32 {
        let v = u32::from_le_bytes(raw[*p..*p + 4].try_into().unwrap());
        *p += 4;
        v
    };
    if raw.len() < 4 {
        return -1;
    }
    let count = u32at(&mut p);
    with_host(|h| {
        for _ in 0..count {
            if p + 24 > raw.len() {
                return -1;
            }
            let start = u64::from_le_bytes(raw[p..p + 8].try_into().unwrap());
            let end = u64::from_le_bytes(raw[p + 8..p + 16].try_into().unwrap());
            p += 16;
            let prot = u32at(&mut p);
            let nlen = u32at(&mut p) as usize;
            let name = String::from_utf8_lossy(&raw[p..p + nlen]).into_owned();
            p += nlen;
            let clen = u32at(&mut p) as usize;
            let code = if clen == 0 {
                p += 8;
                None
            } else {
                let id = String::from_utf8_lossy(&raw[p..p + clen]).into_owned();
                p += clen;
                let off = u64::from_le_bytes(raw[p..p + 8].try_into().unwrap());
                p += 8;
                Some((id, off))
            };
            h.kernel.mem.adopt(start, end, prot, &name, code);
        }
        0
    })
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
    PROCESS_STARTED.store(now_ns(), std::sync::atomic::Ordering::Relaxed);
    // And the same for the first process, which opens its console before it
    // makes a syscall. See the note beside the other one.
    MY_PID.store(kernel.pid, std::sync::atomic::Ordering::Relaxed);
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
    kernel.tty = global_tty();
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
    kernel.gid = unsafe { IDENTITY.1 };
    // Replace the descriptors `Kernel::new` opened on `std::io` with ones the
    // host can actually see.
    install_console(&mut kernel);

    let data = guest_bytes();
    for (seg, from) in program.placements() {
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
        //
        // It still gets a code identity when there is one to give. `gex_load`
        // is handed bytes and no path, so process 1's own text cannot be
        // named -- but its *interpreter* can, and the interpreter is the one
        // object every other process in the run also loads. Without this,
        // pid 1 lifts `ld.so` and stores it under no identity at all, and the
        // next process lifts the same linker over again.
        // The image's own identity comes from the manifest when there is one.
        //
        // `gex_load` is handed bytes and no path, so the executable could not
        // be named -- and `image_pages_are_ours` asks exactly that question
        // before handing a process the closure's image pages. Unnamed, pid 1
        // would be refused the pages built for it and would lift all 18,595
        // blocks of its own text. The page told us which program this is when
        // it set `IMAGE_PROGRAM`; that is the honest answer and the only one
        // available here.
        let whose = from.map(|f| f.to_string()).or_else(|| {
            let w = IMAGE_PROGRAM.0.borrow();
            (!w.is_empty()).then(|| w.clone())
        });
        kernel.mem.insert_loaded(
            seg.vaddr,
            seg.vaddr + seg.mem_size,
            p,
            "[image]",
            whose.map(|f| (f, seg.vaddr)),
        );
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
    // The environment pid 1 starts with, and so the one every process in the
    // run inherits.
    //
    // It was `PATH=/usr/bin:/bin`, which has no `sbin` in it at all -- and
    // dpkg looks for `ldconfig` and `start-stop-daemon`, both of which live
    // in `/sbin`. It says so, and the advice is exactly right:
    //
    //   dpkg: warning: 'ldconfig' not found in PATH or not executable
    //   Note: root's PATH should usually contain /usr/local/sbin,
    //         /usr/sbin and /sbin
    //
    // This is Debian's own default for root. A page that wants something
    // else says so with `gex_add_env`, which is how `TERM` and
    // `DEBIAN_FRONTEND` arrive rather than being guessed at here.
    let env: Vec<String> = {
        let given = std::mem::take(&mut *ENV.0.borrow_mut());
        if given.is_empty() {
            vec![
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
                "HOME=/root".to_string(),
            ]
        } else {
            given
        }
    };
    // `/proc/self/{cmdline,exe}` read these. A relative argv[0] is resolved
    // against the root, which is where a bundled program lives.
    kernel.exe_path = if args[0].starts_with('/') {
        args[0].clone()
    } else {
        format!("/{}", args[0])
    };
    kernel.argv = args.clone();
    kernel.envp = env.to_vec();
    // Process one links for itself and leaves the answer for whoever runs the
    // same program next. There is nothing to restore from: it is the first.
    let hook = if snapshots_on() && program.interp.is_some() {
        SNAP_STUB
    } else {
        0
    };
    let snap_key_1 = snap_key(&kernel.exe_path, &kernel.envp);
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
            entry_hook: hook,
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
    if hook != 0 {
        snap_arm(&mut kernel, snap_key_1, &program.exe, program.interp_base());
    }

    let meta = gex_xlate::Metadata::decode(&METADATA.0.borrow()).unwrap_or_default();
    let entry = program.entry();
    set_host(Host {
        magic_head: HOST_MAGIC,
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
                page_table: shared_page_table(&meta.dir_entries, meta.next_slot, meta.n_blocks),
                translated: Default::default(),
                registry: Vec::new(),
                shared: Default::default(),
                installed_mods: Default::default(),
                reuse_have: 0,
                reuse_hit: 0,
                reuse_miss: 0,
                reuse_noident: 0,
                reuse_served: 0,
                store_ok: 0,
                store_full: 0,
                intrinsics: Default::default(),
                intrinsics_scanned: Default::default(),
                candidates: Vec::new(),
                pending: Default::default(),
                blocks: 0,
                lift_in_closure: 0,
            by_object: Default::default(),
                compiles: 0,
                nanos: 0,
            },
        magic_tail: HOST_MAGIC,
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

/// Pins already reported, so each is said once and not once per syscall.
static PINS_SAID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Say where any newly reserved pin landed, the moment it has landed.
///
/// A misplacement is loud whatever the tracing settings, because the guest is
/// about to branch into code built for a different address; a pin that took
/// is only worth a line when something is listening.
fn report_new_pins() {
    let said = PINS_SAID.load(std::sync::atomic::Ordering::Relaxed);
    let fresh = with_host(|h| {
        if h.kernel.pin_landed.len() <= said {
            return Vec::new();
        }
        h.kernel.pin_landed[said..].to_vec()
    });
    if fresh.is_empty() {
        return;
    }
    PINS_SAID.fetch_add(fresh.len(), std::sync::atomic::Ordering::Relaxed);
    for (path, want, got) in fresh {
        if want == got {
            if tracing() || unsafe { TRACE_MISSES } {
                trace_line(&format!("[gex] pin ok: {path} at {got:#x}\n"));
            }
        } else {
            warn_line(&format!(
                "[gex] pin MISPLACED: {path} wanted {want:#x}, landed {got:#x}\n"
            ));
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

/// Make a bundled file executable.
///
/// Every file in the guest is 0644, because that is the VFS default and the
/// tar's mode never travelled with its bytes. dpkg checks, and says so about
/// programs that are plainly present:
///
///   dpkg: warning: 'sh' not found in PATH or not executable
///   dpkg: error: 7 expected programs not found in PATH or not executable
///
/// Only the modes that differ from the default are sent, so this is the few
/// hundred binaries in a rootfs rather than a call per file.
#[no_mangle]
pub extern "C" fn gex_chmod(path: u32, path_len: u32, mode: u32) {
    let Ok(name) = std::str::from_utf8(shim_slice(path, path_len)) else {
        return;
    };
    // SAFETY: scalars naming a range of this module's memory, with no borrow
    // held across the call. See `gex_add_file`.
    if unsafe { k_present() } != 0 {
        if unsafe { k_fs_populated() } == 0 {
            unsafe { k_fs_chmod(path, path_len, mode) };
        }
        return;
    }
    MODES.0.borrow_mut().push((name.to_string(), mode));
}

/// A directory the bundle contains, whether or not anything is in it.
///
/// `gex_add_file` creates the directories on the way to each file, so a tree
/// with content in it arrives intact. An *empty* directory has no file to
/// carry it and simply did not exist in the guest -- and a program that
/// expects one gets an error naming a path the rootfs plainly contains:
///
///   E: Archives directory /var/cache/apt/archives/partial is missing.
///   W: Unable to read /etc/apt/preferences.d/ - DirectoryExists (2: ...)
///
/// Both of those are in the tar. `web/src/tar.js` even collects the
/// directory entries into `dirs` -- and every caller dropped them on the
/// floor, because there was nothing here to hand them to.
#[no_mangle]
pub extern "C" fn gex_add_dir(path: u32, path_len: u32) {
    let Ok(name) = std::str::from_utf8(shim_slice(path, path_len)) else {
        return;
    };
    // SAFETY: the same shape as `gex_add_file` above -- scalars naming a
    // range of this module's memory, and no borrow held across the call.
    if unsafe { k_present() } != 0 {
        if unsafe { k_fs_populated() } == 0 {
            unsafe { k_fs_mkdir(path, path_len, 0o755) };
        }
        return;
    }
    DIRS.0.borrow_mut().push(name.to_string());
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

/// Which closure library covers each page, by the order they were added.
///
/// The host needs this to instantiate a library only when a branch reaches
/// it. Slots are already written on demand -- see the fill helpers -- but the
/// *instances* were still all built at process start: 37 seconds of an apt
/// run that takes 61, for twenty-nine libraries of which apt touches a
/// handful. `libgcrypt` is 304 MiB and this run calls almost none of it.
static CLOSURE_OWNER: SingleThreaded<std::collections::BTreeMap<u64, u32>> =
    SingleThreaded(const { RefCell::new(std::collections::BTreeMap::new()) });

/// How many closure libraries have been registered, which is the ordinal the
/// next one gets. The host adds them in the same order, so the number it
/// hears back indexes its own list.
static CLOSURE_N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The same, for modules a fork handed over rather than a closure build.
/// Separate from `CLOSURE_OWNER` because `execve` keeps one and not the other.
static LAZY_OWNER: SingleThreaded<std::collections::BTreeMap<u64, u32>> =
    SingleThreaded(const { RefCell::new(std::collections::BTreeMap::new()) });
static LAZY_N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The closure library covering this page, or -1.
#[no_mangle]
pub extern "C" fn gex_closure_owner(page: u64) -> i32 {
    if page * gex_abi::DISPATCH_PAGE < gex_abi::INTERP_BASE {
        let claims = IMAGE_PAGES.0.borrow();
        let Some(idxs) = claims.get(&page) else {
            return -1;
        };
        return with_host(|h| {
            idxs.iter()
                .find(|i| image_is_ours(h, **i))
                .map(|i| *i as i32)
                .unwrap_or(-1)
        });
    }
    CLOSURE_OWNER
        .0
        .borrow()
        .get(&page)
        .map(|v| *v as i32)
        .unwrap_or(-1)
}

/// The `k`th closure object that covers `page` and belongs to this process.
///
/// `gex_closure_owner` answers with the *first* one, and for an image that is
/// not enough. A program's translation is split into batched units (#98), and
/// the split is by block, not by page: page 4099 of `/bin/bash` is covered by
/// object 38, which holds four of its blocks, while the rest of that page
/// lives in sibling objects. Asking only the first owner fills those four
/// slots and leaves the page looking answered, so every other block on it
/// falls past the fill to the translator for the life of the process --
/// 15,306 blocks of bash in a process that execs, against 6 in the one that
/// does not. See #136.
#[no_mangle]
pub extern "C" fn gex_closure_owner_at(page: u64, k: u32) -> i32 {
    if page * gex_abi::DISPATCH_PAGE < gex_abi::INTERP_BASE {
        let claims = IMAGE_PAGES.0.borrow();
        let Some(idxs) = claims.get(&page) else {
            return -1;
        };
        return with_host(|h| {
            idxs.iter()
                .filter(|i| image_is_ours(h, **i))
                .nth(k as usize)
                .map(|i| *i as i32)
                .unwrap_or(-1)
        });
    }
    // Above the interpreter a page number is unique to an object, so there is
    // never a second owner to ask.
    if k != 0 {
        return -1;
    }
    CLOSURE_OWNER
        .0
        .borrow()
        .get(&page)
        .map(|v| *v as i32)
        .unwrap_or(-1)
}

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

/// Directories bundled by the page. See `gex_add_dir`: an empty one has no
/// file to carry it and would otherwise not exist in the guest at all.
static DIRS: SingleThreaded<Vec<String>> = SingleThreaded(const { RefCell::new(Vec::new()) });

/// The environment the page wants pid 1 to start with, if it says. Empty
/// means the default below in `gex_load`.
static ENV: SingleThreaded<Vec<String>> = SingleThreaded(const { RefCell::new(Vec::new()) });

/// Add one `KEY=VALUE` to pid 1's environment. Said before `gex_load`.
#[no_mangle]
pub extern "C" fn gex_add_env(ptr: u32, len: u32) {
    if let Ok(s) = std::str::from_utf8(shim_slice(ptr, len)) {
        ENV.0.borrow_mut().push(s.to_string());
    }
}

/// Modes for bundled files that are not the 0644 default. See `gex_chmod`.
static MODES: SingleThreaded<Vec<(String, u32)>> =
    SingleThreaded(const { RefCell::new(Vec::new()) });

/// Put the staged bundle into a filesystem, creating the directories on the
/// way to each file.
fn install_bundle(root: &gex_kernel::vfs::TmpFs) {
    // The empty ones first, so a file arriving under one of them finds it
    // rather than making it again.
    for name in std::mem::take(&mut *DIRS.0.borrow_mut()) {
        let mut at = String::new();
        for part in name.trim_start_matches('/').split('/').filter(|p| !p.is_empty()) {
            at.push('/');
            at.push_str(part);
            root.add_dir(&at);
        }
    }
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
    // And the modes, after the files exist to carry them.
    for (name, mode) in std::mem::take(&mut *MODES.0.borrow_mut()) {
        let _ = gex_kernel::vfs::FileSystem::chmod(root, &name, mode);
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
/// The program each closure library is the image of, by registration order.
/// Empty for a shared library, which is every object that is not a root.
static LIB_PROGRAM: SingleThreaded<Vec<String>> =
    SingleThreaded(const { RefCell::new(Vec::new()) });

/// Which image libraries claim each page below `INTERP_BASE`.
///
/// Every root of a closure is an image and every image sits at `PIE_BASE`, so
/// they all claim the same pages. Which one a process may have is decided by
/// the program it is running, not by the page.
static IMAGE_PAGES: SingleThreaded<std::collections::BTreeMap<u64, Vec<usize>>> =
    SingleThreaded(const { RefCell::new(std::collections::BTreeMap::new()) });

/// The program whose image this process is running, from the identity the
/// loader recorded for the mapping at `PIE_BASE`.
fn current_image_program(h: &Host) -> Option<String> {
    h.kernel
        .mem
        .vmas()
        .iter()
        .find(|v| v.start <= gex_abi::PIE_BASE && gex_abi::PIE_BASE < v.end)
        .and_then(|v| v.code.as_ref().map(|(p, _)| p.clone()))
}

/// Say everything this process knows about one image page.
///
/// The chain behind a page with no dispatch base is `#pageBase` returning 0
/// because `gex_page_table` returned -1 because `IMAGE_TABLE` has no
/// `(owner, page)` -- while `IMAGE_PAGES` does have the page, or the owner
/// would not have resolved. Both maps are written from the same list in the
/// same call, so the two must be disagreeing about *which* index owns the
/// page. This prints the candidates and what each answers. See #136.
#[no_mangle]
pub extern "C" fn gex_explain_page(page: u64) {
    let claims = IMAGE_PAGES.0.borrow();
    let idxs = claims.get(&page).cloned().unwrap_or_default();
    let tables = IMAGE_TABLE.0.borrow();
    let progs = LIB_PROGRAM.0.borrow();
    with_host(|h| {
        let have = current_image_program(h);
        let mut s = format!(
            "[gex] page {page}: running {:?}, {} candidate(s):",
            have.as_deref().unwrap_or("<none>"),
            idxs.len()
        );
        for i in &idxs {
            s.push_str(&format!(
                " [{i} {:?} ours={} table={:?}]",
                progs.get(*i).map(|p| p.as_str()).unwrap_or("<unnamed>"),
                image_is_ours(h, *i),
                tables.get(&(*i, page)),
            ));
        }
        s.push('\n');
        warn_line(&s);
    });
}

/// Whether image library `idx` is the one this process is running.
fn image_is_ours(h: &Host, idx: usize) -> bool {
    let progs = LIB_PROGRAM.0.borrow();
    let Some(want) = progs.get(idx).filter(|p| !p.is_empty()) else {
        return false;
    };
    let have = current_image_program(h);
    // Once per process, what the two sides actually think.
    //
    // Closure roots translate their own images -- `http` lifted 2,067 blocks
    // of an image the closure contains -- and the question is whether this
    // comparison ever has two names to compare. `Vma::code` is derived from
    // `origin` in `insert_from`, and its own documentation says the two must
    // stay separate because an image segment has no `origin`; if that is what
    // happens here, `have` is `None` and the match below is not a match at
    // all, it is the permissive branch.
    {
        static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if tracing() && !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
            warn_line(&format!(
                "[gex] image match: this process is running {:?}, the closure \
                 offers {:?} for library {idx}\n",
                have.as_deref().unwrap_or("<no code identity on the PIE_BASE mapping>"),
                want,
            ));
        }
    }
    match have {
        Some(have) => have == *want,
        // An identity this side does not know is not evidence of anything;
        // see the note on `image_pages_are_ours`.
        None => true,
    }
}

#[no_mangle]
pub extern "C" fn gex_add_closure_module(ptr: u32, len: u32, prog: u32, prog_len: u32) -> u32 {
    let Some(meta) = gex_xlate::Metadata::decode(shim_slice(ptr, len)) else {
        return 0;
    };
    // From here this process holds a module that can fill pages, so the miss
    // path may spend a trap asking.
    //
    // This used to be armed only by `#installModule`, when a module with
    // passive segments was *instantiated* -- and a closure library is
    // deliberately never instantiated until a fill asks for it. Nothing
    // instantiated, so nothing armed; nothing armed, so no miss ever asked;
    // no miss ever asked, so nothing was instantiated. Every branch into
    // `libc` and `ld.so` fell straight through to the translator, and the
    // closure that covers them whole -- pinned to the right address, its
    // pages in the directory, its blocks in the module -- did nothing at all.
    // pid 1 lifted 10,239 blocks of libc and 6,141 of ld.so with both of them
    // sitting statically recompiled in the process.
    //
    // Registration is the honest moment: a page this module claims can be
    // filled from now on, whether or not the instance exists yet, because
    // `fillSlot` instantiates it on the way.
    FILL_POSSIBLE.store(true, std::sync::atomic::Ordering::Relaxed);
    {
        let idx = CLOSURE_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as usize;
        let program = std::str::from_utf8(shim_slice(prog, prog_len))
            .unwrap_or("")
            .to_string();
        {
            let mut progs = LIB_PROGRAM.0.borrow_mut();
            if progs.len() <= idx {
                progs.resize(idx + 1, String::new());
            }
            progs[idx] = program.clone();
        }
        let mut owner = CLOSURE_OWNER.0.borrow_mut();
        let mut images = IMAGE_PAGES.0.borrow_mut();
        // Only a module that writes its slots at instantiation sets the floor.
        //
        // The floor exists so a claim cannot land on a slot an image's active
        // element segment is about to write at the number the build chose. A
        // demand-filled image writes nothing at instantiation, so it fixes
        // nothing in place and needs no floor -- and charging one anyway is
        // what made a warm process grow table 0 to 3,730,432 slots before the
        // guest ran an instruction. `gexc` numbers slots in one sequence
        // across all sixty-six objects of the closure, so `bash`'s image
        // pages sit almost four million in; the table had to be that long to
        // hold them, and `Table.grow` reallocates and copies, which came to
        // 790ms of an 815ms install. Per-page claiming already covered every
        // library. The image was the one thing still numbered by the build.
        // Only when this process is *not* numbering its own pages. Every
        // object of a closure is built demand-filled now, whatever its block
        // count, so nothing in one writes absolute slots at instantiation and
        // nothing needs a floor. See `Options::demand_fill`.
        if !claim_pages() {
            let mut high = IMAGE_HIGH.0.borrow_mut();
            for (page, base) in &meta.dir_entries {
                if page * gex_abi::DISPATCH_PAGE < gex_abi::INTERP_BASE {
                    let t = gex_abi::dispatch_entry_table(*base);
                    let end = (*base & gex_abi::DISPATCH_SLOT_MASK)
                        + gex_abi::SLOTS_PER_PAGE as u32;
                    if let Some(h) = high.get_mut(t) {
                        *h = (*h).max(end);
                    }
                }
            }
        }
        for (page, _) in &meta.dir_entries {
            // An image's pages are claimed by program, not by page: every
            // root of the closure is at `PIE_BASE` and they all claim these.
            if page * gex_abi::DISPATCH_PAGE < gex_abi::INTERP_BASE {
                images.entry(*page).or_default().push(idx);
            } else {
                owner.insert(*page, idx as u32);
            }
        }
        // The table `gexc` chose for each page, kept so a claim can put the
        // page back in it.
        //
        // Only the table survives the move to per-process numbering:
        // `table.init` takes it as an immediate, so a page cannot leave the
        // table it was built for, while its offset within that table is this
        // process's to choose.
        //
        // Demand-filled libraries only. A module small enough to keep active
        // element segments writes its slots at instantiation, at the numbers
        // the build chose, so there is nothing left to claim and its table
        // has to be that long before it is instantiated.
        if claim_pages() {
            let mut tables = CLOSURE_TABLE.0.borrow_mut();
            let mut image_tables = IMAGE_TABLE.0.borrow_mut();
            for (page, base) in &meta.dir_entries {
                let t = gex_abi::dispatch_entry_table(*base) as u32;
                if page * gex_abi::DISPATCH_PAGE >= gex_abi::INTERP_BASE {
                    tables.insert(*page, t);
                } else {
                    // Keyed by module too. Every root of the closure is an
                    // image at `PIE_BASE`, so several modules name the same
                    // page and only one of them is this process's; which one
                    // is `image_is_ours`, the same question `gex_closure_owner`
                    // already answers for the fill itself.
                    image_tables.insert((idx, *page), t);
                }
            }
            LIB_LAZY.0.borrow_mut().insert(idx as u32);
        }
    }
    // Registered, not published. See `gex_publish_closure_module`.
    0
}

/// Put a registered closure library's pages into the dispatch directory.
///
/// Split from registration because publishing is what costs. A directory
/// entry names a slot, and a branch to a slot past the end of a table traps
/// rather than missing -- so an entry written is a table that has to be grown
/// to hold it. Publishing all thirty-three libraries at once meant every
/// process grew its tables to the whole closure's 2,850,816 slots before the
/// guest ran an instruction: 405ms in pid 1, 524ms in a forked child, and
/// 22 MB of table each. An `apt-get update` starts a hundred processes and
/// enters a handful of libraries in each.
///
/// Unpublished is not unreachable. The directory entry stays zero, which
/// points at the miss span that table 0 reserves for exactly this, so a
/// branch into an unpublished page reaches the miss stub, asks for a fill,
/// and `fillSlot` publishes, grows, instantiates and fills before resuming.
/// That path already existed for the instantiate; this adds the entry and the
/// growth to it.
/// The program the closure's image module was built from, as the manifest
/// names it. Empty when the closure has no image.
static IMAGE_PROGRAM: SingleThreaded<String> =
    SingleThreaded(const { RefCell::new(String::new()) });

#[no_mangle]
pub extern "C" fn gex_set_image_program(ptr: u32, len: u32) {
    if let Ok(s) = std::str::from_utf8(shim_slice(ptr, len)) {
        *IMAGE_PROGRAM.0.borrow_mut() = s.to_string();
    }
}

/// Whether this process is running the program the image module was built
/// from, and may therefore be given its pages.
///
/// An image sits at `PIE_BASE` and every program lands there, so the image
/// library's pages are the one part of a closure that is *not* true whatever
/// the process goes on to run. `execve` was taught to drop them; publishing
/// them was not, and publishing happens on the first branch into a claimed
/// page -- which for a process running `apt-get`, also at `PIE_BASE`, points
/// apt-get's pages at bash's translated blocks. It then runs a few blocks of
/// somebody else's code and dies at whatever syscall it reaches, which is why
/// the fault moved from `openat` to `close` when the workload changed and why
/// it vanishes entirely with `?closure=0`.
///
/// Asked of the image's own code identity rather than tracked as a flag,
/// because a forked child that has not exec'd yet *is* still running its
/// parent's program and the answer has to stay true across the fork.
///
/// Refused only on a *positive* mismatch. An identity this side does not know
/// -- a child whose adopted map did not carry one, a program loaded before
/// the manifest said anything -- is not evidence of anything, and treating it
/// as one is worse than the bug: the first attempt returned `false` whenever
/// the identity was missing, and a forked shell was refused its own pages and
/// stopped making progress at all.
fn image_pages_are_ours(h: &Host) -> bool {
    let want = IMAGE_PROGRAM.0.borrow();
    if want.is_empty() {
        return true;
    }
    match h
        .kernel
        .mem
        .vmas()
        .iter()
        .find(|v| v.start <= gex_abi::PIE_BASE && gex_abi::PIE_BASE < v.end)
        .and_then(|v| v.code.as_ref())
    {
        Some((p, _)) => p.as_str() == want.as_str(),
        None => true,
    }
}

#[no_mangle]
pub extern "C" fn gex_publish_closure_module(ptr: u32, len: u32, idx: u32) -> u32 {
    let Some(meta) = gex_xlate::Metadata::decode(shim_slice(ptr, len)) else {
        return 0;
    };
    // Kept, because `execve` republishes them: the modules are still in this
    // instance's tables, so the new program gets every library the old one
    // reached for nothing.
    with_host(|h| {
        // Not the image's pages, unless this process is running *this*
        // image's program. Every root of the closure is an image and they all
        // sit at `PIE_BASE`, so the question is which one, not whether.
        let ours = image_is_ours(h, idx as usize);
        let mut gm = GuestMem::new(guest_bytes());
        for (page, base) in &meta.dir_entries {
            if page * gex_abi::DISPATCH_PAGE < gex_abi::INTERP_BASE && !ours {
                continue;
            }
            // A library's pages are not written here any more.
            //
            // `gexc` numbers slots in one sequence across the whole closure,
            // so writing a library's entries means growing the table to the
            // highest slot that library names -- 2.7 million for a process
            // that runs `cat /dev/null`, and `Table.grow` writes the miss stub
            // into every one at five million a second. Measured: 1087ms of a
            // 1499ms process, all of it inside the engine.
            //
            // Instead the entry stays zero, which is the miss stub, until a
            // branch actually lands in the page. `gex_claim_page` then gives
            // it a base from this process's own numbering.
            //
            // Images too, and that was the last thing still numbered by the
            // build. An image only has to sit where the build put it if its
            // element segments are active, because those write absolute slots
            // at instantiation -- which is exactly what `n_blocks` decides. A
            // demand-filled image writes nothing until a fill asks, so its
            // pages can be claimed like any library's. Excluding them cost
            // every process two grows of 3.73 million slots: `bash` is object
            // 34 of 66 in one global numbering, `assign` took the high-water
            // from the build's entry, and `Table.grow` reallocates and copies
            // -- 790ms of an 815ms install, unchanged by anything else tried.
            if claim_pages() {
                continue;
            }
            CLOSURE_DIRS.0.borrow_mut().push((*page, *base));
            h.jit.page_table.assign(*page, *base);
            // And the guest's own directory, which is what a branch reads.
            // The image module publishes its own as a data segment; a
            // `Shared` module has no memory of its own to put one in, so the
            // host writes them.
            dir_note(*page, *base, "pin-meta");
            let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, *base);
        }
    });
    1
}

/// The first slot in each table that no image can write.
///
/// An image's element segment is active and absolute: instantiating it writes
/// its blocks at the slots `gexc` chose, whenever that happens. A claim that
/// started at `table.length` handed out slots below that, and the next
/// `exec` wrote its image over them -- `debian-fs` mode=1, `debian-deb` xz=1
/// big=1 real=1, every one of them a branch reaching the wrong function.
///
/// Every image in the closure is registered before any guest runs, so the
/// highest slot any of them can reach is known by then. Claims start above
/// it.
static IMAGE_HIGH: SingleThreaded<[u32; gex_abi::DISPATCH_TABLES]> =
    SingleThreaded(std::cell::RefCell::new([0; gex_abi::DISPATCH_TABLES]));

/// Where this process may start handing out slots in table `i`.
#[no_mangle]
pub extern "C" fn gex_claim_floor(i: u32) -> u32 {
    IMAGE_HIGH.0.borrow().get(i as usize).copied().unwrap_or(0)
}

/// Whether pages are numbered by this process or by the build.
///
/// Off by default. See `gex_set_claim_pages`.
static CLAIM_PAGES: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// Turn per-process page numbering on.
///
/// It exists because the build numbers dispatch slots in one sequence across
/// the whole closure, so a process that branches into a late-numbered library
/// grows its table to that length whatever it uses -- 2.7 million slots for
/// `cat /dev/null`, about a second of `Table.grow`. Claiming a page at a time
/// takes that to 52,224.
///
/// It is off because it is not finished: a module reused from an earlier
/// process carries slot numbers from *that* process's numbering, and nothing
/// reconciles the two. The second `ls` in a shell dies instantiating one.
#[no_mangle]
pub extern "C" fn gex_set_claim_pages(on: i32) {
    CLAIM_PAGES.store(on != 0, std::sync::atomic::Ordering::Relaxed);
}

fn claim_pages() -> bool {
    CLAIM_PAGES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Libraries whose slots are claimed a page at a time.
///
/// The big ones, which `gexc` gives passive element segments and a fill
/// helper. A small one writes its slots at instantiation and has to keep the
/// numbering it was built with.
static LIB_LAZY: SingleThreaded<std::collections::BTreeSet<u32>> =
    SingleThreaded(std::cell::RefCell::new(std::collections::BTreeSet::new()));

/// Whether library `idx` is claimed page by page, for a host deciding how
/// long its table must be before instantiating it.
#[no_mangle]
pub extern "C" fn gex_lib_lazy(idx: u32) -> i32 {
    LIB_LAZY.0.borrow().contains(&idx) as i32
}

/// Which dispatch table `gexc` built each closure page for.
static CLOSURE_TABLE: SingleThreaded<std::collections::BTreeMap<u64, u32>> =
    SingleThreaded(std::cell::RefCell::new(std::collections::BTreeMap::new()));

/// The table a page has to be claimed in, or -1 if this page is not a
/// closure library's.
#[no_mangle]
pub extern "C" fn gex_page_table(page: u64) -> i32 {
    if !claim_pages() {
        return -1;
    }
    // An image page belongs to whichever root of the closure this process is
    // actually running, so it is looked up by module and not by page alone.
    if page * gex_abi::DISPATCH_PAGE < gex_abi::INTERP_BASE {
        let owner = gex_closure_owner(page);
        if owner < 0 {
            return -1;
        }
        return IMAGE_TABLE
            .0
            .borrow()
            .get(&(owner as usize, page))
            .map(|t| *t as i32)
            .unwrap_or(-1);
    }
    CLOSURE_TABLE
        .0
        .borrow()
        .get(&page)
        .map(|t| *t as i32)
        .unwrap_or(-1)
}

/// Which dispatch table `gexc` built each image page for, per image.
///
/// Separate from `CLOSURE_TABLE` because image pages collide: every root of
/// the closure sits at `PIE_BASE` and names the same pages.
static IMAGE_TABLE: SingleThreaded<std::collections::BTreeMap<(usize, u64), u32>> =
    SingleThreaded(std::cell::RefCell::new(std::collections::BTreeMap::new()));

/// Give a page a base in this process's numbering and write its entry.
///
/// The caller has already grown the table, so `base` is a slot span nothing
/// else holds. Returns the entry written.
#[no_mangle]
pub extern "C" fn gex_claim_page(page: u64, base: u32) -> u32 {
    let table = match gex_page_table(page) {
        t if t >= 0 => t as u32,
        _ => return 0,
    };
    let entry = gex_abi::dispatch_entry(table as usize, base);
    dir_note(page, entry, "claim-page");
    with_host(|h| {
        h.jit.page_table.assign(page, entry);
        let mut gm = GuestMem::new(guest_bytes());
        let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, entry);
    });
    CLOSURE_DIRS.0.borrow_mut().push((page, entry));
    entry
}

/// The slot a page's dispatch entry points at, in this process.
///
/// What `gexfillat` has to be told. A module's segments are numbered as
/// `gexc` laid the closure out, in one sequence across sixty-six libraries,
/// and a process that enters a late-numbered one has to grow its table to
/// that length -- 2.7 million slots for `cat /dev/null`, filled with the miss
/// stub at four million a second. The directory is per process, so the
/// numbering can be too; a fill writes where the directory says rather than
/// where the build guessed.
/// The dispatch page whose directory writes should be narrated, or -1.
static DIR_WATCH: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);

#[no_mangle]
pub extern "C" fn gex_watch_dir(page: i64) {
    DIR_WATCH.store(page, std::sync::atomic::Ordering::Relaxed);
}

/// Say who is about to change a watched page's directory entry.
///
/// A page filled at one base is later dispatched through another -- measured,
/// `1085580 was[at=188416] now[at=302080]` -- and the slots written at the
/// first base are then unreachable. `gex_claim_page` only allocates when the
/// entry reads zero, so one of the other writers is overwriting a live entry
/// or clearing it. This names which. See #136.
fn dir_note(page: u64, entry: u32, who: &str) {
    let want = DIR_WATCH.load(std::sync::atomic::Ordering::Relaxed);
    if want < 0 || want as u64 != page {
        return;
    }
    let gm = GuestMem::new(guest_bytes());
    let had = gm.read_u32(gex_abi::DISPATCH_DIR + page * 4).unwrap_or(0);
    if had != entry {
        // With the pid: three claims of three different bases for one page
        // are either one process losing its numbering over and over, or
        // three processes numbering independently and a fill landing in the
        // wrong one. `my_pid` reads a static, which matters -- this is
        // called from inside `with_host` closures and must not borrow.
        warn_line(&format!(
            "[gex] pid {} dir page {page}: {:#x} -> {:#x} by {who}\n",
            my_pid(),
            had & gex_abi::DISPATCH_SLOT_MASK,
            entry & gex_abi::DISPATCH_SLOT_MASK,
        ));
    }
}

#[no_mangle]
pub extern "C" fn gex_watch_dir_marker() {}

#[no_mangle]
pub extern "C" fn gex_page_base(page: u32) -> u32 {
    let gm = GuestMem::new(guest_bytes());
    gm.read_u32(gex_abi::DISPATCH_DIR + page as u64 * 4)
        .unwrap_or(0)
        & gex_abi::DISPATCH_SLOT_MASK
}

/// How long dispatch table `i` has to be for what this process has published.
///
/// One running total across all eight tables cannot answer this any more.
/// Closures own tables 0..3 with their slot numbers baked in at build time
/// and the run translates into tables 4..7, so a total of "table 4's base
/// plus twelve" would grow tables 0..3 to ten million entries each.
#[no_mangle]
pub extern "C" fn gex_table_need(i: u32) -> u32 {
    with_host(|h| h.jit.page_table.table_need(i as usize) as u32)
}

/// Register a module a forked child inherited, without instantiating it.
///
/// The same shape as `gex_add_closure_module` and deliberately *not* the same
/// list. A closure library covers the object it was built from whatever the
/// process goes on to run, which is why `execve` keeps its pages; a unit the
/// runtime translator produced covers whatever the old program had mapped
/// there, and keeping those across an exec would point the directory at code
/// belonging to a program that is gone.
///
/// This exists because a fork hands the child its parent's whole history --
/// about 485 modules in an `apt-get update`, 19 seconds of instantiate across
/// twenty-six processes -- and the child branches into almost none of it. The
/// directory entries have to be published now, since the child's inherited
/// directory already names these pages; the *instance* can wait for a branch.
#[no_mangle]
pub extern "C" fn gex_add_lazy_module(ptr: u32, len: u32) -> u32 {
    let Some(meta) = gex_xlate::Metadata::decode(shim_slice(ptr, len)) else {
        return 0;
    };
    // And the same for a module inherited from a parent: it is registered
    // now and instantiated on the branch that wants it, so the miss path has
    // somewhere to ask from this moment on.
    FILL_POSSIBLE.store(true, std::sync::atomic::Ordering::Relaxed);
    let idx = LAZY_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    with_host(|h| {
        let mut gm = GuestMem::new(guest_bytes());
        let mut owner = LAZY_OWNER.0.borrow_mut();
        for (page, base) in &meta.dir_entries {
            h.jit.page_table.assign(*page, *base);
            dir_note(*page, *base, "register-module");
            let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, *base);
            owner.insert(*page, idx);
        }
        h.jit.page_table.advance_to(meta.next_slot);
        h.jit.page_table.slots() as u32
    })
}

/// The inherited module covering this page, or -1.
#[no_mangle]
pub extern "C" fn gex_lazy_owner(page: u64) -> i32 {
    LAZY_OWNER
        .0
        .borrow()
        .get(&page)
        .map(|v| *v as i32)
        .unwrap_or(-1)
}

/// Throw away a forked child's inherited dispatch directory and put back
/// only the entries it can honour.
///
/// A child that defers installing its parent's modules must not claim its
/// parent's slots. The directory arrives with the copied memory naming one
/// for every page the parent had translated, and a child that installed none
/// of them has filled none of those slots -- so every branch is a bet on
/// what the slot happens to hold, and the run dies later in an unrelated
/// syscall with a corrupted pointer. Moving the bulk install earlier or
/// later does not remove the bet; only forgetting the claims does.
///
/// Zeroing alone is not enough, and the reason is worth stating: the guest
/// module publishes its *own* entries from a data segment, and in the shared
/// build those segments are passive and guarded by an atomic that the copied
/// memory already has set. So the child's module will never re-publish them.
/// Erase them and the child cannot execute its first instruction -- which is
/// exactly what happened: the shell forked, the child printed nothing, and
/// the parent sat in `wait4` until the harness gave up.
///
/// So: erase everything, then write back what the module's own metadata says
/// this image covers. What remains is a directory that claims only what this
/// process can dispatch to, which is the invariant deferring needs.
#[no_mangle]
pub extern "C" fn gex_reset_directory() -> u32 {
    let meta = gex_xlate::Metadata::decode(&METADATA.0.borrow()).unwrap_or_default();
    let data = guest_bytes();
    let from = gex_abi::DISPATCH_DIR as usize;
    let to = LOW_END as usize;
    data[from..to].fill(0);
    let mut gm = GuestMem::new(guest_bytes());
    let mut n = 0u32;
    for (page, base) in &meta.dir_entries {
        dir_note(*page, *base, "restore-dirs");
        if gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, *base).is_ok() {
            n += 1;
        }
    }
    n
}

/// A dispatch slot the host is being asked to write, or -1.
///
/// A module's element segments are passive, so its blocks reach the table
/// only when somebody calls `gexfillat`. When a branch misses on a page this
/// process already has a base for, the code exists in a module this process
/// holds and simply has not been written down: the right answer is to ask for
/// that slot, not to serve the object again or lift the block a second time.
static FILL_WANTED: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(-1);

/// Pages already asked about, so one nobody can fill is asked once and then
/// falls through to the ordinary path. Without this a page whose base came
/// from a module this process does not hold would ask for ever.
static FILL_ASKED: SingleThreaded<std::collections::BTreeSet<u64>> =
    SingleThreaded(const { RefCell::new(std::collections::BTreeSet::new()) });

/// Whether the host is holding any module unwritten.
///
/// Without this the miss path asks for a fill on the first miss of every
/// page whose base it already has -- a trap and a round trip into JavaScript
/// -- and on the runtime path there is never anything to fill, because those
/// units are small and written the moment they are installed. It cost
/// `apt-get update` 114 seconds against 82. The host sets it when it first
/// defers a module, so a run with nothing deferred behaves exactly as it did.
static FILL_POSSIBLE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Pages asked about that nobody could fill, and the page being asked now.
///
/// A failure is not permanent. The module covering a page may simply not be
/// installed *in this process* yet -- which is the ordinary case for a forked
/// child, since it installs what it inherited one module at a time while the
/// guest is already running. Recording the ask as final meant such a page was
/// asked once, found nowhere, and then had every one of its blocks lifted
/// instead: 132,000 blocks a process against 234 fills.
static FILL_FAILED: SingleThreaded<std::collections::BTreeSet<u64>> =
    SingleThreaded(const { RefCell::new(std::collections::BTreeSet::new()) });

/// The page the host is being asked about, so the answer can be attributed.
static FILL_PENDING: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(-1);

/// The host is holding a module whose slots are not written yet.
///
/// Turning this on also forgives every page that could not be filled before,
/// because the module just installed may be the one that covers them.
#[no_mangle]
pub extern "C" fn gex_fill_possible(on: i32) {
    FILL_POSSIBLE.store(on != 0, std::sync::atomic::Ordering::Relaxed);
    if on != 0 {
        let failed = std::mem::take(&mut *FILL_FAILED.0.borrow_mut());
        let mut asked = FILL_ASKED.0.borrow_mut();
        let spans = gex_abi::SLOTS_PER_PAGE as u64
            / FILL_WIDTH.load(std::sync::atomic::Ordering::Relaxed) as u64;
        for page in failed {
            // Every span of it: the module that has just arrived may cover
            // parts of the page this one could not.
            for i in 0..spans {
                asked.remove(&(page * spans + i));
            }
        }
    }
}

/// How far into the page the fill should start, in slots.
///
/// A fill writes `FILL_SPAN` slots from here rather than the whole page. See
/// `gex_abi::FILL_SPAN` for why.
static FILL_FROM: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The address that missed, for the fill the host is about to run.
///
/// `FILL_FROM` is that address rounded down to a span, which is what the
/// module needs but not what the *guest* needs: the question that matters is
/// whether the one slot this pc dispatches through comes back written, and a
/// span read-back cannot answer it. Every miss that reaches the translator is
/// one whose span was already filled -- `why[asked=379]` in #136 -- so the
/// fill is writing something the guest does not read, and this is how to see
/// which slot.
static FILL_PC: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[no_mangle]
pub extern "C" fn gex_fill_pc() -> u64 {
    FILL_PC.load(std::sync::atomic::Ordering::Relaxed)
}

/// Slots a fill is expected to write, as the host found them.
///
/// `FILL_SPAN` normally, and a whole page when the modules in this run are
/// older than the span. A closure built before `gexfillat` took a `from`
/// ignores the argument and writes all 1,024 slots, so asking once per span
/// would ask eight times for a page and write the whole thing each time --
/// eight times the work the change was meant to save. The host can see the
/// arity of the export and says so; this keeps a new shim usable against a
/// closure somebody has not rebuilt.
static FILL_WIDTH: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(gex_abi::FILL_SPAN);

#[no_mangle]
pub extern "C" fn gex_set_fill_width(n: u32) {
    let n = if n == 0 { gex_abi::FILL_SPAN } else { n };
    FILL_WIDTH.store(n.min(gex_abi::SLOTS_PER_PAGE as u32), std::sync::atomic::Ordering::Relaxed);
}

#[no_mangle]
pub extern "C" fn gex_fill_from() -> u32 {
    FILL_FROM.load(std::sync::atomic::Ordering::Relaxed)
}

/// The slot the host should write, or -1 if it is not being asked.
#[no_mangle]
pub extern "C" fn gex_fill_wanted() -> i64 {
    FILL_WANTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The host has answered, and whether it found anybody.
///
/// A page nobody could fill is remembered as such, so the next miss on it
/// takes the ordinary path rather than trapping again -- but only until the
/// next deferred module arrives, which may be the one that covers it. A page
/// that *was* filled stays in `FILL_ASKED` and is never asked twice; if a
/// branch on it misses again the fill did not cover that block, and asking
/// once more would only spin.
#[no_mangle]
pub extern "C" fn gex_fill_done(ok: i32) {
    let page = FILL_PENDING.swap(-1, std::sync::atomic::Ordering::Relaxed);
    if ok == 0 && page >= 0 {
        FILL_FAILED.0.borrow_mut().insert(page as u64);
    }
    FILL_WANTED.store(-1, std::sync::atomic::Ordering::Relaxed);
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

/// Whether a pending signal makes a parked thread runnable. See
/// `thread_ready`.
///
/// A constant beside `SAFEPOINT_SIGNALS` and for the same reason: it is on
/// the path everything blocking goes through, so "did this cause it" is a
/// question worth being able to answer in one build rather than one argument.
/// Measured, not assumed: nested command substitution wedges 3 runs of 3
/// with this off, exactly as it does with it on. The wedge is not this.
const SIGNAL_ENDS_WAIT: bool = true;

/// Whether a safepoint delivers signals. See `gex_safepoint`.
///
/// A constant rather than a switch because it exists to be flipped while
/// bisecting: this is the newest thing on the hottest path in the runtime,
/// and "is the crash in here" has no cheaper answer than building it both
/// ways. It has been asked once already and the answer was no -- a forked
/// child trapped at the same rate with this off.
const SAFEPOINT_SIGNALS: bool = true;

/// The global signal counter as this Worker last saw it at a safepoint.
///
/// See the two-step check in `gex_safepoint`. Per Worker, and a plain static
/// rather than a thread-local because a Worker is one thread of one process
/// and the cost of asking has to be a single load.
static SIGNAL_SEEN: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

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
///
/// It was then two milliseconds, which is not a wait at all: nothing that
/// reaches here sleeps, it wakes and re-parks five hundred times a second.
/// `apt-get update` spent 34 seconds of a 115 second run in `k_park` doing
/// exactly that, and 40 seconds of system time on the futex traffic.
///
/// A hundred is affordable now because a wait only reaches this bound if it
/// was misjudged, and the misjudgement that used to be common -- calling a
/// poll over pipes a page wait -- is gone: the only thing that arrives as a
/// message to a guest's own Worker is a Wisp socket, and a process with a
/// network is excluded above. So this is a backstop against a wake that was
/// never sent, not a polling interval.
const PARK_HERE_NS: u64 = 100_000_000;

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
        // Standard input, which is now a queue in the kernel rather than one
        // in this Worker's own shim. Whoever has the keystroke writes it and
        // wakes; nothing arrives by a message this Worker has to be awake to
        // receive. Compared by address, as `is_remote` does -- the handle in
        // a parked thread is the same `Arc` the table holds, upcast, and the
        // data half of a fat pointer survives the cast.
        let stdin_p = Arc::as_ptr(&*STDIN) as *const ();
        if Arc::as_ptr(h) as *const () == stdin_p {
            return true;
        }
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
        // A deadline is the clock's to end, and the clock is the page's -- so
        // a *timed* wait still belongs to the page. An untimed one does not,
        // unless this process has a network.
        //
        // The only thing a guest can wait for that arrives as a message to
        // its own Worker is a Wisp socket: the WebSocket is constructed in
        // the guest's Worker, so its bytes land on that thread's event loop,
        // and a Worker parked inside wasm is not running it. Everything else
        // a poll can name -- a pipe, a unix socket, another process -- is
        // ended by another Worker, whose `k_wake` reaches a parked one.
        //
        // So the question is whether this process has a network at all, which
        // is a property of the process and not of the descriptors in the set.
        // Asking about the descriptors is what was tried before and it
        // stalled the gears demo; this cannot make that mistake, because it
        // does not look at them.
        //
        // What it costs when the answer is wrong is nothing: the deadline
        // still ends the wait, and a process with a network keeps exactly the
        // behaviour it had.
        //
        // Measured on `apt-get update`, which has no network: the park loop
        // was 34 seconds of a 115 second run, waking and re-parking every two
        // milliseconds because it dared not sleep longer.
        Waiting::Net(Some(_)) => true,
        Waiting::Net(None) => kernel.net.is_some(),
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
    // A Worker owns exactly one thread, so there is never a sibling whose
    // turn this would take. While threads could share a Worker this had to
    // ask, because sleeping in a syscall would take the turn from the very
    // thread that could have made the wait come true.
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
        if with_host(|h| {
            let sig = any_signal(h);
            h.threads.iter().any(|t| thread_ready(t, now, sig))
        }) {
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

/// The terminal's current `c_lflag`.
///
/// Bit 1 is ICANON and bit 3 is ECHO. A page with a global kernel does not
/// need to ask -- the discipline that obeys these is in there with them, and
/// the page reads them directly. This is for the page that has no global
/// kernel and therefore no shared terminal, and must do its own line editing
/// against whatever this process last set.
#[no_mangle]
pub extern "C" fn gex_tty_lflag() -> u32 {
    with_host(|h| u32::from_le_bytes(h.kernel.tty.termios()[12..16].try_into().unwrap()))
}

/// Tell the guest how big the terminal is, and signal whoever is using it.
///
/// A resize is `SIGWINCH` to the foreground process group and nothing else:
/// a program that has drawn a full-screen interface has no other way to
/// learn that the screen changed under it. Sent only when the size actually
/// changed -- see `Tty::set_winsize`.
#[no_mangle]
pub extern "C" fn gex_set_winsize(rows: u32, cols: u32) {
    with_host(|h| {
        let (_, _, x, y) = h.kernel.tty.winsize();
        if h.kernel.tty.set_winsize((rows as u16, cols as u16, x, y)) {
            let fg = h.kernel.tty.fg();
            let g = if fg > 0 { fg } else { h.kernel.pid };
            let _ = h
                .kernel
                .procs
                .kill_group(h.kernel.pid, g, gex_kernel::signal::sig::SIGWINCH);
        }
    });
}

/// Say that stdin is a terminal, which is what makes a shell print a prompt.
///
/// And claim it, if nobody has. On Linux a session leader that opens a
/// terminal with no owner becomes its controlling process and its group
/// becomes the foreground group -- which is the whole reason `^C` reaches
/// anything before a shell has called `tcsetpgrp`. Without this the terminal
/// has no foreground group at all for the first command of every run, and
/// that is the command someone is most likely to interrupt.
#[no_mangle]
pub extern "C" fn gex_set_tty(on: i32) {
    unsafe { STDIN_TTY = on != 0 };
    if on != 0 && console_tty().sid() == 0 {
        console_tty().set_sid(gex_kernel::INIT_PID);
    }
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

/// Something a person must see whatever the tracing settings, on the host's
/// console at error level rather than in the guest's output.
fn warn_line(s: &str) {
    unsafe { warn(s.as_ptr() as u32, s.len() as u32) };
}

/// Write a line to the host's debug console, not the guest's stderr.
fn trace_line(s: &str) {
    unsafe { debug(s.as_ptr() as u32, s.len() as u32) };
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

/// Nanoseconds spent inside syscalls, and how many there were. Always kept.
static SYS_TOTAL_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SYS_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How long this process has spent in syscalls, in microseconds.
#[no_mangle]
pub extern "C" fn gex_sys_us() -> u64 {
    SYS_TOTAL_NS.load(std::sync::atomic::Ordering::Relaxed) / 1000
}

#[no_mangle]
pub extern "C" fn gex_sys_calls() -> u64 {
    SYS_CALLS.load(std::sync::atomic::Ordering::Relaxed)
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
    // Every `HEARTBEAT` calls, which is what the setting says and did not
    // mean. The rule was "a power of two, and at least 512", and it has come
    // down twice for the same reason both times -- 65,536 that a glxgears run
    // never reached, then 4,096 that apt's http method never reached. A warm
    // `ls` makes about 130 and never reached 512 either, so the census still
    // could not speak for the processes it was lowered for. The threshold is
    // the caller's now.
    let every = unsafe { HEARTBEAT }.max(1);
    if total as u64 % every != 0 {
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
    IN_SYSCALL.store(true, std::sync::atomic::Ordering::Relaxed);
    let _in_syscall = InSyscall;
    CTX_ADDR.store(ctx as u32 as u64, std::sync::atomic::Ordering::Relaxed);
    // The process tree, always. Stood down under `GEX_TRACE`, which narrates
    // these along with everything else.
    if !tracing() {
        note_process_syscall(ctx as u32 as u64, pc);
    }
    // And whether the kernel this call is about to run against is still
    // intact. A load and a compare, once per syscall.
    check_host_magic("a syscall");
    // Whose process this is, for the calls that have to name it.
    //
    // Set here rather than at `load` and `adopt` and `execve` because every
    // one of those is a place it could be missed, and it is one store.
    MY_PID.store(
        with_host(|h| h.kernel.pid),
        std::sync::atomic::Ordering::Relaxed,
    );
    // And whether a pinned library landed where the closure built it. Same
    // shape and same reason: a load and a compare, once per syscall.
    //
    // `gex_pin_report` runs after `gex.run` returns, which in a shell demo is
    // never -- process 1 is the shell and it does not exit -- and adopted
    // children do not call it at all. So the one report that says "the module
    // was built for one address and the code runs at another" was invisible
    // in exactly the configuration the project exists for.
    report_new_pins();
    // How long each kind of syscall takes, when there is a heartbeat.
    //
    // Counts alone cannot answer the question glxgears asks: it holds a
    // steady thirty frames a second while making only a few hundred calls,
    // so whatever the frame is spending is spent *inside* one of them and
    // the census says every one is rare. A blocking call parks the Worker
    // in here, so this covers the waiting as well as the work.
    // Always, not only under the heartbeat.
    //
    // `guest` is now the majority of a warm process -- 240ms of a 330ms
    // `cat /dev/null` -- and the two things inside it are executing
    // translated code and servicing syscalls. Thirty-four calls a process is
    // far too few for a clock read each to matter, and without this the
    // split is a guess. The per-number census above stays behind the
    // heartbeat, which is where the cost would be.
    let began = now_ns();
    SYS_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        // The dynamic linker has finished and jumped to what it was told was
        // the entry, which is gex's stub. Keep what it built and carry on at
        // the program's real entry, with every register but `x8` as the
        // linker left it -- nothing writes a return value for this one.
        if nr == SNAP_SYSCALL && SNAP_PENDING.0.borrow().1 != 0 {
            let (key, entry) = *SNAP_PENDING.0.borrow();
            *SNAP_PENDING.0.borrow_mut() = (0, 0);
            if key != 0 && snapshots_stored() {
                let blob = snap_take(h, kstate.ctx_addr, entry);
                // SAFETY: the pointer names this module's memory, which is
                // what the kernel copies across from, and the slice outlives
                // the call.
                if snapshots_shared() {
                    let n = blob.len().min(snapshot_share_cap()) as u32;
                    unsafe { k_snap_store(key, blob.as_ptr() as u32, n) };
                }
                link_state(3);
                // Under tracing only. `trace_line` writes to the guest's
                // stderr, which is the terminal a person is reading, and the
                // per-process line already says `link kept`.
                if tracing() {
                    trace_line(&format!(
                        "[link] {} linked, {} KiB kept at {}ms\n",
                        h.kernel.exe_path,
                        blob.len() / 1024,
                        (now_ns() - PROCESS_STARTED.load(std::sync::atomic::Ordering::Relaxed))
                            / 1_000_000
                    ));
                }
            }
            h.kernel.leave_thread(&mut h.threads[cur()].kstate);
            h.threads[cur()].pc = entry;
            set_resume(entry);
            set_resume_ctx(kstate.ctx_addr);
            return 1;
        }
        // Recorded before the call, because the kernel writes the result over
        // `x0` and a report read afterwards would show every call taking its
        // own return value as its first argument.
        // A syscall from a process that has already exited.
        //
        // `exit_process` tears the descriptor table down, so anything the
        // guest does after this point fails in ways that look like ordinary
        // bugs somewhere else -- `dpkg-deb`'s child was seen calling
        // `exit_group(0)` and then writing a diagnostic to a stderr that no
        // longer existed. Whether the guest truly runs on or the record is
        // misleading is exactly what this says.
        if EXITED.load(std::sync::atomic::Ordering::Relaxed) {
            warn_line(&format!(
                "[gex] pid {} made a {} after exiting",
                h.kernel.pid,
                gex_kernel::syscalls::syscall_name(nr),
            ));
        }
        let a0 = gm.read_u64(kstate.ctx_addr + ctx::x(0) as u64).unwrap_or(0);
        let a1 = gm.read_u64(kstate.ctx_addr + ctx::x(1) as u64).unwrap_or(0);
        let a2 = gm.read_u64(kstate.ctx_addr + ctx::x(2) as u64).unwrap_or(0);
        let slot = sys_note(nr, a0, a1);
        let outcome = h.kernel.syscall(&mut gm, pc as u64);
        let ret = gm.read_u64(kstate.ctx_addr + ctx::x(0) as u64).unwrap_or(0);
        sys_ret(slot, ret);
        if is_mem_call(nr) {
            mem_note(nr, a0, a1, ret);
        }
        // Every futex answer that is an error, by name.
        //
        // glibc calls `futex_fatal_error` -- "The futex facility returned an
        // unexpected error code." -- for anything outside the short list each
        // of its wrappers tolerates, and then aborts with no clue which call
        // or which code. The ring cannot say either, because a call that
        // blocks has not written `x0` when it is recorded. These are rare
        // enough to print whole.
        if nr == 98 {
            ftx_note(a1, a0, ret as i64);
        }
        // What the guest was trying to say when it could not say it.
        //
        // glibc reports a fatal condition by writing to fd 2 and then calling
        // `abort`, so a diagnostic that fails to arrive turns a named failure
        // into "killed by signal 6". `dpkg-deb`'s decompressing child does
        // exactly that: `writev(2, ...)` comes back EBADF in one child and 0
        // in the other, and the run is left with no idea what either of them
        // found. Only on a write to stderr that did not go through whole, so
        // a working program is silent.
        if a0 == 2 && (nr == 64 || nr == 66) {
            // (base, length) pairs: one for `write`, `a2` of them for
            // `writev`, so the two cases differ only in where the list is.
            let parts: Vec<(u64, u64)> = if nr == 64 {
                vec![(a1, a2)]
            } else {
                (0..a2.min(16))
                    .filter_map(|i| {
                        Some((
                            gm.read_u64(a1 + i * 16).ok()?,
                            gm.read_u64(a1 + i * 16 + 8).ok()?,
                        ))
                    })
                    .collect()
            };
            let want: u64 = parts.iter().map(|p| p.1).sum();
            if ret != want {
                let mut said = Vec::new();
                for (p, n) in parts {
                    if let Ok(b) = gm.read(p, n.min(200) as usize) {
                        said.extend_from_slice(b);
                    }
                }
                warn_line(&format!(
                    "[gex] pid {} could not reach stderr ({} of {want} bytes in {} parts): {}",
                    h.kernel.pid,
                    ret as i64,
                    a2,
                    String::from_utf8_lossy(&said).trim_end(),
                ));
            }
        }
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
            SyscallOutcome::Killed { signal, .. } => {
                // Loudly, and whatever the tracing settings.
                //
                // A guest that dies of a signal used to leave nothing at all
                // on the console: the process simply stopped and its parent
                // reported whatever it made of the wait status. `dpkg-deb`
                // saying "<decompress> subprocess was killed by signal
                // (Aborted)" was the only evidence that anything had happened
                // in the child, and it names neither the pid nor where it was.
                // The call it died in, by number and arguments, because
                // "killed by signal 6" is true of every `abort` and says
                // nothing about which one. A guest raises its own SIGABRT
                // through `tgkill`, and the arguments say whether it aimed at
                // itself.
                let at = gm.read_u64(kstate.ctx_addr + ctx::PC as u64).unwrap_or(0);
                warn_line(&format!(
                    "[gex] pid {} killed by signal {} at pc={at:#x}\n  threads:{}\n  \
                     last calls:{}\n  address space:{}\n  futexes:{}",
                    h.kernel.pid,
                    signal,
                    // Every thread, not only the one that died. Two threads
                    // sharing a register context would clobber each other and
                    // everything downstream of that looks like corruption
                    // somewhere else entirely -- which is the shape #126 has.
                    // Cheap to rule in or out, and never recorded before.
                    h.threads
                        .iter()
                        .enumerate()
                        .map(|(i, t)| {
                            format!(
                                " t{i}(tid={} ctx={:#x} pc={:#x} {})",
                                t.kstate.tid,
                                t.kstate.ctx_addr,
                                t.pc,
                                waiting_name(&t.waiting),
                            )
                        })
                        .collect::<String>(),
                    sys_recent(),
                    mem_recent(),
                    ftx_recent(),
                ));
                exit_process(h, -(signal as i32))
            }
            SyscallOutcome::SpawnThread {
                ctx_addr,
                tid,
                clear_child_tid,
            } => {
                // The kernel has already written the child's context; it
                // starts at the pc that context carries.
                let start = gm.read_u64(ctx_addr + ctx::PC as u64).unwrap_or(0);
                // A Worker of its own. Always: a thread is a Worker here.
                //
                // The table entry is still this process's -- `join`, the
                // futexes and `live_threads` are all counted here -- but the
                // thread runs on another stack, and `Elsewhere` is the state
                // that says "not me". Without it two Workers would run the
                // same context.
                h.threads.push(Thread {
                    kstate: ThreadState {
                        ctx_addr,
                        tid,
                        clear_child_tid,
                    },
                    pc: start,
                    waiting: Waiting::Elsewhere,
                    blocked_on: None,
                });
                stage_thread(tid, ctx_addr, start);
                // The stack goes back so the host can see the record and
                // start the Worker, exactly as a `fork` does. The parent
                // resumes where it was: `clone` returns the tid to it.
                set_resume(pc as u64 + 4);
                set_resume_ctx(h.threads[cur()].kstate.ctx_addr);
                1
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
    {
        let took = now_ns().saturating_sub(began);
        SYS_TOTAL_NS.fetch_add(took, std::sync::atomic::Ordering::Relaxed);
        if unsafe { HEARTBEAT } != 0 {
            let nr = LAST_SYSCALL.load(std::sync::atomic::Ordering::Relaxed) as usize;
            if nr < 512 {
                unsafe { SYS_NS[nr] = SYS_NS[nr].saturating_add(took) };
            }
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
    stage_fork_fds(h, pid);
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
///
/// `child` is who the reopened pipe ends belong to. They were being opened
/// under the *parent's* pid, so the global kernel's record said the parent
/// held two of each and the child none -- and when the child crashed,
/// `k_pipe_forget` had nothing to release for it: "pid 3 crashed; 0 pipe ends
/// released", with the reader still waiting on an end-of-file the dead
/// child's end was holding shut.
fn stage_fork_fds(h: &Host, child: i32) {
    let mut out = String::new();
    for (fd, file, cloexec) in h.kernel.fds.entries() {
        // A pipe end has to be opened again in the global kernel, or the
        // parent closing its copy would look like the last reference going
        // away and the reader downstream would see an end of file that has
        // not happened.
        // What the child is told to reopen. A path by default -- and an id
        // when the global kernel holds the open file, because a path is not
        // enough. `dpkg-deb` builds its tar members in files it creates and
        // unlinks at once, so a child reopening by name found nothing, the
        // descriptor went missing without a word, and the first write in the
        // compressing child was `EBADF`. An id survives an unlink.
        let mut name = file.path.clone();
        if let Some(id) = pipe_id(&file.path) {
            let write = file.writable;
            unsafe { k_pipe_open_end(id, write as i32, child) };
        } else if let Some(id) = file.handle.remote_id() {
            let dup = unsafe { k_fs_dup(id) };
            if dup < 0 {
                warn_line(&format!(
                    "gex: fork could not hand fd {fd} ({}) to pid {child}\n",
                    file.path
                ));
                continue;
            }
            name = format!("#{dup}");
        } else if !file.path.starts_with('/') {
            // Not a path, not a pipe, and not the terminal: a socket. Nothing
            // here can rebuild that from a name.
            continue;
        }
        out.push_str(&format!(
            "{} {} {} {} {}\n",
            fd,
            cloexec as i32,
            file.flags(),
            file.offset(),
            name
        ));
    }
    *FORK_FDS.0.borrow_mut() = out;
}

/// The console descriptors, rebuilt in a child from the name alone.
///
/// A fork stages each descriptor as a path and the child opens it again, and
/// that works for a file because a file is bytes on a filesystem both of them
/// can see. The terminal is not: it is a Worker's own `Console`, and the
/// three the kernel installs are named after it -- `/dev/stdin` and the rest
/// -- rather than existing anywhere to be opened.
///
/// So a child rebuilt only the descriptors it could *find*, and any other
/// descriptor pointing at the terminal was dropped on the floor. dash dups
/// its terminal to fd 10 for job control, and in the child fd 10 was not
/// there: `tcsetpgrp` returned `EBADF`, and dash treats that as fatal rather
/// than as a warning. The child died -- at exit status 2, having printed
/// nothing -- *before* reaching the `execve` it was forked to do.
///
/// Which is why no external command has ever run under a shell in the
/// browser. Every `ls` in a page was a shell that forked, died in the child,
/// and printed a job-control complaint that read like a cosmetic one.
fn console_handle(path: &str) -> Option<(Arc<dyn FileHandle>, bool, bool)> {
    Some(match path {
        "/dev/stdin" => (Arc::clone(&*STDIN) as Arc<dyn FileHandle>, true, false),
        "/dev/stdout" => (Arc::new(Console { fd: 1 }), false, true),
        "/dev/stderr" => (Arc::new(Console { fd: 2 }), false, true),
        "/dev/tty" => (Arc::new(Tty), true, true),
        _ => return None,
    })
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
            } else if let Some(id) = path.strip_prefix('#').and_then(|v| v.parse::<i32>().ok()) {
                // An id the parent already asked the global kernel to
                // duplicate, so this file is the parent's file whatever has
                // happened to its name since.
                let Some(root) = global_root() else { continue };
                let acc = flags & gex_kernel::fs::oflags::ACCMODE;
                let f = gex_kernel::file::OpenFile::new(
                    gex_kernel::vfs::remote::adopt(root.remote(), id),
                    String::new(),
                    flags,
                    acc != gex_kernel::fs::oflags::O_WRONLY,
                    acc != gex_kernel::fs::oflags::O_RDONLY,
                );
                f.set_offset(offset);
                f
            } else if let Some((handle, readable, writable)) = console_handle(path) {
                // The terminal, which is this Worker's and not a file. A dup
                // of it in the parent has to be a dup of it here.
                gex_kernel::file::OpenFile::new(handle, path.to_string(), flags, readable, writable)
            } else {
                // Through the child's *own* mounts, not the bare root: `/dev`
                // and `/proc` are mounted per process, so a descriptor on
                // `/dev/null` or `/proc/self/maps` could not be found by name
                // in the filesystem alone and went the same way as the
                // terminal.
                use gex_kernel::vfs::FileSystem;
                let handle = h.kernel.vfs.open(path, flags, 0).or_else(|e| {
                    match global_root() {
                        Some(root) => root.open(path.trim_start_matches('/'), flags, 0),
                        None => Err(e),
                    }
                });
                let handle = match handle {
                    Ok(h) => h,
                    Err(e) => {
                        // Never quietly. A descriptor that does not arrive is
                        // an `EBADF` at the child's first use of it, a long
                        // way from here and with nothing pointing back.
                        warn_line(&format!(
                            "gex: fork left fd {fd} ({path}) behind: errno {}\n",
                            e.0
                        ));
                        continue;
                    }
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
    CTX_ADDR.store(v, std::sync::atomic::Ordering::Relaxed);
    RESUME_CTX.with(|c| c.set(Some(v)));
}

/// Where the running thread's register context is, without asking the kernel.
///
/// Every question the crash reporter asks used to go through `with_host`, and
/// a crash inside a `with_host` closure leaves the kernel borrowed -- so each
/// question panicked on the leaked borrow instead of answering, and the
/// report arrived with no pc at all:
///
///     the kernel was re-entered at lib.rs:7178 while lib.rs:6321 holds it
///     RefCell already mutably borrowed
///     process 14 failed: memory access out of bounds, after syscall -1
///
/// A crash in the dispatch-miss path is exactly the case that needs a pc, and
/// exactly the case that could not produce one. A plain static can always be
/// read.
static CTX_ADDR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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

// A thread is a Worker. There is no second shape.
//
// There used to be a `THREAD_WORKERS` switch and, behind it, a scheduler that
// ran a process's threads by turns on one Worker's stack. Both are gone. The
// two shapes were chosen by whether the memory happened to be shared rather
// than by anyone deciding, which made the one real threaded program in the
// Debian rootfs impossible to compare against itself -- and the inline shape
// was the one nothing was tested on.
//
// What that costs: a build whose memory is not a `SharedArrayBuffer` can no
// longer run a `clone` that asks for a thread. It stages one and the host
// cannot start it. That is a loud failure rather than a quiet one, which is
// the trade -- see `gex_thread_stage` and the host's side of it.

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
    // Who this process is. `Kernel::new` takes 1000 and `getuid` answers with
    // it, so a child that does not say otherwise is not root -- and every
    // `dpkg` a shell or apt forks then refuses:
    //
    //   dpkg: error: requested operation requires superuser privilege
    //
    // `gex_load` has always done this and `gex_adopt_child` never did, so it
    // was invisible to anything that did not fork. `apt-get update` forks
    // eighty times and needs root for none of it.
    kernel.uid = unsafe { IDENTITY.0 };
    kernel.gid = unsafe { IDENTITY.1 };
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
    kernel.tty = global_tty();
    install_console(&mut kernel);
    kernel.pid = pid;
    kernel.tid = pid;
    PROCESS_STARTED.store(now_ns(), std::sync::atomic::Ordering::Relaxed);
    // Before anything opens a descriptor.
    //
    // A forked child rebuilds its parent's descriptor table here, *before* it
    // makes a single syscall -- and the syscall entry was the only place this
    // was set. So every inherited pipe end was recorded against pid 0, and
    // when the child crashed `k_pipe_forget` had nothing to release for it:
    // "pid 3 crashed; 0 pipe ends released".
    MY_PID.store(pid, std::sync::atomic::Ordering::Relaxed);
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
        magic_head: HOST_MAGIC,
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
            page_table: shared_page_table(&meta.dir_entries, meta.next_slot, meta.n_blocks),
            translated: Default::default(),
            registry: Vec::new(),
            shared: Default::default(),
            installed_mods: Default::default(),
            reuse_have: 0,
            reuse_hit: 0,
            reuse_miss: 0,
            reuse_noident: 0,
            reuse_served: 0,
            store_ok: 0,
            store_full: 0,
            intrinsics: Default::default(),
            intrinsics_scanned: Default::default(),
            candidates: Vec::new(),
            pending: Default::default(),
            blocks: 0,
            lift_in_closure: 0,
            by_object: Default::default(),
            compiles: 0,
            nanos: 0,
        }},
        magic_tail: HOST_MAGIC,
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
    // See `gex_exited`: this is what tells the host apart a program that
    // finished from one that abandoned the wasm stack with no status set.
    EXITED.store(true, std::sync::atomic::Ordering::Relaxed);
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
    // And anything the table did not account for. See `k_pipe_forget`: a
    // guest that leaves by a trap rather than by `exit_group` never reaches
    // the line above, so the global kernel keeps its own list.
    // SAFETY: takes a scalar and no borrow of anything here.
    unsafe {
        if k_present() != 0 {
            k_pipe_forget(pid);
        }
    }
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
    // And nowhere to resume, which has to be said rather than assumed.
    //
    // The host's loop reads `gex_resume_pc` after every trap and re-enters the
    // guest when it is set, so a resume left over from an earlier parked call
    // -- a `write` that waited for room in a pipe, say -- sends a process that
    // has just called `exit_group` back into its own code with its descriptor
    // table already emptied. `dpkg-deb`'s decompressing child did exactly
    // that: `exit_group(0)`, then six more syscalls, then glibc found a heap
    // it had already torn down, printed `free(): invalid pointer` to a stderr
    // that was now EBADF, and aborted. The parent reported "<decompress>
    // subprocess was killed by signal (Aborted)" and every `apt install` died
    // on it.
    set_resume_none();
    set_resume_ctx_none();
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
    // Has this program been linked already, in this run, in this
    // environment? Then there is nothing to load: the answer is an address
    // space, and it goes back byte for byte.
    let key = snap_key(path, envp);
    let linked = snap_lookup(key);
    let program = if linked.is_some() {
        None
    } else {
        let Some(bytes) = h.kernel.read_file(path) else {
            return 0;
        };
        let read = |p: &str| h.kernel.read_file(p);
        match gex_ld::load_program(&bytes, gex_abi::PIE_BASE, INTERP_BASE, &read) {
            Ok(p) => Some(p),
            Err(_) => return 0,
        }
    };
    // Record, when this is the first process to run the program and there is
    // a linker to catch.
    let hook = if program.as_ref().and_then(|p| p.interp.as_ref()).is_some() && snapshots_on() {
        SNAP_STUB
    } else {
        0
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
    h.kernel.mem =
        gex_kernel::mem::MemoryMap::new(program.as_ref().map(|p| p.brk()).unwrap_or(0));
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
    for (seg, from) in program.iter().flat_map(|p| p.placements()) {
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
        // No origin: an image segment is *built* by the loader rather than
        // copied from a file at an offset -- `mem_size` runs past the file's
        // bytes and the tail is zeroed -- so it cannot be reconstructed by
        // re-reading, and a snapshot has to carry its bytes.
        //
        // Saying otherwise cost a run: the identity was recorded as the
        // segment's vaddr rather than a file offset, and putting the process
        // back reported `[initial] is gone; 0x20000000 cannot be restored`
        // and left the guest executing zeroes.
        //
        // It does get a code identity, which is a different claim: this
        // program's segment at this address. Not a file offset, because
        // nothing re-reads it -- only two processes comparing what they are
        // about to translate, and for them "the same executable, the same
        // segment" is the whole question. Leaving it out is what made every
        // `ls` translate its own text again: two thirds of the misses in a
        // second run were on pages the loader itself had placed and then
        // declined to name.
        h.kernel.mem.insert_loaded(
            seg.vaddr,
            seg.vaddr + seg.mem_size,
            prot,
            "[image]",
            Some((from.unwrap_or(path).to_string(), seg.vaddr)),
        );
    }

    // The recorded address space, when there is one. It replaces the map
    // this function just built, because the map is part of what was
    // recorded.
    let restored = match &linked {
        Some(blob) => match snap_put(h, blob) {
            Some(l) => Some(l),
            // A file the recording named has gone, or the blob is from
            // another build. Nothing has been loaded, so there is nothing to
            // run: say `ENOEXEC` rather than enter a half-built image.
            None => {
                trace_line(&format!("[link] {path}: recorded image would not go back\n"));
                return 0;
            }
        },
        None => None,
    };

    // What the auxiliary vector says about the executable: from the image
    // when one was loaded, and from the recording when one was restored.
    let facts = match (&program, &restored) {
        (Some(p), _) => gex_ld::elf::headers_only(
            p.exe.phdr_vaddr,
            p.exe.phentsize,
            p.exe.phnum,
            p.exe.entry,
        ),
        (None, Some(l)) => gex_ld::elf::headers_only(l.phdr, l.phent, l.phnum, l.entry),
        (None, None) => return 0,
    };
    let interp_base = match (&program, &restored) {
        (Some(p), _) => p.interp_base(),
        (None, Some(l)) => l.interp_base,
        (None, None) => 0,
    };
    // A restored process is past the linker, so `AT_ENTRY` is the honest
    // answer and the stub is not armed.
    let hook = if restored.is_some() { 0 } else { hook };
    let layout = gex_ld::build_initial_stack(
        &facts,
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
            interp_base,
            entry_hook: hook,
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
    if DIR_WATCH.load(std::sync::atomic::Ordering::Relaxed) >= 0 {
        warn_line(&format!("[gex] pid {} dir cleared by exec\n", my_pid()));
    }
    data[gex_abi::DISPATCH_DIR as usize..LOW_END as usize].fill(0);
    // And forget which spans have been asked for, because the answers went
    // with the directory.
    //
    // `FILL_ASKED` holds a span so it is never asked twice: a second ask for
    // a block the module does not have would spin. That reasoning holds only
    // while the fill's answer is still reachable, and the line above has just
    // made it unreachable -- the base a span was filled against is gone, and
    // the page will be numbered somewhere else.
    //
    // Measured in `/bin/ls / ; bash -c "/bin/ls /usr"`. pid 3 claims page
    // 4229 at 188416 and fills it, execs, and the page comes back at 302080
    // where nothing is written. The span is still marked, so nothing re-asks
    // it, and every block on that page goes to the translator for the rest of
    // the process's life -- `why[asked]` for every lift in the run, and
    // 15,306 blocks of bash in a process that has bash statically
    // recompiled. See #136.
    FILL_ASKED.0.borrow_mut().clear();
    FILL_FAILED.0.borrow_mut().clear();
    // Slots already handed out are never reused -- a page that comes back
    // through a `fork` checkpoint still points at its own -- so allocation
    // carries on from the high-water mark rather than restarting.
    // No entries at all, so the only thing `from_entries` can record is the
    // mark -- and a claiming process does not allocate there. `usize::MAX`
    // says "nothing here has active segments to place", which is true of an
    // empty entry list whatever the new program turns out to be.
    h.jit.page_table = shared_page_table(&[], h.jit.page_table.slots(), usize::MAX);
    h.jit.translated.clear();
    // Except the closure's. Those modules are still in the dispatch tables --
    // `execve` rebuilds the image in this memory, not the wasm instance
    // around it -- so the new program can have every library the build
    // covered for nothing. See `CLOSURE_DIRS`.
    // What this process has to work with when it becomes a new program.
    //
    // An exec'd bash lifts 15,307 blocks where pid 1 lifts 6, and the lookup
    // that should cover an image page -- `gex_closure_owner` -> `image_is_ours`
    // -- is never reached after an exec, which happens only when
    // `IMAGE_PAGES` has no entry for the page. So print what this process
    // actually holds. See #136.
    if tracing() {
        let imgs = IMAGE_PAGES.0.borrow().len();
        let dirs = CLOSURE_DIRS.0.borrow().len();
        let progs = LIB_PROGRAM.0.borrow().iter().filter(|p| !p.is_empty()).count();
        warn_line(&format!(
            "[gex] exec in pid {}: {imgs} image pages, {dirs} closure dirs, \
             {progs} libraries with a program\n",
            h.kernel.pid,
        ));
    }
    let closure = CLOSURE_DIRS.0.borrow();
    if !closure.is_empty() {
        let mut kept = 0usize;
        for (page, base) in closure.iter() {
            // Not the image's pages. A closure library is pinned to an
            // address of its own and covers the same code whatever the
            // process goes on to run; the *image* sits at `PIE_BASE` and is
            // replaced by this very call.
            //
            // Since `--shared-image` the image is a closure library like any
            // other, so it arrived here with the rest and its entries
            // survived the exec -- pointing the new program's pages at the
            // old program's translated blocks. `ls | wc -l` died with
            // `code 132, pc=0x1003f44`: SIGILL inside `ls`, executing bash.
            // Everything below `INTERP_BASE` is the executable and its break.
            if page * gex_abi::DISPATCH_PAGE < gex_abi::INTERP_BASE {
                continue;
            }
            kept += 1;
            h.jit.page_table.assign(*page, *base);
            let at = (gex_abi::DISPATCH_DIR + page * 4) as usize;
            if at + 4 <= data.len() {
                data[at..at + 4].copy_from_slice(&base.to_le_bytes());
            }
        }
        trace_line(&format!(
            "[gex] exec: {} closure pages kept of {}, {} slots\n",
            kept,
            closure.len(),
            h.jit.page_table.slots()
        ));
    }
    drop(closure);

    let ctx_addr = h.threads[cur()].kstate.ctx_addr;
    let c = ctx_addr as usize;
    // A loaded program starts with clear registers. A restored one starts
    // with the registers the linker left, because `x0` holds the address of
    // `_dl_fini` -- what `__libc_start_main` registers with `atexit` -- and
    // the thread pointer names the TLS block the linker allocated.
    for i in 0..31u8 {
        let v = restored.as_ref().map(|l| l.x[i as usize]).unwrap_or(0);
        write_u64(data, c + ctx::x(i), v);
    }
    if let Some(l) = &restored {
        write_u64(data, c + ctx::TPIDR, l.tpidr);
    }
    // `sp` is the stack just built, not the recorded one: the two are the
    // same address by construction -- see `BLOB_RESERVE` -- and the argv on
    // it is this process's.
    write_u64(data, c + ctx::SP, layout.sp);
    let entry = match (&program, &restored) {
        (Some(p), _) => p.entry(),
        (None, Some(l)) => l.entry,
        (None, None) => return 0,
    };
    write_u64(data, c + ctx::PC, entry);
    write_u64(data, c + ctx::MON_SIZE, 0);
    data[c + ctx::SAFEPOINT_COUNTER..c + ctx::SAFEPOINT_COUNTER + 4]
        .copy_from_slice(&1024i32.to_le_bytes());
    h.threads = vec![Thread {
        kstate: ThreadState::new(ctx_addr, h.kernel.pid),
        pc: entry,
        waiting: Waiting::Runnable,
        blocked_on: None,
    }];
    set_current(h, 0);
    if let (Some(p), None) = (&program, &restored) {
        if hook != 0 {
            snap_arm(&mut h.kernel, key, &p.exe, p.interp_base());
        }
    }
    if restored.is_some() {
        link_state(4);
    }
    entry
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
        // Every store, with what was stored and how big the map became.
        //
        // One-shot was not enough: it printed once in a three-process run and
        // reported a map of one page while the metrics claimed `stored=8`, so
        // three explanations remained open -- a private map per Worker, a key
        // that differs between store and lookup, or almost nothing reaching
        // the global map at all. Naming pid, page and key separates them:
        // whether the length accumulates across processes answers the first,
        // and whether a later process looks up a pair an earlier one wrote
        // answers the second.
        //
        // Capped, because a run that stores thousands would drown the log and
        // the question is answered by the first dozen.
        static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let nth = SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if tracing() && nth < 12 {
            let n = unsafe { k_slot_len() };
            warn_line(&format!(
                "[gex] slot store pid={} page={page:#x} key={key:#x} base={base} -> map {n}\n",
                h.kernel.pid,
            ));
        }
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
/// The page holding the stub that catches the dynamic linker's last jump.
///
/// Just above the stack, which grows down from [`gex_abi::STACK_TOP`], and
/// below [`gex_abi::MMAP_BOTTOM`], so it is inside the fixed low layout every
/// process has and cannot collide with anything the guest maps.
const SNAP_STUB: u64 = gex_abi::STACK_TOP;

/// The syscall number the stub uses to hand control back. Above every number
/// Linux has assigned on aarch64, so no guest can reach it by accident.
const SNAP_SYSCALL: u64 = 0x6765_7801;

/// `movz x8, #0x7801` / `movk x8, #0x6765, lsl #16` / `svc #0`.
///
/// Three instructions and no registers touched but `x8`, which is
/// caller-saved and which `_start` does not read -- so `x0`, holding the
/// address of `_dl_fini` that glibc's `__libc_start_main` will register with
/// `atexit`, arrives at the entry exactly as the linker left it.
const SNAP_STUB_CODE: [u32; 3] = [0xd28f0028, 0xf2aceca8, 0xd4000001];

/// Whether to keep the linked image of a program and start the next process
/// of it from the result.
/// 0 off, 1 record and restore, 2 catch the linker's last jump and throw the
/// result away, 3 serialise the address space and then drop it on the floor
/// without handing it to the global kernel, 4 hand the global kernel the
/// first four kilobytes of it and no more -- which separates "calling across
/// into the kernel here is wrong" from "writing a megabyte and a half into
/// the kernel's memory is wrong".
///
/// The middle setting exists to split the mechanism in half. Catching is a
/// stub page, an auxiliary value and a resume; storing walks the whole
/// address space and pulls a megabyte and a half into another wasm memory.
/// When a run that has both goes wrong, one run with 2 says which half.
static SNAPSHOT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The program being linked in this process, as `(key, real entry)`.
///
/// Set when the stack is built with `AT_ENTRY` pointing at [`SNAP_STUB`], and
/// read when the stub calls in. Zero means this process is not recording.
static SNAP_PENDING: SingleThreaded<(u64, u64)> = SingleThreaded(const { RefCell::new((0, 0)) });

/// What the linked-image store did for this process.
///
/// Reported on the per-process line rather than traced, because the arming
/// happens in the first milliseconds of a process and the terminal a test
/// reads keeps only its last twenty lines -- so the one message that says
/// whether the mechanism engaged at all was always the one that had scrolled
/// away.
///
/// 0 off, 1 asked for with no global kernel to keep the answer in, 2 armed
/// and the program then ran its own linker, 3 kept, 4 restored, 5 asked for
/// and nothing since -- which is what a process that never reached the arming
/// point reports, and is the difference between "the switch did not arrive"
/// and "the switch arrived and did nothing".
static LINK_STATE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[no_mangle]
pub extern "C" fn gex_link_state() -> u32 {
    LINK_STATE.load(std::sync::atomic::Ordering::Relaxed)
}

fn link_state(v: u32) {
    LINK_STATE.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// Turn the linked-image store on or off. Off is what a run with no global
/// kernel gets, since there would be nowhere to keep the answer.
#[no_mangle]
pub extern "C" fn gex_set_snapshot(on: u32) {
    SNAPSHOT.store(on, std::sync::atomic::Ordering::Relaxed);
    if on != 0 {
        link_state(5);
    }
    // SAFETY: `k_present` takes no borrow.
    let kernel = unsafe { k_present() != 0 };
    if on != 0 && !kernel {
        link_state(1);
    }
}

fn snapshots_on() -> bool {
    // SAFETY: `k_present` takes no borrow.
    SNAPSHOT.load(std::sync::atomic::Ordering::Relaxed) != 0 && unsafe { k_present() != 0 }
}

/// Whether a caught linked image is worth keeping, or is being thrown away to
/// find out which half of the mechanism a failure is in.
fn snapshots_stored() -> bool {
    matches!(SNAPSHOT.load(std::sync::atomic::Ordering::Relaxed), 1 | 3)
}

/// Whether the serialised image is handed across to the global kernel, which
/// is the one step that touches a memory another Worker also uses.
fn snapshots_shared() -> bool {
    matches!(SNAPSHOT.load(std::sync::atomic::Ordering::Relaxed), 1 | 4)
}

/// How much of a serialised image is handed across.
fn snapshot_share_cap() -> usize {
    if SNAPSHOT.load(std::sync::atomic::Ordering::Relaxed) == 4 {
        4096
    } else {
        usize::MAX
    }
}

/// What identifies a linked address space.
///
/// The program, and the environment it was linked in: `LD_LIBRARY_PATH` and
/// `LD_PRELOAD` change which objects are loaded and where, and a snapshot
/// taken under one and restored under another would be a different program's
/// libraries at this one's addresses. The whole environment rather than the
/// `LD_` part of it, because it costs a hash either way and a run whose
/// commands all share an environment -- which is every shell -- still hits.
fn snap_key(path: &str, envp: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    for e in envp {
        e.hash(&mut h);
    }
    h.finish()
}

/// Put the catcher stub in the address space and say where the program's own
/// entry is, so the syscall it makes can be answered.
fn snap_arm(kernel: &mut Kernel, key: u64, exe: &gex_ld::Image, interp_base: u64) {
    *SNAP_FACTS.0.borrow_mut() = (exe.phdr_vaddr, exe.phentsize, exe.phnum, interp_base);
    let entry = exe.entry;
    let data = guest_bytes();
    let at = SNAP_STUB as usize;
    for (i, w) in SNAP_STUB_CODE.iter().enumerate() {
        data[at + i * 4..at + i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    kernel.mem.insert_loaded(
        SNAP_STUB,
        SNAP_STUB + 4096,
        gex_kernel::mem::prot::READ | gex_kernel::mem::prot::EXEC,
        "[gex-link]",
        None,
    );
    *SNAP_PENDING.0.borrow_mut() = (key, entry);
    link_state(2);
}

/// The bytes of one mapping, or a note saying where to re-read them.
///
/// A library's text is the file's bytes and nothing has touched it, so the
/// snapshot carries eight bytes and a path rather than two megabytes. What it
/// must carry is everything the linker *wrote*: the GOTs, the relocated
/// `.data`, the TLS template it copied and its own bookkeeping -- which is
/// exactly the set of mappings that have been writable, and is a few hundred
/// kilobytes rather than a few megabytes.
fn snap_write_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn snap_read_str(raw: &[u8], p: &mut usize) -> Option<String> {
    let n = u32::from_le_bytes(raw.get(*p..*p + 4)?.try_into().ok()?) as usize;
    *p += 4;
    let s = String::from_utf8_lossy(raw.get(*p..*p + n)?).into_owned();
    *p += n;
    Some(s)
}

fn snap_read_u64(raw: &[u8], p: &mut usize) -> Option<u64> {
    let v = u64::from_le_bytes(raw.get(*p..*p + 8)?.try_into().ok()?);
    *p += 8;
    Some(v)
}

fn snap_read_u32(raw: &[u8], p: &mut usize) -> Option<u32> {
    let v = u32::from_le_bytes(raw.get(*p..*p + 4)?.try_into().ok()?);
    *p += 4;
    Some(v)
}

/// A program as its dynamic linker left it, ready to be entered.
struct LinkedImage {
    entry: u64,
    /// What the auxiliary vector has to say about the executable, so that a
    /// restored process needs no ELF parse at all.
    phdr: u64,
    phent: u64,
    phnum: u64,
    interp_base: u64,
    x: [u64; 31],
    sp: u64,
    tpidr: u64,
}

/// What the recording process knew about its own executable.
static SNAP_FACTS: SingleThreaded<(u64, u64, u64, u64)> =
    SingleThreaded(const { RefCell::new((0, 0, 0, 0)) });

/// Serialise this process's address space, as the dynamic linker left it.
fn snap_take(h: &mut Host, ctx_addr: u64, entry: u64) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"SNP1");
    out.extend_from_slice(&entry.to_le_bytes());
    let (phdr, phent, phnum, interp_base) = *SNAP_FACTS.0.borrow();
    for v in [phdr, phent, phnum, interp_base] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&h.kernel.mem.brk_current().to_le_bytes());
    {
        let gm = GuestMem::new(guest_bytes());
        for i in 0..31u8 {
            let v = gm.read_u64(ctx_addr + ctx::x(i) as u64).unwrap_or(0);
            out.extend_from_slice(&v.to_le_bytes());
        }
        for f in [ctx::SP, ctx::TPIDR] {
            let v = gm.read_u64(ctx_addr + f as u64).unwrap_or(0);
            out.extend_from_slice(&v.to_le_bytes());
        }
    }

    // Which mappings, and in which form. The stack is rebuilt for whatever
    // arguments the next process has rather than carried, and the catcher
    // stub belongs to the process that recorded, not to the one that
    // restores.
    let regions: Vec<(u64, u64, u32, String, Option<(String, u64)>, Option<(String, u64)>)> = h
        .kernel
        .mem
        .vmas()
        .iter()
        .filter(|v| v.name != "[stack]" && v.start != SNAP_STUB)
        .map(|v| {
            (
                v.start,
                v.end,
                v.prot,
                v.name.clone(),
                v.code.clone(),
                // Re-readable only if it has never been writable, which is
                // the invariant `origin` carries -- and which is asked for
                // again here rather than trusted, because the whole image
                // turns on it: re-reading a page the linker wrote is how a
                // restored process gets an unrelocated GOT.
                v.origin
                    .clone()
                    .filter(|_| v.prot & gex_kernel::mem::prot::WRITE == 0),
            )
        })
        .collect();
    out.extend_from_slice(&(regions.len() as u32).to_le_bytes());
    let data = guest_bytes();
    for (start, end, prot, name, code, origin) in regions {
        out.extend_from_slice(&start.to_le_bytes());
        out.extend_from_slice(&end.to_le_bytes());
        out.extend_from_slice(&prot.to_le_bytes());
        snap_write_str(&mut out, &name);
        match &code {
            Some((id, off)) => {
                out.extend_from_slice(&1u32.to_le_bytes());
                snap_write_str(&mut out, id);
                out.extend_from_slice(&off.to_le_bytes());
            }
            None => out.extend_from_slice(&0u32.to_le_bytes()),
        }
        match &origin {
            Some((id, off)) => {
                out.extend_from_slice(&1u32.to_le_bytes());
                snap_write_str(&mut out, id);
                out.extend_from_slice(&off.to_le_bytes());
            }
            None => {
                // By the page, with a bitmap of which ones hold anything.
                //
                // Most of a linked address space is zero: `.bss`, the tail of
                // the break, the part of a TLS block nothing has written.
                // Carried whole, `ls` came to 1682 KiB; the bytes that are
                // actually the linker's answer are a fraction of that, and
                // every one of them is in a page some store has touched.
                out.extend_from_slice(&0u32.to_le_bytes());
                let (a, b) = (start as usize, (end as usize).min(data.len()));
                let bytes: &[u8] = if a < b { &data[a..b] } else { &[] };
                let pages = bytes.len().div_ceil(4096);
                out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                let bitmap_at = out.len();
                out.resize(bitmap_at + pages.div_ceil(8), 0);
                for p in 0..pages {
                    let lo = p * 4096;
                    let hi = (lo + 4096).min(bytes.len());
                    if bytes[lo..hi].iter().all(|b| *b == 0) {
                        continue;
                    }
                    out[bitmap_at + p / 8] |= 1 << (p % 8);
                    out.extend_from_slice(&bytes[lo..hi]);
                }
            }
        }
    }
    out
}

/// Put a recorded address space back, and answer with the entry to resume at.
///
/// Zero if the blob cannot be read or a file it names has gone, in which case
/// the caller loads the program the long way.
/// What putting the last address space back cost: regions, bytes re-read
/// from files, bytes copied out of the image, and milliseconds in each.
static SNAP_COST: SingleThreaded<(u64, u64, u64, u64, u64)> =
    SingleThreaded(const { RefCell::new((0, 0, 0, 0, 0)) });

#[no_mangle]
pub extern "C" fn gex_link_cost(out: u32) -> u32 {
    let c = *SNAP_COST.0.borrow();
    let dst = shim_slice_mut(out, 40);
    for (i, v) in [c.0, c.1, c.2, c.3, c.4].iter().enumerate() {
        dst[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    40
}

fn snap_put(h: &mut Host, raw: &[u8]) -> Option<LinkedImage> {
    let mut p = 0usize;
    if raw.get(0..4)? != b"SNP1" {
        return None;
    }
    p += 4;
    let entry = snap_read_u64(raw, &mut p)?;
    let phdr = snap_read_u64(raw, &mut p)?;
    let phent = snap_read_u64(raw, &mut p)?;
    let phnum = snap_read_u64(raw, &mut p)?;
    let interp_base = snap_read_u64(raw, &mut p)?;
    let brk = snap_read_u64(raw, &mut p)?;
    let mut x = [0u64; 31];
    for slot in x.iter_mut() {
        *slot = snap_read_u64(raw, &mut p)?;
    }
    let sp = snap_read_u64(raw, &mut p)?;
    let tpidr = snap_read_u64(raw, &mut p)?;
    let n = snap_read_u32(raw, &mut p)?;

    h.kernel.mem = gex_kernel::mem::MemoryMap::new(brk);
    h.kernel.mem.grows_on_demand(arena_grow, arena_back);
    // One read per file, not one per mapping. A library arrives as four
    // mappings -- text, rodata, relro, data -- and re-reading `libc.so.6` for
    // each of them is seven megabytes of filesystem for one and three
    // quarters of library.
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let (mut n_read, mut n_copy, mut ms_read, mut ms_copy) = (0u64, 0u64, 0u64, 0u64);
    for _ in 0..n {
        let start = snap_read_u64(raw, &mut p)?;
        let end = snap_read_u64(raw, &mut p)?;
        let prot = snap_read_u32(raw, &mut p)?;
        let name = snap_read_str(raw, &mut p)?;
        let code = if snap_read_u32(raw, &mut p)? == 1 {
            let id = snap_read_str(raw, &mut p)?;
            Some((id, snap_read_u64(raw, &mut p)?))
        } else {
            None
        };
        let from_file = snap_read_u32(raw, &mut p)? == 1;
        // The memory has to reach the mapping before anything is written
        // into it: a restored process starts with the fixed low layout and
        // nothing above it, and every library is above it.
        if !arena_back(end) {
            return None;
        }
        if from_file {
            let path = snap_read_str(raw, &mut p)?;
            let off = snap_read_u64(raw, &mut p)?;
            let began = now_ns();
            if !files.iter().any(|(k, _)| *k == path) {
                files.push((path.clone(), h.kernel.read_file(&path)?));
            }
            ms_read += now_ns() - began;
            n_read += end - start;
            let bytes = &files.iter().find(|(k, _)| *k == path)?.1;
            let data = guest_bytes();
            let (a, b) = (start as usize, (end as usize).min(data.len()));
            if a >= b {
                return None;
            }
            data[a..b].fill(0);
            let take = (b - a).min(bytes.len().saturating_sub(off as usize));
            data[a..a + take].copy_from_slice(&bytes[off as usize..off as usize + take]);
            h.kernel.mem.insert_from(start, end, prot, &name, Some((path, off)));
        } else {
            let len = snap_read_u32(raw, &mut p)? as usize;
            let pages = len.div_ceil(4096);
            let began = now_ns();
            let bitmap = raw.get(p..p + pages.div_ceil(8))?.to_vec();
            p += pages.div_ceil(8);
            let data = guest_bytes();
            let (a, b) = (start as usize, (end as usize).min(data.len()));
            if a >= b {
                return None;
            }
            // Zero first: a page the bitmap says nothing about is a page the
            // recording found zero, and this memory is a fresh Worker's but
            // not necessarily untouched -- an `execve` runs in the Worker
            // that had the previous program.
            data[a..b].fill(0);
            for q in 0..pages {
                if bitmap[q / 8] & (1 << (q % 8)) == 0 {
                    continue;
                }
                let lo = q * 4096;
                let hi = (lo + 4096).min(len);
                let payload = raw.get(p..p + (hi - lo))?;
                p += hi - lo;
                let (da, db) = (a + lo, (a + hi).min(b));
                if da < db {
                    data[da..db].copy_from_slice(&payload[..db - da]);
                }
            }
            h.kernel.mem.adopt(start, end, prot, &name, code);
            ms_copy += now_ns() - began;
            n_copy += end - start;
        }
    }
    *SNAP_COST.0.borrow_mut() = (
        n as u64,
        n_read,
        n_copy,
        ms_read / 1_000_000,
        ms_copy / 1_000_000,
    );
    Some(LinkedImage { entry, phdr, phent, phnum, interp_base, x, sp, tpidr })
}

/// Fetch the linked image of `key` out of the global kernel.
fn snap_lookup(key: u64) -> Option<Vec<u8>> {
    if !snapshots_on() {
        return None;
    }
    // SAFETY: both calls take scalars, and the second is handed a pointer
    // into this module's memory with the length the first reported.
    unsafe {
        let len = k_snap_len(key);
        if len == 0 {
            return None;
        }
        let mut blob = vec![0u8; len as usize];
        let got = k_snap_get(key, blob.as_mut_ptr() as u32, len);
        (got == len).then_some(blob)
    }
}

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

thread_local! {
    /// Why a page that the identity map recognised still had nothing to
    /// serve: nobody claimed it, and every claim was already installed.
    static SERVE_NONE: std::cell::RefCell<(u64, u64)> = const { std::cell::RefCell::new((0, 0)) };
    /// Why a page had no identity: the mapping was writable, or it had no
    /// `code`.
    static NOIDENT_WHY: std::cell::RefCell<(u64, u64)> = const { std::cell::RefCell::new((0, 0)) };
}

/// Which *object* a guest address belongs to, if it is one that was loaded
/// from somewhere nameable.
///
/// The coarse half of [`page_identity`]: the file and nothing else. A page
/// identity says "the same code at the same address"; this says "the same
/// library", which is the grain a process wants to install at. Having missed
/// once inside libc, it will miss all over libc, and every one of those is a
/// module somebody has already built.
fn object_identity(h: &Host, addr: u64) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let v = h
        .kernel
        .mem
        .vmas()
        .iter()
        .find(|v| addr >= v.start && addr < v.end)?;
    if v.prot & gex_kernel::mem::prot::WRITE != 0 {
        return None;
    }
    let (id, _) = v.code.as_ref()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    Some(hasher.finish())
}

/// Every module any process has stored for this object that this Worker has
/// not got: `(id, slots, dirs, wasm)` each.
/// Whether the object covering this page has been translated whole.
fn obj_finished(h: &Host, page: u64) -> bool {
    // SAFETY: a scalar in, a scalar out, and no borrow of anything this
    // module owns.
    unsafe {
        if k_present() == 0 {
            return false;
        }
        match object_identity(h, page * gex_abi::DISPATCH_PAGE) {
            Some(obj) => k_obj_is_translated(obj) != 0,
            None => false,
        }
    }
}

fn shared_modules_of_object(h: &Host, obj: u64) -> Vec<(u32, u64, Vec<(u64, u32)>, Vec<u8>)> {
    let mut out = Vec::new();
    // SAFETY: scalars in, and the readers below size their buffers from the
    // kernel's own answer before asking it to fill them.
    unsafe {
        if k_present() == 0 {
            return out;
        }
        for n in 0.. {
            let id = k_mod_for_object(obj, n);
            if id == 0 {
                break;
            }
            if h.jit.installed_mods.contains(&id) || out.iter().any(|(i, ..)| *i == id) {
                continue;
            }
            let len = k_mod_len(id) as usize;
            if len == 0 {
                continue;
            }
            let mut wasm = vec![0u8; len];
            if k_mod_read(id, wasm.as_mut_ptr() as u32, len as u32) as usize != len {
                continue;
            }
            let dlen = k_mod_dirs_len(id) as usize;
            let mut raw = vec![0u8; dlen];
            if dlen > 0 && k_mod_dirs_read(id, raw.as_mut_ptr() as u32, dlen as u32) as usize != dlen
            {
                continue;
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
            out.push((id, k_mod_slots(id), dirs, wasm));
        }
    }
    out
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
        // The first module covering this page that this Worker has not got.
        // A page accumulates one claim per translation unit that reached it,
        // and taking only the first left the rest of the page's blocks to be
        // lifted again -- four hundred pages recognised, nineteen modules
        // served, and the second `ls` still translating fifteen thousand
        // blocks.
        let mut id = 0;
        let mut seen = 0;
        for n in 0.. {
            let candidate = k_mod_for(page, key, n);
            if candidate == 0 {
                SERVE_NONE.with(|c| {
                    let mut c = c.borrow_mut();
                    if seen == 0 {
                        c.0 += 1;
                    } else {
                        c.1 += 1;
                    }
                });
                return None;
            }
            seen += 1;
            if !h.jit.installed_mods.contains(&candidate) {
                id = candidate;
                break;
            }
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
    // Which of the two reasons a page has no identity, counted apart. They
    // want different fixes -- a writable mapping is code the guest could have
    // changed, a mapping with no `code` is one the loader declined to name --
    // and "noident" alone could not tell them apart. Under Node this is
    // mostly zero and in Chromium it was every miss.
    if v.prot & gex_kernel::mem::prot::WRITE != 0 {
        NOIDENT_WHY.with(|c| c.borrow_mut().0 += 1);
        return None;
    }
    if v.code.is_none() {
        NOIDENT_WHY.with(|c| {
            let mut c = c.borrow_mut();
            c.1 += 1;
            // The first few, named. A count says how often a page could not be
            // identified; only the mapping says which loader path left it
            // unnamed, and those want different fixes.
            if tracing() && c.1 <= 8 {
                trace_line(&format!(
                    "[gex] no code identity for {:#x}: vma {:#x}..{:#x} prot {} name {:?}\n",
                    page, v.start, v.end, v.prot, v.name
                ));
            }
        });
    }
    // What was *loaded* here, not what could be re-read from disk. Those are
    // different questions and one field was answering both: an image segment
    // has no `origin` because its zeroed tail cannot be reconstructed, and an
    // `mprotect`ed piece loses its `origin` because RELRO relocations are not
    // the file's bytes. Both true, and neither says the code differs.
    let (id, off) = v.code.as_ref()?;
    // The file and the offset within it, and nothing else.
    //
    // The extent of the mapping used to be in here too, and it made the key
    // unstable in both directions. `ld.so` builds a library's mapping in
    // four steps -- reserve the span, `MAP_FIXED` each segment over it,
    // `munmap` the slack, `mprotect` the RELRO -- and every one of those
    // splits a vma. Which splits a page ends up on either side of depends on
    // what else the process has mapped, so two processes running the same
    // program hashed the same libc page differently; and the RELRO
    // `mprotect` happens *after* some pages are already translated, so a
    // page's identity changed during the life of one process.
    //
    // Neither is a fact about the code. What determines the bytes on a page
    // is the file and the offset, and what determines whether a translation
    // of them is reusable is the address -- which is already the other half
    // of the key, since the store is indexed by `(page, identity)`.
    //
    // Measured on three `ls` in one shell: 18 shared modules served out of
    // 1,295 dispatch misses before, because the key almost never matched.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    (off + (page - v.start)).hash(&mut hasher);
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
    /// Run this thread: which one, whether its deadline passed, and whether
    /// what woke it was a signal rather than the thing it was waiting for.
    Thread(usize, bool, bool),
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
/// Whether a pending signal would actually *do* something to this process.
///
/// Two steps, and the second is the one that matters. First the inbox other
/// processes write into is drained into this process's own pending set --
/// idempotent, and it puts the bits where the dispatcher reads them anyway.
/// Then the question is asked of the *disposition*, not of the bit.
///
/// "Is a bit set" is the wrong question for ending a wait. An ignored signal
/// interrupts nothing on Linux, and `SIGCHLD` is ignored by default and
/// arrives every time a child exits -- so a shell waiting on a pipe would be
/// woken by its own children, told `EINTR`, and sent round again for nothing.
/// Nested command substitution is that shape three deep.
fn any_signal(h: &mut Host) -> bool {
    h.kernel.collect_signals();
    h.kernel.signals.would_deliver()
}

fn thread_ready(t: &Thread, now: u64, signalled: bool) -> bool {
    // A signal ends a wait -- every interruptible one, which here is all of
    // them but a thread that has exited or is running in another Worker.
    //
    // Nothing did this, and it is the whole of why `^C` reached the
    // foreground group and nothing happened. A guest in `nanosleep` parks as
    // `Waiting::Sleep(deadline)` and becomes runnable when the deadline
    // passes and at no other moment, so a SIGINT sat in the process table for
    // thirty seconds and was collected by a process that was about to exit
    // anyway. The same for a read on a pipe, a `wait4` on a child, a poll:
    // this host could interrupt none of them.
    //
    // Becoming runnable is all that is needed. The picked thread finishes its
    // parked call, and the code that follows already asks `deliver_pending`,
    // which either builds the handler's frame or ends the process.
    if SIGNAL_ENDS_WAIT && signalled && !matches!(t.waiting, Waiting::Exited | Waiting::Elsewhere) {
        return true;
    }
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
/// What every thread of this process is waiting for, on demand.
///
/// The two censuses that existed both fire on a counter -- 1024 idle spins,
/// 4096 wakes -- and a deadlock reaches neither. Nothing spins, because every
/// thread is parked on something it believes will arrive; nothing wakes,
/// because nothing does. So the one state that most needs describing was the
/// one state no instrument could see, and a wedged run printed *nothing at
/// all*: no output, no exit, no idle report.
///
/// Written into the caller's memory as ASCII, returning the length. The host
/// asks when a run has gone quiet.
#[no_mangle]
pub extern "C" fn gex_wait_census(out: u32, max: u32) -> u32 {
    // Why the run loop cannot proceed, alongside what the threads want.
    //
    // A thread that says `runnable` and a loop that parks anyway is a
    // contradiction, and the two numbers that resolve it are the pause mask
    // -- which decides *which* way the loop waits -- and the resume address,
    // which is what it would need to carry on. Neither is reachable from the
    // thread table, and without them "t0=runnable" is a fact with no
    // explanation attached.
    let why = paused();
    let resume = resume().unwrap_or(0);
    // Whether this process is over. The run loop asks it before anything
    // else -- `gex_exited() ? 0 : gex_resume_pc()` -- so a process that has
    // finished takes the waiting branch however runnable its threads look,
    // and that is the only reading that fits `paused=0`, a resume address
    // that is set, and a thread that says runnable.
    let over = with_host_free(|h| h.exit.is_some()).unwrap_or(false);
    let text = with_host_free(|h| {
        let mut s = format!(
            "pid {} paused={why:#x} resume={resume:#x} exited={over}:",
            h.kernel.pid
        );
        for (i, t) in h.threads.iter().enumerate() {
            s.push_str(&format!(" t{i}={}@{:#x}", waiting_name(&t.waiting), t.pc));
            // A name says a thread is in `read`; it does not say what it is
            // reading, which is the whole question when two processes are
            // waiting for each other.
            match &t.waiting {
                Waiting::Read(h) | Waiting::Write(h) | Waiting::Accept(h) => {
                    let d = h.debug_state();
                    if !d.is_empty() {
                        s.push_str(&format!("({d})"));
                    }
                }
                _ => {}
            }
            if let Some(gex_kernel::BlockOp::WaitChild { want, .. }) = &t.blocked_on {
                s.push_str(&format!("(want {want})"));
            }
        }
        s
    })
    .unwrap_or_else(|| "pid ?: the kernel is held".to_string());
    let b = text.as_bytes();
    let n = (b.len() as u32).min(max);
    let mut gm = GuestMem::new(guest_bytes());
    let _ = gm.write(out as u64, &b[..n as usize]);
    n
}

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
            let sig = any_signal(h);
            let i = cur();
            if thread_ready(&h.threads[i], now, sig) {
                let timed_out = match &h.threads[i].waiting {
                    Waiting::Futex { parked, .. } => !parked.woken(),
                    _ => false,
                };
                // Woken by the signal and not by the wait: ask the same
                // question with the signal taken away.
                let by_signal = sig && !thread_ready(&h.threads[i], now, false);
                Pick::Thread(i, timed_out, by_signal)
            } else if matches!(h.threads[i].waiting, Waiting::Exited) {
                // This Worker's thread is done. The process is not: the
                // others are Workers of their own, and whoever joins this one
                // is already parked on its futex.
                Pick::Over
            } else {
                // Nothing this Worker owns can run. Everything else in the
                // process is a Worker the engine is already running, and it
                // makes its own progress without being switched to.
                Pick::Idle
            }
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
        let Pick::Thread(i, timed_out, by_signal) = pick else {
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
                // ever wake. On Linux this hangs; saying so is more useful --
                // and saying *which* futex, and where the guest is standing,
                // is what turns it from a symptom into something to look up.
                //
                // Without the addresses, `zstd` reported this twice and left
                // nothing to go on but the word "futex": see #113, where the
                // output turned out to be a byte-exact prefix of QEMU's and
                // the only defect was whatever parks here.
                let who = with_host(|h| {
                    let mut s = String::new();
                    let gm = GuestMem::new(guest_bytes());
                    for (i, t) in h.threads.iter().enumerate() {
                        if let Waiting::Futex { parked, .. } = &t.waiting {
                            let pc = gm
                                .read_u64(t.kstate.ctx_addr + ctx::PC as u64)
                                .unwrap_or(0);
                            s.push_str(&format!(
                                "gex:   thread {i} parked on {:#x}, pc {:#x}{}\n",
                                parked.addr(),
                                pc,
                                h.kernel
                                    .mem
                                    .vmas()
                                    .iter()
                                    .find(|v| pc >= v.start && pc < v.end)
                                    .map(|v| format!(" in {} +{:#x}", v.name, pc - v.start))
                                    .unwrap_or_default(),
                            ));
                        }
                    }
                    s
                });
                trace_line("gex: every thread is blocked on a futex\n");
                trace_line(&who);
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
            let was_futex = matches!(t.waiting, Waiting::Futex { .. });
            t.waiting = Waiting::Runnable;
            let op = t.blocked_on.take();
            // A thread that parked in a futex and arrives here with nothing
            // to finish is resumed with no return value written, so the guest
            // reads whatever `x0` last held -- which for `futex(uaddr, op)` is
            // `uaddr`, a large positive number. glibc takes the negation of
            // that as an errno, does not recognise it, and calls
            // `futex_fatal_error`. Exactly the shape of #126, so it is worth
            // knowing whether it ever happens rather than assuming.
            if was_futex && op.is_none() {
                warn_line(&format!(
                    "[gex] pid {} resumed thread {i} from a futex with no result to write",
                    h.kernel.pid,
                ));
            }
            let (mut pc, kstate) = (t.pc, t.kstate.clone());
            if let Some(op) = op {
                // The syscall that parked this thread still owes it a return
                // value, and the kernel writes that into the context of
                // whichever thread it thinks is running.
                let mut gm = GuestMem::new(guest_bytes());
                h.kernel.enter_thread(&kstate);
                let r = finish_parked(&op, timed_out, by_signal, &mut gm);
                // A parked call's answer never reaches the syscall ring: the
                // ring is written when the call is made, and a call that
                // blocks has not produced one yet. An error here is therefore
                // invisible, and for a futex that matters -- glibc aborts the
                // process over answers it does not expect.
                if let gex_kernel::BlockOp::FutexWait { uaddr, .. } = &op {
                    ftx_note(
                        !0,
                        *uaddr,
                        match &r {
                            Ok(v) => *v as i64,
                            Err(e) => -(e.0 as i64),
                        },
                    );
                }
                let outcome = h.kernel.finish_blocking(&mut gm, r);
                // Did the answer actually land in the register the guest will
                // read? Every futex answer this runtime produces is legal --
                // 0, or a count -- and glibc still says it was given one it
                // does not recognise, so the remaining possibility is that
                // what was written is not what is there. `finish_blocking`
                // writes into whichever thread the kernel currently thinks is
                // running, and with two threads that is a claim worth
                // checking rather than assuming.
                if let (gex_kernel::BlockOp::FutexWait { .. }, Ok(v)) = (&op, &r) {
                    let got = gm.read_u64(kstate.ctx_addr + ctx::x(0) as u64).unwrap_or(!0);
                    if got != *v {
                        warn_line(&format!(
                            "[gex] pid {} wrote {v} for a futex wait and x0 holds {got:#x}",
                            h.kernel.pid,
                        ));
                    }
                }
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
    by_signal: bool,
    gm: &mut GuestMem<'_>,
) -> Result<u64, Errno> {
    // A wait that a *signal* ended did not get what it was waiting for, and
    // saying otherwise is worse than saying nothing: a blocking read finished
    // this way re-attempts the read, finds the pipe still empty and hands the
    // guest `EAGAIN` on a descriptor that is not non-blocking. `EINTR` is
    // what Linux returns and what every caller already has a branch for.
    //
    // It matters most for the signals that change nothing. `SIGCHLD` is
    // ignored by default and is sent on every child exit, so without this
    // every shell waiting on a pipe would be woken by its own children and
    // told something untrue about the pipe.
    if by_signal {
        return Err(Errno::EINTR);
    }
    match op {
        gex_kernel::BlockOp::FutexWait { deadline, .. } if timed_out => {
            // Loud, because glibc treats this as fatal when it did not ask
            // for a timeout: `futex_wait_simple` tolerates 0, EAGAIN and
            // EINTR and calls `futex_fatal_error` for anything else. A wait
            // with no deadline that comes back timed out is this runtime
            // inventing an error the caller has no branch for.
            if deadline.is_none() {
                warn_line("[gex] a futex wait with no deadline came back timed out");
            }
            Err(Errno::ETIMEDOUT)
        }
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
    // This *is* how a syscall returns when it redirects, so the guest is no
    // longer inside one.
    //
    // `InSyscall`'s `Drop` cannot say so: a wasm trap aborts rather than
    // unwinds, which is exactly the property that makes the flag trustworthy
    // for a *crash* inside a handler -- and exactly what makes it a lie here,
    // where trapping is the ordinary way out of `execve`, a dispatch miss and
    // a thread switch. Left unset, every crash after the first `execve`
    // reported itself as being inside syscall 221, and that sent the hunt for
    // the demo site's crash into `do_exec` twice.
    IN_SYSCALL.store(false, std::sync::atomic::Ordering::Relaxed);
    warch::unreachable()
}

/// The syscall number the guest is inside, or the last one it was inside.
///
/// A plain static, deliberately: it is read after a crash, when the kernel
/// may still be borrowed by whatever crashed, and anything that took the
/// kernel to answer would panic instead of answering.
static LAST_SYSCALL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(!0);

/// Whether the guest is *still* inside that syscall, rather than past it.
///
/// `LAST_SYSCALL` alone cannot tell the two apart, and the difference is the
/// whole value of the number. `apt update` crashed on a `ret` in a forked
/// child and reported "inside syscall 99" -- `set_robust_list`, whose handler
/// is `Ok(0)` and touches nothing. It was simply the last call the child had
/// made before running on into translated code. Days were spent inside a
/// handler that could not fault.
///
/// Cleared by dropping [`InSyscall`], which a trap does not run: a wasm trap
/// aborts rather than unwinds, so a crash genuinely inside a handler leaves
/// this set, which is exactly what it should say.
static IN_SYSCALL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct InSyscall;

impl Drop for InSyscall {
    fn drop(&mut self) {
        IN_SYSCALL.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Which syscall the guest was in. `0xffffffff` if it has made none.
#[no_mangle]
pub extern "C" fn gex_last_syscall() -> u32 {
    LAST_SYSCALL.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether [`gex_last_syscall`] names a call still on the stack.
#[no_mangle]
pub extern "C" fn gex_in_syscall() -> u32 {
    IN_SYSCALL.load(std::sync::atomic::Ordering::Relaxed) as u32
}

/// The address of the last indirect branch the guest took.
///
/// The only address worth reporting for a trap on the dispatch path, and the
/// only one that survives it: see [`gex_abi::ctx::LAST_TARGET`]. Read without
/// the kernel, because a crash may have left it borrowed.
#[no_mangle]
pub extern "C" fn gex_last_target() -> i64 {
    let at = CTX_ADDR.load(std::sync::atomic::Ordering::Relaxed);
    if at == 0 {
        return 0;
    }
    GuestMem::new(guest_bytes())
        .read_u64(at + ctx::LAST_TARGET as u64)
        .unwrap_or(0) as i64
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
    // A signal, if one is pending for this process.
    //
    // Every other host has done this since signals landed and the browser
    // never has: here a signal was only ever noticed on the way out of a
    // syscall. That is enough for a program doing I/O and no use at all for
    // one that is not -- and "not" is precisely the case someone reaches for
    // `^C` in. A guest in a loop with no syscall in it could not be
    // interrupted by anything, so `while :; do :; done` at a shell owned the
    // tab until it was closed.
    //
    // Cheap to ask: `has_signals` is one atomic load on a word in the shared
    // kernel, and this runs once per SLICES_PER_SWITCH safepoints at most.
    // Off is a bisect, not a retreat: this is the newest thing on the
    // hottest path, and the question "is the crash in here" has no cheaper
    // answer than building it both ways.
    if SAFEPOINT_SIGNALS {
    // Two steps, because this runs once per thousand back edges in every
    // Worker at once. `k_signal_seq` is one relaxed load of a counter every
    // `kill` bumps; the real question -- which walks the process table behind
    // its lock -- is only asked when that counter has moved. A signal raised
    // in this process (a timer, `raise`) is a field read behind the host's
    // `RefCell` and costs nothing either way.
    let sent = if unsafe { k_present() } != 0 { unsafe { k_signal_seq() } } else { 0 };
    let anything = SIGNAL_SEEN.swap(sent, std::sync::atomic::Ordering::Relaxed) != sent
        || with_host(|h| h.kernel.signals.has_pending());
    // A thread table with this thread in it. A safepoint can fire in a child
    // that is still being built, and the preemption below only indexes
    // `threads` after establishing there are at least two of them -- this had
    // no such guard, and an empty table is a panic rather than an answer.
    let have_thread = with_host(|h| cur() < h.threads.len());
    if have_thread
        && anything
        && with_host(|h| h.kernel.procs.has_signals(h.kernel.pid) || h.kernel.signals.has_pending())
    {
        let delivered = with_host(|h| {
            let a = h.threads[cur()].kstate.ctx_addr;
            let mut gm = GuestMem::new(guest_bytes());
            // The context's pc has to be exact before a handler frame is
            // built on top of it: the frame records where to return to, and a
            // plain branch does not store the pc anywhere.
            let _ = gm.write_u64(a + ctx::PC as u64, pc as u64);
            h.threads[cur()].pc = pc as u64;
            h.kernel.enter_thread(&h.threads[cur()].kstate);
            let d = h.kernel.deliver_pending(&mut gm);
            h.kernel.leave_thread(&mut h.threads[cur()].kstate);
            match d {
                gex_kernel::signal::Delivery::None => 0,
                // Where the handler starts. Left for the *scheduler* to act
                // on, exactly as the preemption below does: this thread is
                // runnable at a new pc, and `schedule` is what turns that
                // into a resume the host understands. Setting the resume
                // state here instead looks equivalent and is not -- it skips
                // the wake bookkeeping at the top of `schedule` and the
                // thread choice underneath it, and the run ends without a
                // word.
                gex_kernel::signal::Delivery::Resume(target) => {
                    h.threads[cur()].pc = target;
                    h.threads[cur()].waiting = Waiting::Runnable;
                    1
                }
                gex_kernel::signal::Delivery::Terminate { signal, .. } => {
                    exit_process(h, -(signal as i32));
                    2
                }
            }
        });
        if delivered == 1 && schedule() != 0 {
            trap();
        }
        if delivered == 2 {
            trap();
        }
    }
    }
    // Preemption is the engine's job now.
    //
    // This used to count safepoints and hand the stack to another thread of
    // this process every fourth slice. There is no other thread here to hand
    // it to: each one is a Worker, and the check that asked "could another
    // thread run?" answered yes about a thread already running somewhere
    // else -- which put two Workers on one set of registers.
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
                    warn_line("gex: corrupt signal frame on sigreturn\n");
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
    //
    // But first: does a module this process already holds cover this page and
    // simply have its slots unwritten? Filling one page is microseconds;
    // recompiling the object around it took 56 seconds across an
    // `apt-get update` under a deferred closure, against 0.4 for the same run
    // with active segments. The ask used to sit *below* this, so a deferred
    // process recompiled whole libraries it already had, and the mechanism
    // meant to avoid work bought the most expensive kind.
    //
    // Once per page: a page nobody can fill falls through on the next miss to
    // everything below, which is what happened before any of this existed.
    if FILL_POSSIBLE.load(std::sync::atomic::Ordering::Relaxed) {
        // Once per *span*, not once per page. A fill writes `FILL_SPAN` slots
        // around the one that missed rather than the whole page, so a later
        // branch elsewhere in the same page has to be able to ask again --
        // and asking twice for one span would loop for ever, which is what
        // this set prevents. Eight spans to a page at 128 slots.
        let page = gex_abi::dispatch_page(pc);
        let width = FILL_WIDTH.load(std::sync::atomic::Ordering::Relaxed);
        let from = ((pc & 0xfff) >> 2) as u32 & !(width - 1);
        let spans = gex_abi::SLOTS_PER_PAGE as u32 / width;
        let key = page * spans as u64 + (from / width) as u64;
        let fresh = FILL_ASKED.0.borrow_mut().insert(key);
        if fresh {
            FILL_FROM.store(from, std::sync::atomic::Ordering::Relaxed);
            FILL_PC.store(pc, std::sync::atomic::Ordering::Relaxed);
            FILL_WANTED.store(page as i64, std::sync::atomic::Ordering::Relaxed);
            FILL_PENDING.store(page as i64, std::sync::atomic::Ordering::Relaxed);
            with_host(|h| {
                set_resume(pc);
                set_resume_ctx(h.threads[cur()].kstate.ctx_addr);
            });
            trap();
        }
    }
    // Past the fill, and a closure library still claims this page.
    //
    // That is the case worth counting, because everything about it looks
    // right from outside: the pin took, the module covers the page, the
    // directory names it -- and the block is lifted anyway. Either the fill
    // was never asked for, or it was asked and the module had no block at
    // this address. The count says how much the closure is failing to save;
    // `[closure] page unfilled` says where to look.
    if gex_closure_owner(gex_abi::dispatch_page(pc)) >= 0 {
        let n = with_host(|h| {
            h.jit.lift_in_closure += 1;
            h.jit.lift_in_closure
        });
        if n <= 8 || (tracing() && n % 1000 == 0) {
            trace_line(&format!(
                "[closure] page unfilled: lifting {pc:#x}, page {} is claimed ({n} so far)\n",
                gex_abi::dispatch_page(pc)
            ));
        }
    }
    // Why this miss is about to translate, counted at the moment it does.
    //
    // Every count in #136 says *where* blocks came from and none says *why*,
    // and three plausible causes -- a thin closure, a narrow fill span, the
    // bulk recompiler -- were each eliminated by a full build-and-measure
    // cycle that a counter here would have settled in one.
    //
    //   0  filling is not possible yet: no closure module has registered
    //   1  the page has no closure owner, so nothing could have filled it
    //   2  owned, and this span was already asked for -- the fill ran and
    //      the guest missed here anyway, which is the interesting one
    //   3  owned, span fresh, and it still arrived here
    {
        let why = if !FILL_POSSIBLE.load(std::sync::atomic::Ordering::Relaxed) {
            0
        } else if gex_closure_owner(gex_abi::dispatch_page(pc)) < 0 {
            1
        } else {
            let width = FILL_WIDTH.load(std::sync::atomic::Ordering::Relaxed);
            let page = gex_abi::dispatch_page(pc);
            let from = ((pc & 0xfff) >> 2) as u32 & !(width - 1);
            let spans = gex_abi::SLOTS_PER_PAGE as u32 / width;
            let key = page * spans as u64 + (from / width) as u64;
            if FILL_ASKED.0.borrow().contains(&key) { 2 } else { 3 }
        };
        LIFT_WHY[why].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // And keep the addresses of the interesting case, because only this
        // side ever sees them.
        //
        // `why == 2` is a span the fill already answered, and every lift in
        // the demo is one. Whether the slot such an address dispatches
        // through is written at this moment decides between a dispatch fault
        // and a disagreement about block addresses -- and JavaScript, which
        // is the only thing that can read a table, is not called here. So
        // record them and let the Worker look afterwards.
        if why == 2 {
            let n = LIFT_PC_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as usize;
            if n < LIFT_PCS.len() {
                LIFT_PCS[n].store(pc, std::sync::atomic::Ordering::Relaxed);
                // And the base *now*, because by the time anything reads this
                // the process may be a different program.
                //
                // A forked child runs its parent's code until it execs. pid 4
                // of the `ls`-under-bash run lifts 6,864 blocks of bash's text
                // while it is still bash, then becomes `/bin/ls` -- so a probe
                // that reads at exit asks about `ls` and gets "no owner" for
                // pages that are bash's, which says nothing about the moment
                // the block was lifted. See #136.
                LIFT_BASES[n].store(
                    gex_page_base(gex_abi::dispatch_page(pc) as u32),
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
        }
    }
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
        // Before anything else, ask whether a module this process already
        // holds covers this page and has simply not written its slots.
        //
        // The page, not a directory entry. A miss knows the address it
        // faulted on and therefore always knows the page; it knows an entry
        // only for a page some module has already claimed here, and that is a
        // tiny minority -- 109 of them against 116,442 misses in an
        // `apt-get update` against a closure. Gating the ask on an entry left
        // every other miss to serve or lift, so a process re-translated the
        // code its own closure held, and then had two sets of blocks for one
        // address range: a directory naming one and a table holding the
        // other, which is an illegal instruction rather than a clean trap.
        //
        // Before, not after, the ordinary paths. Filling a slot from a module
        // already in this process is strictly cheaper than serving an object
        // or lifting a block, and it is also the only one of the three that
        // cannot introduce a second translation of the same address.
        //
        // Once per page: a page nobody can fill falls through on the next
        // miss to what happened before any of this existed.
        // Ask even when the page already has a base. Having one does not mean
        // having this block: discovery finds the blocks it can reach from
        // where it started, so a page is filled in over many misses, and the
        // gate that skipped a page with a base sent four hundred misses a
        // process straight to the translator. `shared_module_at` refuses a
        // module this Worker has already installed, so asking again after a
        // real translation costs a lookup and nothing else.
        {
            // The first few misses of each process, in full.
            //
            // An exec'd bash lifts 15,307 blocks where pid 1 lifts 6, and
            // `image_is_ours` is never reached after an exec even though
            // `IMAGE_PAGES` holds 245 pages in every process. Either this
            // path does not consult `gex_closure_owner` for these pages, or
            // the misses are not landing where they look. This says which.
            {
                static NTH: std::sync::atomic::AtomicU32 =
                    std::sync::atomic::AtomicU32::new(0);
                if tracing() && NTH.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 6 {
                    let addr = page * gex_abi::DISPATCH_PAGE;
                    let in_images = IMAGE_PAGES.0.borrow().contains_key(&page);
                    let below = addr < gex_abi::INTERP_BASE;
                    warn_line(&format!(
                        "[gex] miss pid={} addr={addr:#x} page={page:#x} \
                         below_interp={below} in_image_pages={in_images} \
                         owner={}\n",
                        h.kernel.pid,
                        gex_closure_owner(page),
                    ));
                }
            }
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
                // Everything anybody has translated of the object this
                // address is in, not the one module that covers this page.
                //
                // Per page did not work and the counters said exactly why:
                // 361 of 381 recognised pages had every covering module
                // already installed, and the block missed anyway. A unit
                // holds what was reachable from the block that faulted in the
                // process that built it, so arriving at the same page by a
                // different route gets nothing from it. Having missed once
                // inside a library a process will miss all over it, and each
                // of those is a module somebody has already built -- so take
                // them all on the first miss and stop coming back.
                // How many units one miss may take, and it matters far more
                // than it looks. Merging makes a batch one module, so the
                // obvious move is to take everything -- and that is ten times
                // slower, measured:
                //
                //     16 per miss      31,747 ms
                //     4096 per miss   300,091 ms
                //
                // on the 21-process shell loop. Compiling one enormous module
                // is worse than compiling several small ones by enough to
                // swamp everything else, so the cap is not a safety valve, it
                // is the setting. Sixteen is measured; the values between have
                // not been.
                // Settable, so it can be swept. Sixteen came from comparing
                // it against 4096 -- every unit of an object merged into one
                // module -- which was 31,747ms against 300,091ms. That ruled
                // out *unbounded*; it did not establish that sixteen is the
                // best bounded value, and the numbers have moved since.
                // Compiling is per *module* far more than per byte: a 6.7 MiB
                // module compiles in 50ms, while a process installing 815
                // small ones spends 1550ms compiling -- about 1.9ms each,
                // nearly all of it fixed overhead.
                //
                // Not an environment variable: `std::env::var` always fails
                // on `wasm32-unknown-unknown`, which is why
                // `GEX_DISPATCH_BALLAST` has never done anything in a page.
                let per_miss = PER_MISS_N.load(std::sync::atomic::Ordering::Relaxed);
                let mut ready = shared_modules_of_object(
                    h,
                    object_identity(h, page * gex_abi::DISPATCH_PAGE).unwrap_or(0),
                );
                // All of it, when the object has been translated whole.
                //
                // The cap exists because a stored unit holds only what was
                // reachable from the block that faulted in the process that
                // built it, so most units do not contain the address this
                // process wants and taking hundreds of them is hundreds of
                // compiles for nothing. That reasoning stops applying the
                // moment the object is complete: everything is covered, so
                // taking the lot ends the misses in that object outright.
                //
                // Measured before this: a process finds what earlier ones
                // published -- `hit=241 miss=0`, 958 modules served -- and
                // still lifts twenty-five thousand blocks, because sixteen at
                // a time is a lottery over a set that mostly does not answer.
                let whole = obj_finished(h, page);
                if !whole {
                    ready.truncate(per_miss);
                }
                if !ready.is_empty() {
                    if tracing() {
                        trace_line(&format!(
                            "[gex] serve {} module(s) of the object at pc={pc:#x}\n",
                            ready.len()
                        ));
                    }
                    // One module out of however many were found. Installed
                    // separately, each is a `WebAssembly.compile` and an
                    // `instantiate`, and that -- not translation -- is what a
                    // process spends its time on once it can identify its own
                    // pages: about seventeen hundred of them, three and a half
                    // seconds, against three hundred milliseconds of lifting.
                    //
                    // The merge copies bodies without reading them and patches
                    // only the immediates the emitter recorded, so the code
                    // that runs afterwards is byte-for-byte what it would have
                    // been. See `gex_emit::merge`.
                    let mut slots = 0u64;
                    let mut dirs: Vec<(u64, u32)> = Vec::new();
                    for (id, s, d, _) in &ready {
                        h.jit.installed_mods.insert(*id);
                        slots = slots.max(*s);
                        // Every page they cover, before the install rather
                        // than after: installing asks the shim which slot each
                        // block belongs in, and the answer comes from here. A
                        // Worker that assigned only the page it faulted on
                        // would place one block and silently drop the rest.
                        for (p, b) in d {
                            h.jit.page_table.assign(*p, *b);
                        }
                        dirs.extend_from_slice(d);
                        h.jit.reuse_served += 1;
                    }
                    // Does any of this answer the block that faulted?
                    //
                    // A unit holds what was reachable from the block that
                    // faulted in the process that built it, so an object's
                    // stored modules mostly do not contain any given address
                    // in it. Serving a batch and going back to the guest was
                    // therefore not progress: the branch missed again, the
                    // next miss took the next batch, and one address cost 306
                    // misses and five thousand modules before the translator
                    // produced the block in eleven milliseconds.
                    //
                    // It matters far more now than when it was first found. A
                    // forked child that defers arrives with an empty
                    // directory *by design*, so every page it touches is a
                    // miss -- and at three hundred misses each, a child made
                    // no measurable progress in two and a half minutes.
                    let covers = dirs.iter().any(|(p, _)| *p == page);
                    let bytes: Vec<&[u8]> = ready.iter().map(|(_, _, _, w)| w.as_slice()).collect();
                    match gex_xlate::merge::merge(&bytes) {
                        Some(wasm) => h.jit.pending.push_back(PendingModule { dirs, slots, wasm }),
                        None => {
                            // Not the shape the merger expects. One at a time
                            // rather than lose the code.
                            for (_, s, d, w) in ready {
                                h.jit.pending.push_back(PendingModule {
                                    dirs: d,
                                    slots: s,
                                    wasm: w,
                                });
                            }
                        }
                    }
                    publish_pending(h);
                    if covers {
                        set_resume(pc);
                        set_resume_ctx(h.threads[cur()].kstate.ctx_addr);
                        return true;
                    }
                    // Otherwise fall through and lift the block in this same
                    // turn. What was served is queued either way, and the
                    // trap at the end of translation installs all of it.
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
                    dir_note(page, base, "miss-shared-slot");
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
        // Published by what this unit *covers*, not by what it happened to
        // allocate. See the note on `TranslatedModule::covered`: handed the
        // allocation list, nine of ten units published nothing at all.
        let mod_id = share_module(&unit.wasm, unit.table_slots, &unit.covered);
        if mod_id == 0 {
            h.jit.store_full += 1;
        } else {
            h.jit.store_ok += 1;
        }
        // How many of this unit's pages could be named, against how many
        // there are.
        //
        // `noident` counts only the *miss* path, so a page that has an
        // identity when it faults and none when its translation is stored is
        // invisible -- and the whole store happens here. One run of two `ls`
        // processes made a single global store between them while the metrics
        // claimed eight per process, so the entries are being dropped
        // somewhere and this is the only place they can go.
        {
            let total = unit.covered.len();
            let named = unit
                .covered
                .iter()
                .filter(|(pg, _)| page_identity(h, *pg * gex_abi::DISPATCH_PAGE).is_some())
                .count();
            static NTH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if tracing() && NTH.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                warn_line(&format!(
                    "[gex] storing a unit: {named} of {total} pages can be named\n"
                ));
            }
        }
        for (page, base) in &unit.covered {
            if let Some(key) = page_identity(h, *page * gex_abi::DISPATCH_PAGE) {
                record_shared_slot(h, *page, key, *base);
                if mod_id != 0 {
                    // SAFETY: a call into the kernel taking no borrow here.
                    unsafe { k_mod_claim(*page, key, mod_id) };
                }
            }
        }
        // Claim against the objects this unit actually emitted code for.
        //
        // `dir_entries` is the wrong list to claim from: it holds only the
        // pages this unit *freshly allocated* a base for, and a unit whose
        // blocks land on pages an earlier unit already based contributes
        // nothing to it. That is most of them -- 445 units stored and about
        // 27 claimed, which is why installing per object bought so little.
        // What the unit emitted is `exported_blocks`, and every one of those
        // is inside an object somebody else will miss in.
        if mod_id != 0 {
            let mut claimed: Vec<u64> = Vec::new();
            for (addr, _) in &unit.exported_blocks {
                if let Some(obj) = object_identity(h, *addr) {
                    if !claimed.contains(&obj) {
                        claimed.push(obj);
                        // SAFETY: a call into the kernel taking no borrow.
                        unsafe { k_mod_claim_object(obj, mod_id) };
                    }
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
        let whose = object_name_at(h, pc);
        note_blocks(&mut h.jit.by_object, whose, pc, stats.blocks as u64);
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
    let (adopted, pages) = with_host(|h| {
        let first = gex_abi::dispatch_page(map.start);
        let last = gex_abi::dispatch_page(map.end.saturating_sub(1));
        let pages = (last - first + 1) as usize;
        let mut missing = 0usize;
        for page in first..=last {
            if h.jit.page_table.lookup(page).is_some() {
                continue;
            }
            let Some(key) = page_identity(h, page * gex_abi::DISPATCH_PAGE) else {
                missing += 1;
                continue;
            };
            // Through `shared_slot`, which asks the *global* kernel before it
            // asks this process's own mirror.
            //
            // Reading `h.jit.shared` directly is why an `apt-get update`
            // translated 3,279,636 blocks -- nearly twice the entire
            // decodable text of every library it touches. The mirror holds
            // only what *this* process stored, so a process that started
            // after another had already translated and published libapt
            // looked, found nothing of its own, and translated it again. The
            // store, the identity map and `k_mod_for_object` all exist to
            // stop exactly that, and this path was the one place that did not
            // consult them. The miss path always has.
            match shared_slot(h, page, key) {
                Some(base) => {
                    h.jit.page_table.assign(page, base);
                    // And the guest's directory, which `execve` cleared.
                    let mut gm = GuestMem::new(guest_bytes());
                    let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, base);
                }
                None => missing += 1,
            }
        }
        (missing, pages)
    });
    if adopted == 0 {
        return 0;
    }
    // Not gated on how much of the object is unclaimed. That was tried --
    // skip unless a quarter of the pages are missing -- on the theory that a
    // few stragglers were dragging whole libraries through the translator.
    // It changed the block count by twenty out of 3,381,964, because the
    // premise was wrong: when a process reaches this, nearly every page of
    // the object really is unclaimed.
    //
    // The reason is that apt's processes run at once, in parallel Workers,
    // and all of them start with an empty store. Twenty-six processes look
    // for libapt, none of them find it, and all of them translate it. The
    // sharing works perfectly and arrives too late to matter. See #93.
    let _ = pages;
    // One process translates an object, not all of them.
    //
    // The store answers "has anybody already done this"; until now nothing
    // answered "is anybody doing it". apt's ten processes run at once in
    // parallel Workers, so all of them reach `libapt` before any has
    // published it, all find nothing, and all translate it -- 3,381,984
    // blocks against the 1,724,482 that are the entire decodable text of
    // every library the run touches.
    //
    // A claim, not a lock. The process that loses does not wait: it returns
    // to the miss path, which lifts the handful of blocks it actually needs
    // and is shared in its own right, and it picks the object up from the
    // store once the winner publishes. Waiting would trade duplicated work
    // for a stall behind whichever process is slowest, which is the same
    // trade the `PER_MISS` cap already refused.
    let (obj, me) = with_host(|h| (object_identity(h, map.start), h.kernel.pid));
    let Some(obj) = obj else {
        return 0;
    };
    if unsafe { k_obj_translating(obj, me as u32) } == 0 {
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
    // Timed, like the block path is.
    //
    // It was not, and the two counters it feeds disagreed by sixty-five
    // times: `blocks` counted what this lifted and `nanos` did not, so a
    // deferred closure run read "2,639,578 blocks in 986ms" -- 2,675 a
    // millisecond against the 41 a millisecond the same machine manages on
    // the block path. That is not a fast translator, it is an untimed one,
    // and the missing time was most of the twenty-eight seconds a deferred
    // closure could not account for.
    let object_started = now_ns();
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
    let object_nanos = now_ns().saturating_sub(object_started);
    let produced = with_host(|h| {
        h.jit.translated = known;
        if modules.iter().all(|m| m.exported_blocks.is_empty()) {
            return 0usize;
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
        {
            let whose = map.path.rsplit('/').next().unwrap_or(&map.path).to_string();
            note_blocks(&mut h.jit.by_object, whose, map.start, stats.blocks as u64);
        }
        h.jit.nanos += object_nanos;
        let produced = modules.len();
        // And what this claimed, so the next process to map the same library
        // finds it rather than translating it again. The miss path does this
        // for what it translates; without the same here, the second process
        // to load libc pays for all 62,085 blocks of it a second time.
        for m in &modules {
            // Covered, not allocated -- the same distinction as the miss path
            // above. This loop's own comment says the cost of getting it
            // wrong: "the second process to load libc pays for all 62,085
            // blocks of it a second time". It was getting it wrong.
            for (page, base) in &m.covered {
                if let Some(key) = page_identity(h, *page * gex_abi::DISPATCH_PAGE) {
                    // And in the global kernel, not only this process's
                    // mirror, or the next process cannot find it either.
                    record_shared_slot(h, *page, key, *base);
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
        produced
    });
    // Released whether or not anything came of it -- a translation that
    // produced nothing must not stop the next process from trying -- and
    // recorded as *done* when it did, so the next process serves the object
    // out of the store instead of lifting it again. That second half is the
    // difference between an `ls` that costs 78,000 blocks and one that costs
    // 23,000.
    unsafe { k_obj_translated(obj, produced as u32) };
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

/// Whether region `i` could be rebuilt by re-reading its file rather than
/// copied.
///
/// A fork copies every mapped region -- 15.4 MiB for a shell -- and most of
/// that is library text the child could map from the same file it came from.
/// `origin` records exactly that and only for a mapping that has never been
/// writable, which is the condition for the bytes still matching the file.
/// Reported before the mechanism is built, so the mechanism is justified.
#[no_mangle]
pub extern "C" fn gex_vma_reloadable(i: u32) -> i32 {
    with_host(|h| {
        let vmas = h.kernel.mem.vmas();
        let Some(v) = vmas.get(i as usize) else {
            return 0;
        };
        let writable = v.prot & gex_kernel::mem::prot::WRITE != 0;
        (!writable && v.origin.is_some()) as i32
    })
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
    let _ = added;
}

/// Narrate the calls that change what is running: `clone` and `execve`.
///
/// On by default, which nothing else about the guest is. A run's shape is its
/// process tree -- who forked, who exec'd what, in which order -- and every
/// interesting failure this host has had was located by reconstructing that:
/// a child crashing and its parent waiting for ever, a method that never
/// started, a fork storm that produced fifty processes and one line of
/// output. Reconstructing it from a full `strace` means reading past every
/// `read` and `mmap` in between, and reconstructing it from nothing at all is
/// what most of those cost their time to.
///
/// Only these two, and only their arguments. `GEX_TRACE` still narrates
/// everything and this stands down when it is on, so the two never double up.
///
/// AArch64 has no `fork`: glibc's `fork` is `clone` with `SIGCHLD` and no
/// `CLONE_THREAD`, so the two are told apart here rather than by number --
/// which is the distinction that matters, because one makes a process and the
/// other makes a thread.
/// Syscalls made from the dynamic linker's text, and when the last one was.
static LOADER_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LOADER_LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many syscalls came from the interpreter, and how long after the
/// process started it made its last one -- roughly, when linking finished.
#[no_mangle]
pub extern "C" fn gex_loader_calls() -> u64 {
    LOADER_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

#[no_mangle]
pub extern "C" fn gex_loader_done_ms() -> u64 {
    let last = LOADER_LAST.load(std::sync::atomic::Ordering::Relaxed);
    let start = PROCESS_STARTED.load(std::sync::atomic::Ordering::Relaxed);
    if last == 0 || start == 0 || last < start {
        return 0;
    }
    (last - start) / 1_000_000
}

/// When this process began, for the measurement above.
static PROCESS_STARTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The path of the most recent `openat`, for the crash report.
static LAST_OPEN: SingleThreaded<String> = SingleThreaded(const { RefCell::new(String::new()) });

/// How many bytes of it there are, written into the shim's own buffer.
#[no_mangle]
pub extern "C" fn gex_last_open(out: u32, max: u32) -> u32 {
    let s = LAST_OPEN.0.borrow();
    let n = s.len().min(max as usize);
    shim_slice_mut(out, n as u32).copy_from_slice(&s.as_bytes()[..n]);
    n as u32
}

fn note_process_syscall(base: u64, pc: i64) {
    use gex_kernel::syscalls::{clone_flags, nr};
    let gm = GuestMem::new(guest_bytes());
    let g = |i: u8| gm.read_u64(base + ctx::x(i) as u64).unwrap_or(0);
    let n = g(8);
    // How much of a process is its dynamic linker.
    //
    // gex loads the image and the interpreter and then jumps to the guest's
    // `ld.so`, which does the actual linking -- search, `openat`, `mmap`,
    // symbol hashing, relocation -- in translated code, once per process.
    // A closure pins every library at build time, so all of that recomputes
    // an answer the build already had. Counting the syscalls it makes, and
    // when it makes its last one, says what that is worth before anything is
    // built to avoid it.
    let pc = pc as u64;
    if pc >= gex_abi::INTERP_BASE && pc < gex_abi::INTERP_BASE + 0x0010_0000 {
        LOADER_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        LOADER_LAST.store(now_ns(), std::sync::atomic::Ordering::Relaxed);
    }
    // The last path this process asked to open, kept for the crash report.
    //
    // A guest that faults inside `openat` says only that it faulted inside
    // `openat`. Which file it was asking for is the whole question, and it is
    // one bounded read of a string the handler is about to read anyway.
    if n == nr::OPENAT {
        if let Ok(b) = gm.read_cstr(g(1), 256) {
            *LAST_OPEN.0.borrow_mut() = String::from_utf8_lossy(&b).into_owned();
        }
    }
    if n != nr::CLONE && n != nr::EXECVE {
        return;
    }
    let cstr = |at: u64, cap: usize| {
        gm.read_cstr(at, cap)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    };
    let pid = with_host(|h| h.kernel.pid);
    let what = if n == nr::EXECVE {
        // The program and how it was called. A trace saying only that an
        // `execve` happened is what made "apt's method does not start"
        // unanswerable for as long as it was: the argv is the difference
        // between the wrong program and the right program with wrong
        // arguments.
        let mut argv = String::new();
        for i in 0..8u64 {
            let p = gm.read_u64(g(1) + i * 8).unwrap_or(0);
            if p == 0 {
                break;
            }
            if i > 0 {
                argv.push(' ');
            }
            argv.push_str(&cstr(p, 64));
        }
        format!("execve \"{}\" [{argv}]", cstr(g(0), 255))
    } else {
        let f = g(0);
        // Named where a name exists and hex otherwise, because the flags are
        // the whole content of a `clone`: the same call number makes a
        // thread, a process, or something sharing half a process's state.
        let mut named = Vec::new();
        for (bit, name) in [
            (clone_flags::VM, "VM"),
            (clone_flags::THREAD, "THREAD"),
            (clone_flags::SETTLS, "SETTLS"),
            (clone_flags::PARENT_SETTID, "PARENT_SETTID"),
            (clone_flags::CHILD_CLEARTID, "CHILD_CLEARTID"),
            (clone_flags::CHILD_SETTID, "CHILD_SETTID"),
        ] {
            if f & bit != 0 {
                named.push(name);
            }
        }
        let kind = if f & clone_flags::THREAD != 0 {
            "clone/thread"
        } else {
            "clone/fork"
        };
        format!(
            "{kind} flags={f:#x} [{}] sig={} stack={:#x} ptid={:#x} tls={:#x} ctid={:#x}",
            named.join("|"),
            f & 0xff,
            g(1),
            g(2),
            g(3),
            g(4),
        )
    };
    trace_line(&format!("[gex:{pid}] t{} pc={pc:#x} {what}\n", cur()));
}

/// Which object's executable mapping a guest address falls in, if any.
/// The short name of whatever object holds this address, for a counter.
///
/// Takes the host it already has, because the caller is inside `with_host`
/// and asking again would be a second borrow of a `RefCell` -- which is a
/// panic, not an error, and the way three crashes this session were reported
/// instead of the ones that caused them.
/// Add `n` blocks lifted at `a` to what `whose` has cost so far.
fn note_blocks(
    m: &mut std::collections::BTreeMap<String, (u64, u64, u64)>,
    whose: String,
    a: u64,
    n: u64,
) {
    let e = m.entry(whose).or_insert((0, u64::MAX, 0));
    e.0 += n;
    e.1 = e.1.min(a);
    e.2 = e.2.max(a);
}

fn object_name_at(h: &Host, a: u64) -> String {
    h.kernel
        .mem
        .vmas()
        .iter()
        .find(|v| a >= v.start && a < v.end)
        .map(|v| {
            // `code`, not `origin`: the code identity is the thing the shared
            // store keys on, and `origin` is deliberately `None` for anything
            // the loader built rather than mapped -- which is every image and
            // every interpreter, so attribution reported `[image]` for all of
            // them and could not tell a program's own text from `ld.so`'s.
            let n = v.code.as_ref().map(|(p, _)| p.as_str()).unwrap_or(&v.name);
            let n = n.rsplit('/').next().unwrap_or(n);
            // A built region is named by where it is as well as what it is:
            // an executable and its interpreter share one path and differ
            // only by address.
            if v.name == "[image]" {
                format!("{n}@{:#x}", v.start)
            } else {
                n.to_string()
            }
        })
        .unwrap_or_else(|| "unmapped".to_string())
}

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
    // The remembered address first, and only then the kernel.
    //
    // This is a *crash reporter's* question, so the case that matters is the
    // one where the kernel is not available: a wasm trap aborts rather than
    // unwinds, so the borrow taken by whatever trapped is still held when JS
    // asks where the guest was. Asking through `with_host` then panics on top
    // of the crash, and the message that reaches the console is "the kernel
    // was re-entered at gex_current_ctx while gex_dispatch_miss holds it" --
    // which describes the reporter and says nothing whatever about the fault.
    // Two rounds of chasing an out-of-bounds access were spent reading that
    // sentence as though it were the bug.
    let at = CTX_ADDR.load(std::sync::atomic::Ordering::Relaxed);
    if at != 0 {
        return at as i64;
    }
    with_host_free(|h| h.threads.get(cur()).map_or(0, |t| t.kstate.ctx_addr as i64)).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn gex_current_pc() -> i64 {
    // Without the kernel when there is a remembered context, because the
    // crash this most needs to describe is one that left the kernel borrowed.
    // See `CTX_ADDR`.
    let at = CTX_ADDR.load(std::sync::atomic::Ordering::Relaxed);
    if at != 0 {
        let gm = GuestMem::new(guest_bytes());
        return gm.read_u64(at + ctx::PC as u64).unwrap_or(0) as i64;
    }
    with_host_free(|h| {
        let gm = GuestMem::new(guest_bytes());
        let at = h.threads.get(cur()).map_or(0, |t| t.kstate.ctx_addr);
        gm.read_u64(at + ctx::PC as u64).unwrap_or(0) as i64
    })
    .unwrap_or(0)
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

/// The highest slot total any queued module needs.
///
/// So the host can grow its dispatch tables **once** for a batch instead of
/// once per module. `Table.grow` reallocates, so growing four hundred times
/// by a little is quadratic in the table's size: serving a whole object took
/// table growth from 317ms to 1741ms a process and gave back more than the
/// compile it saved.
#[no_mangle]
pub extern "C" fn gex_jit_pending_slots() -> u32 {
    with_host(|h| h.jit.pending.iter().map(|p| p.slots).max().unwrap_or(0) as u32)
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
            dir_note(page, base, "install-done");
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
        // Counted for the arena report, which cannot borrow the host: it
        // runs inside the allocator's refill path. `registry` keeps every
        // module's bytes for the life of the process so a joining Worker can
        // instantiate them, and nothing trims it -- the first thing to rule
        // in or out for the 1,861 MiB an `apt-get update` holds live. See
        // #140.
        REGISTRY_BYTES.fetch_add(done.wasm.len() as u64, std::sync::atomic::Ordering::Relaxed);
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
        // `x16` and `x17` as well, because a PLT stub is where indirect
        // branches actually come from and those are the two registers it
        // uses. The whole of `libc+0x27200` is
        //
        //     stp x16, x30, [sp, #-0x10]!
        //     adrp x16, <got>
        //     ldr  x17, [x16, #0xff8]
        //     br   x17
        //
        // so a dispatch that traps there is dispatching on whatever the GOT
        // held, not on the page the pc is in -- which is why a report built
        // from `dispatch_page(pc)` kept describing an entry that was
        // perfectly in range while the branch was somewhere else entirely.
        for (name, a) in [
            ("sp", sp),
            ("x0", r(0)),
            ("x1", r(1)),
            ("x16", r(16)),
            ("x17", r(17)),
        ] {
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
/// Record that this process is gone, for a process that did not say so.
///
/// `exit_process` publishes the exit status and releases the descriptors, and
/// it is exactly the path a crashed guest does not take: a wasm fault
/// abandons the stack without running anything. So `dpkg` faulted inside
/// `openat`, and *both* halves of an `apt-get update` deadlock followed from
/// the same omission -- `apt` waited on a `read` for an end-of-file its dead
/// child's pipe end was still holding shut, and pid 1 waited on `wait4` for a
/// status nothing had published.
///
/// Called from the Worker after `run` returns, where the shim instance is
/// still alive and can still reach the global kernel. A process that crashed
/// should take itself down and nothing else with it, which is what a real
/// kernel arranges and the only reason a guest is allowed to crash at all.
///
/// `status` is what the parent will see. A fault is not an exit code, so it
/// is reported as a signal the way a real one would be.
#[no_mangle]
pub extern "C" fn gex_process_gone(status: i32) -> u32 {
    if EXITED.load(std::sync::atomic::Ordering::Relaxed) {
        return 0;
    }
    EXITED.store(true, std::sync::atomic::Ordering::Relaxed);
    let pid = my_pid();
    let n = unsafe {
        if k_present() != 0 {
            k_pipe_forget(pid)
        } else {
            0
        }
    };
    with_host(|h| h.kernel.procs.exit(pid, status));
    warn_line(&format!(
        "[gex] pid {pid} crashed; {n} pipe ends released, status {status}\n"
    ));
    n
}

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
    // What the queue is holding, for the arena report -- which runs inside
    // the allocator's refill path and can neither allocate nor borrow the
    // host, so it has to be a static and this is the one place with `h` that
    // every push and the pop both pass through.
    //
    // `publish_pending` shows the host only the *front* of the queue. If the
    // host installs more slowly than the guest queues, or stops, the rest
    // piles up holding whole module images. An `apt-get update` fills a 2 GiB
    // arena with ~23 MiB objects it never frees, which is that shape exactly.
    // O(n) per publish is fine for a diagnostic, and if n is large that is
    // the finding. See #140.
    PENDING_BYTES.store(
        h.jit.pending.iter().map(|m| m.wasm.len() as u64).sum(),
        std::sync::atomic::Ordering::Relaxed,
    );
    PENDING_MAX.fetch_max(
        h.jit.pending.len() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
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
/// Blocks lifted, by object, as `name=count name=count`, into `out`.
///
/// The total says a process lifted 4,308 blocks; this says which objects they
/// came from, which is the difference between a closure that is missing
/// something and a program that genuinely has its own code to translate.
#[no_mangle]
pub extern "C" fn gex_jit_by_object(out: u32, max: u32) -> u32 {
    let text = with_host(|h| {
        let mut v: Vec<(&String, &(u64, u64, u64))> = h.jit.by_object.iter().collect();
        v.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
        v.iter()
            .take(8)
            .map(|(k, (n, lo, hi))| format!("{k}={n}@{lo:#x}..{hi:#x}"))
            .collect::<Vec<_>>()
            .join(" ")
    });
    let n = text.len().min(max as usize);
    shim_slice_mut(out, n as u32).copy_from_slice(&text.as_bytes()[..n]);
    n as u32
}

/// Why misses reached the translator. See the note at the increment.
static LIFT_WHY: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// The first few addresses that translated despite their span being filled.
static LIFT_PCS: [std::sync::atomic::AtomicU64; 16] = [
    std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0),
];
static LIFT_PC_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The dispatch base each of those pages had at the moment it was lifted.
static LIFT_BASES: [std::sync::atomic::AtomicU32; 16] = [
    std::sync::atomic::AtomicU32::new(0), std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0), std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0), std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0), std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0), std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0), std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0), std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0), std::sync::atomic::AtomicU32::new(0),
];

#[no_mangle]
pub extern "C" fn gex_lift_base(i: u32) -> u32 {
    LIFT_BASES
        .get(i as usize)
        .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn gex_lift_pc(i: u32) -> u64 {
    LIFT_PCS
        .get(i as usize)
        .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn gex_lift_why(i: u32) -> i64 {
    LIFT_WHY
        .get(i as usize)
        .map(|c| c.load(std::sync::atomic::Ordering::Relaxed) as i64)
        .unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn gex_jit_blocks() -> i64 {
    with_host(|h| h.jit.blocks as i64)
}

#[no_mangle]
pub extern "C" fn gex_jit_compiles() -> i64 {
    with_host(|h| h.jit.compiles as i64)
}

/// Blocks lifted on a page a closure library already claims.
///
/// Counted since the demand-fill path went in and never reported, which left
/// the only interesting question about a closure run unanswerable from a
/// summary: a process that lifts 78,938 blocks of a library it holds whole is
/// either failing to ask for the fill or asking and being told no, and those
/// have different fixes. Every one of these is a block the static
/// recompilation already produced and the run paid for again.
#[no_mangle]
pub extern "C" fn gex_lift_in_closure() -> i64 {
    with_host(|h| h.jit.lift_in_closure as i64)
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
            5 => h.jit.store_ok,
            6 => h.jit.store_full,
            7 => SERVE_NONE.with(|c| c.borrow().0),
            8 => SERVE_NONE.with(|c| c.borrow().1),
            9 => NOIDENT_WHY.with(|c| c.borrow().0),
            10 => NOIDENT_WHY.with(|c| c.borrow().1),
            _ => h.jit.reuse_noident,
        }) as i64
    })
}

/// Milliseconds the shim itself spent translating.
#[no_mangle]
pub extern "C" fn gex_jit_millis() -> i64 {
    with_host(|h| (h.jit.nanos / 1_000_000) as i64)
}
