//! The browser host: the same `gex-kernel`, compiled to wasm and run beside
//! the guest module.
//!
//! The shim imports the guest module's memory -- the same 4 GiB -- and puts its
//! own heap and stack in the hole above the guest's `STACK_TOP` (see
//! `build.sh`), so a syscall reads the guest's buffers in place with no copy.
//!
//! Each process is its own Worker with its own memory; threads within a process
//! share it; state shared between processes lives in a separate kernel wasm
//! module (`shims/kernel`) instantiated against one shared memory.
//!
//! JS creates the memory, instantiates both modules against it, wires this
//! module's exports to the guest's `gex.*` imports, and supplies a console and
//! a clock.

// Only meaningful as wasm: everything below assumes it shares a linear memory
// with the guest module beside it. On any other target the crate is empty, so
// `cargo build --workspace` on the development host still works.
#![cfg(target_family = "wasm")]

// `memory.atomic.wait32` and `memory.atomic.notify`, which the kernel lock in
// the shared build is built on. Unstable in `core::arch::wasm32` and nowhere
// else: they are wasm instructions, and the shared build is a nightly build
// already because a module importing a shared memory has to be.
#![cfg_attr(feature = "shared-memory", feature(stdarch_wasm_atomic_wait))]

#[cfg(target_arch = "wasm32")]
use core::arch::wasm32 as warch;
// The memory64 build (docs/mem64.md step 6): an x86-64 guest with a 64-bit
// memory, for the Unity games on .NET (docs/unity-dotnet.md P5).
#[cfg(target_arch = "wasm64")]
use core::arch::wasm64 as warch;

use gex_abi::ctx;
use gex_kernel::errno::{Errno, SysResult};
use gex_kernel::file::OpenFile;
use gex_kernel::vfs::{FileHandle, Metadata};
use gex_kernel::net;
use gex_kernel::{GuestMem, Kernel, SyscallOutcome, ThreadState};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
pub mod gl;
pub mod wl;
mod xwm;
pub(crate) mod memmap;
mod cgen;
mod dense;
mod fjit;
mod waitq;
mod sigs;
mod vfork;
// The NT personality's host side: a Windows program (docs/nt-layer.md §10.2).
mod nt;

use std::sync::{Arc, Mutex};

// The two things wasm cannot do for itself. Everything else about a syscall
// happens in this module.
#[link(wasm_import_module = "gex_env")]
extern "C" {
    /// Install, synchronously on this Worker, what a dispatch miss just
    /// queued or found missing here (the pending translations, the registry,
    /// the dense block a slot names): 1 when it could, and the miss stub
    /// dispatches again in place; 0 (a page's main thread cannot compile
    /// synchronously) and the miss traps out as before.
    fn jit_install(ctx: i32) -> i32;
    /// Write `len` bytes at `ptr` -- an address in the shared memory -- to the
    /// host's console for `fd`. A pointer-sized address: the wasm64 shim's
    /// buffers (a thread's stack among them) can be above 4 GiB.
    fn write(fd: i32, ptr: usize, len: u32);
    /// The shim's own narration, to the host's debug console -- not fd 2, so a
    /// trace never interleaves with the guest's own stderr.
    fn debug(ptr: u32, len: u32);
    /// The same channel at error level, for what a person must see whatever
    /// else is switched off.
    fn warn(ptr: u32, len: u32);
    /// Nanoseconds since an arbitrary epoch.
    fn now() -> i64;
    /// `performance.now()`, in milliseconds: the clock's hot path. An f64,
    /// not an i64: an i64 import result is a BigInt per call, most of a
    /// clock read's cost (Chrome: about 650 ns of a 757 ns
    /// QueryPerformanceCounter, docs/wine-overhead.md F2).
    fn perf_now() -> f64;
    /// The epoch `now` counts from in this Worker, in ns (`ORIGIN_NS`):
    /// `now() == clock_origin() + round(perf_now() * 1e6)`. Read once per
    /// thread.
    fn clock_origin() -> i64;
    /// Block until the host has delivered every `write` so far. Called just
    /// before this process's exit is published: output from different
    /// processes reaches the host by different routes, so without it a parent
    /// that waited for this process could print before its last output.
    fn drain_output();

    // The network. A page cannot open a socket, so each of these asks JS, which
    // has a Wisp connection to a proxy. All non-blocking: only the kernel can
    // park a guest thread, and JS blocking here would take the Worker's wasm
    // stack with it. A stream is a small integer JS hands out; zero is never a
    // stream, so it can mean failure.
    fn net_open() -> i32;
    fn net_connect(id: i32, host: u32, host_len: u32, port: i32);
    /// 0 connecting, 1 open, 2 closed, 3 refused.
    fn net_status(id: i32) -> i32;
    /// Bytes taken, or -1.
    fn net_send(id: i32, ptr: u32, len: u32) -> i32;
    /// Bytes delivered, 0 at end of stream, -1 when nothing has arrived yet.
    fn net_recv(id: i32, ptr: u32, len: u32) -> i32;
    /// `net_recv`, leaving the bytes queued (`recv(MSG_PEEK)`).
    fn net_peek(id: i32, ptr: u32, len: u32) -> i32;
    /// Whether `net_recv` would answer without waiting.
    fn net_avail(id: i32) -> i32;
    fn net_close(id: i32);
    /// One more holder of a stream (a socket another process now holds too):
    /// `net_close` then ends it only at the last. 1 if it exists, else 0.
    fn net_dup(id: i32) -> i32;
}

/// Whether the page gave the shim a network at all.
static HAVE_NET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn have_net() -> bool {
    HAVE_NET.load(std::sync::atomic::Ordering::Acquire)
}

/// The page's network, when it has one: one stack for every process in this
/// memory (`WispNet` holds nothing of its own). Read by each socket call
/// (`sigs`'s `NetIo`) and by a kernel as it is built, so a network the page
/// turns on later reaches every process at its next call, with no kernel to
/// tell.
pub(crate) fn page_net() -> Option<&'static Arc<dyn net::NetStack>> {
    static WISP: std::sync::LazyLock<Arc<dyn net::NetStack>> = std::sync::LazyLock::new(|| Arc::new(WispNet));
    have_net().then(|| &*WISP)
}

/// Say that the page has a Wisp connection behind the `net_*` imports.
///
/// Off by default: a page with no proxy configured should have its guest see
/// a machine with no network, which fails cleanly, rather than one whose
/// every connection hangs.
#[no_mangle]
pub extern "C" fn gex_set_network(on: i32) {
    HAVE_NET.store(on != 0, std::sync::atomic::Ordering::Release);
}

/// One Wisp stream, as the kernel's [`net::Transport`].
#[derive(Debug)]
///
/// Its id is atomic: a parked thread's idle turn asks `has_data` without the
/// kernel lock (`waitq`).
struct WispStream {
    id: std::sync::atomic::AtomicI32,
}

impl net::Transport for WispStream {
    fn connect(&self, host: &str, port: u16) -> Result<(), Errno> {
        let id = unsafe { net_open() };
        if id <= 0 {
            return Err(Errno::ENETUNREACH);
        }
        self.id.store(id, std::sync::atomic::Ordering::Release);
        unsafe { net_connect(id, host.as_ptr() as u32, host.len() as u32, port as i32) };
        Ok(())
    }

    fn status(&self) -> net::Status {
        match self.id.load(std::sync::atomic::Ordering::Acquire) {
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
        let id = self.id.load(std::sync::atomic::Ordering::Acquire);
        if id == 0 {
            return Err(Errno::ENOTCONN);
        }
        match unsafe { net_send(id, data.as_ptr() as u32, data.len() as u32) } {
            n if n >= 0 => Ok(n as usize),
            _ => Err(Errno::EPIPE),
        }
    }

    fn recv(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let id = self.id.load(std::sync::atomic::Ordering::Acquire);
        if id == 0 {
            return Ok(0);
        }
        match unsafe { net_recv(id, buf.as_mut_ptr() as u32, buf.len() as u32) } {
            -1 => Err(Errno::EAGAIN),
            n if n >= 0 => Ok(n as usize),
            _ => Err(Errno::ECONNRESET),
        }
    }

    /// Wine's wineserver peeks one byte of every stream socket it sees
    /// readable, to tell data from the far end having closed: without this
    /// it took the socket for broken and every TLS handshake over Wisp
    /// failed at its first read.
    fn peek(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let id = self.id.load(std::sync::atomic::Ordering::Acquire);
        if id == 0 {
            return Ok(0);
        }
        match unsafe { net_peek(id, buf.as_mut_ptr() as u32, buf.len() as u32) } {
            -1 => Err(Errno::EAGAIN),
            n if n >= 0 => Ok(n as usize),
            _ => Err(Errno::EOPNOTSUPP),
        }
    }

    fn has_data(&self) -> bool {
        let id = self.id.load(std::sync::atomic::Ordering::Acquire);
        id != 0 && unsafe { net_avail(id) } != 0
    }

    fn close(&self) {
        let id = self.id.swap(0, std::sync::atomic::Ordering::AcqRel);
        if id != 0 {
            unsafe { net_close(id) };
        }
    }

    /// The page's stream ids are the page's, so another process's socket can
    /// stand on the same stream: it is held once more (`net_dup`), and ends
    /// when its last holder closes.
    fn carry(&self) -> Option<String> {
        let id = self.id.load(std::sync::atomic::Ordering::Acquire);
        (id != 0 && unsafe { net_dup(id) } == 1).then(|| format!("wisp:{id}"))
    }
}

/// Where the browser's connections come from.
#[derive(Debug)]
struct WispNet;

unsafe impl Send for WispNet {}
unsafe impl Sync for WispNet {}

impl net::NetStack for WispNet {
    fn tcp(&self) -> Result<Arc<dyn net::Transport>, Errno> {
        Ok(Arc::new(WispStream { id: std::sync::atomic::AtomicI32::new(0) }))
    }
}

// The global kernel: the process table, in a module of its own.
//
// Every process is a Worker with its own memory, so they cannot share a
// `ProcTable` the way threads do (an `Arc` is an address, and an address means
// a memory). It lives in a second wasm module (`shims/kernel`) every Worker
// instantiates against one shared memory, reached by calling these imports.
// With no global kernel, `k_present` is zero and the rest are stubs: one
// process with its own table, as before.
#[link(wasm_import_module = "gex_k")]
extern "C" {
    /// Is there a global kernel behind these calls at all?
    fn k_present() -> i32;
    /// The machine's synthetic DNS answers (`gex_kernel::net::share_names`):
    /// the address for a name, allocated on first asking; and the name an
    /// address was given for, into `out` (its length, or -1 for none).
    fn k_dns_addr(name: gex_kernel::PAddr, name_len: u32) -> u32;
    fn k_dns_name(addr: u32, out: gex_kernel::PAddr, cap: u32) -> i32;
    /// Wait slots and wait queues (`shims/kernel/src/wait.rs`, `waitq`):
    /// this Worker's slot, its value, a park on it; a queue's waiters added,
    /// removed, notified; queues made, freed and chained for `gex_kernel::wait`.
    fn k_ws_self() -> i32;
    fn k_ws_seq(slot: i32) -> i32;
    fn k_ws_park(slot: i32, seq: i32, ns: i64) -> i32;
    fn k_ws_addr(slot: i32) -> u32;
    fn k_wq_new() -> u32;
    fn k_wq_free(id: u32);
    fn k_wq_notify(id: u32);
    fn k_wq_chain(from: u32, to: u32);
    fn k_wq_unchain(from: u32, to: u32);
    fn k_wq_add(id: u32, slot: u32);
    fn k_wq_del(id: u32, slot: u32);
    fn k_wq_addr(id: u32) -> u32;
    /// The queue of a kernel object, by its id in the kernel's tables.
    fn k_pipe_queue(id: i32) -> u32;
    fn k_sock_queue(id: i32) -> u32;
    fn k_pty_queue(id: i32) -> u32;
    fn k_proc_queue(pid: i32) -> u32;
    fn k_stdin_queue() -> u32;
    fn k_futex_queue(addr: u64) -> u32;
    /// Precise cross-Worker futex wake: `k_futex_wake` bumps the shared
    /// generation of `addr` (and wakes parked Workers), `k_futex_gen` reads it.
    /// A waiter samples the generation when it parks and wakes when it moves --
    /// exact, where re-reading the guest word only guesses.
    fn k_futex_wake(addr: u64);
    fn k_futex_gen(addr: u64) -> u32;
    fn k_spawn(ppid: i32) -> i32;
    fn k_tid_new(pid: i32) -> i32;
    fn k_tid_end(tid: i32);
    fn k_task_owner(id: i32) -> i32;
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
    fn k_take_kill_signals(pid: i32, taken: u64) -> u64;
    fn k_kill_sender(pid: i32, signo: u32) -> i32;
    fn k_has_signals(pid: i32) -> i32;
    fn k_tkill(caller: i32, pid: i32, tid: i32, signo: u32) -> i32;
    fn k_take_thread_signal(pid: i32, tid: i32) -> i64;
    fn k_ppid(pid: i32) -> i32;
    fn k_pgid(caller: i32, of: i32) -> i32;
    fn k_sid(caller: i32, of: i32) -> i32;
    fn k_setsid(pid: i32) -> i32;
    fn k_setpgid(caller: i32, target: i32, pgid: i32) -> i32;
    fn k_request_halt(code: i32);
    fn k_halt_code() -> i32;
    fn k_proc_set_info(pid: i32, at: gex_kernel::PAddr, len: u32);
    fn k_proc_snapshot(at: gex_kernel::PAddr, cap: u32) -> i32;

    // The filesystem. Every address below is in *this* process's memory; the
    // other side copies across with `memory.copy`, which is the only way it
    // can reach a memory it does not own. See `gex_kernel::vfs::remote` for
    // the two layouts that cross.
    fn k_fs_open(path: gex_kernel::PAddr, path_len: u32, flags: u64, mode: u32) -> i32;
    fn k_fs_close(handle: i32);
    fn k_fs_dup(handle: i32) -> i32;
    fn k_fs_read(handle: i32, offset: u64, at: gex_kernel::PAddr, len: u32) -> i32;
    fn k_fs_write(handle: i32, offset: u64, at: gex_kernel::PAddr, len: u32) -> i32;
    fn k_fs_write_now(handle: i32, offset: u64, at: gex_kernel::PAddr, len: u32, now: i64) -> i32;
    fn k_fs_open_now(path: gex_kernel::PAddr, path_len: u32, flags: u64, mode: u32, now: i64) -> i32;
    fn k_fs_fchmod(handle: i32, mode: u32) -> i32;
    fn k_fs_meta(handle: i32, at: gex_kernel::PAddr) -> i32;
    fn k_fs_truncate(handle: i32, len: u64) -> i32;
    fn k_fs_advise(handle: i32, offset: u64, len: u64, advice: u32) -> i32;
    fn k_fs_read_dir(handle: i32, at: gex_kernel::PAddr, cap: u32) -> i32;
    fn k_fs_path_meta(path: gex_kernel::PAddr, path_len: u32, at: gex_kernel::PAddr) -> i32;
    fn k_fs_read_link(path: gex_kernel::PAddr, path_len: u32, at: gex_kernel::PAddr, cap: u32) -> i32;
    fn k_fs_walk(path: gex_kernel::PAddr, path_len: u32, mounts: gex_kernel::PAddr, mounts_len: u32, follow: u32, at: gex_kernel::PAddr, cap: u32) -> i32;
    fn k_fs_unlink(path: gex_kernel::PAddr, path_len: u32) -> i32;
    fn k_fs_mkdir(path: gex_kernel::PAddr, path_len: u32, mode: u32) -> i32;
    fn k_fs_rmdir(path: gex_kernel::PAddr, path_len: u32) -> i32;
    fn k_fs_rename(from: gex_kernel::PAddr, from_len: u32, to: gex_kernel::PAddr, to_len: u32, noreplace: u32) -> i32;
    fn k_fs_moved(handle: i32, at: gex_kernel::PAddr, cap: u32) -> i32;
    fn k_fs_hold_renames(handle: i32, owner: i32, on: u32) -> i32;
    fn k_fs_rename_epoch() -> i32;
    fn k_fs_chmod(path: gex_kernel::PAddr, path_len: u32, mode: u32) -> i32;
    fn k_fs_set_mtime(path: gex_kernel::PAddr, path_len: u32, secs: i64) -> i32;
    fn k_fs_touch(handle: i32, secs: i64) -> i32;
    fn k_fs_set_times(path: gex_kernel::PAddr, path_len: u32, a_s: i64, a_ns: u32, m_s: i64, m_ns: u32, now: i64) -> i32;
    fn k_fs_set_times_fd(handle: i32, a_s: i64, a_ns: u32, m_s: i64, m_ns: u32, now: i64) -> i32;
    fn k_fs_symlink(target: gex_kernel::PAddr, target_len: u32, path: gex_kernel::PAddr, path_len: u32) -> i32;
    fn k_fs_link(from: gex_kernel::PAddr, from_len: u32, to: gex_kernel::PAddr, to_len: u32) -> i32;
    fn k_fs_mksocket(path: gex_kernel::PAddr, path_len: u32) -> i32;
    fn k_fs_add(path: gex_kernel::PAddr, path_len: u32, at: gex_kernel::PAddr, len: u32) -> i32;
    fn k_fs_add_lazy(path: gex_kernel::PAddr, path_len: u32, url: gex_kernel::PAddr, url_len: u32, size: u64) -> i32;
    /// The system-wide map from a page of a mapped object to the dispatch slots
    /// its code was translated into; zero means nobody has. In the kernel so
    /// the second process to run `ls` finds what the first one translated.
    fn k_slot_get(page: u64, key: u64) -> u32;
    fn k_slot_put(page: u64, key: u64, base: u32);
    fn k_slot_len() -> u32;
    /// Standard input, shared. Whoever has a keystroke writes it here and
    /// wakes a parked reader, rather than posting a message to a Worker that
    /// is asleep inside wasm and cannot receive one.
    fn k_stdin_write(at: gex_kernel::PAddr, len: u32) -> i32;
    fn k_stdin_read(at: gex_kernel::PAddr, len: u32) -> i32;
    fn k_stdin_ready() -> i32;
    fn k_stdin_eof();
    /// The terminal: one `struct termios`, one size, one foreground process
    /// group and one line discipline, for every process at once. The page
    /// pushes keystrokes straight into it; these are the guest's half.
    fn k_tty_get_termios(at: gex_kernel::PAddr, len: u32) -> i32;
    fn k_tty_set_termios(at: gex_kernel::PAddr, len: u32) -> i32;
    fn k_tty_get_locked(at: gex_kernel::PAddr, len: u32) -> i32;
    fn k_tty_set_locked(at: gex_kernel::PAddr, len: u32) -> i32;
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
    fn k_tty_push(at: gex_kernel::PAddr, len: u32) -> i32;
    /// Slots nobody else will be given, and the mark that keeps runtime
    /// allocation clear of what static recompilation already assigned.
    fn k_slot_alloc(count: u64) -> u64;
    fn k_slot_floor(floor: u64);
    /// How many slots the mark has left before it runs off the last dispatch
    /// table. Nothing gives slots back, so this only falls; see `k_slot_left`.
    fn k_slot_left() -> u64;
    /// Translated modules, once for the whole system: a dispatch table belongs
    /// to a module instance, so the module bytes themselves must be shareable.
    fn k_mod_store(ptr: gex_kernel::PAddr, len: u32, slots: u64, dirs_ptr: gex_kernel::PAddr, dirs_len: u32) -> u32;
    fn k_mod_claim(page: u64, key: u64, id: u32);
    fn k_mod_for(page: u64, key: u64, n: u32) -> u32;
    fn k_mod_claim_object(obj: u64, id: u32);
    fn k_obj_translating(obj: u64, pid: u32) -> u32;
    fn k_obj_translated(obj: u64, produced: u32);
    /// The translation cache across page loads (docs/gexcc.md §8).
    fn k_key_path(key: u64, path: gex_kernel::PAddr, len: u32, size: u64, mtime: i64) -> i32;
    /// A wasm32 program's content key by its file, system-wide
    /// (`cgen/wasmprog.rs` `known_key`): 1 and the key's two lanes at `out`, or 0.
    fn k_progkey_get(path: gex_kernel::PAddr, path_len: u32, ino: u64, mtime: i64, size: u64, len: u32, out: gex_kernel::PAddr) -> i32;
    fn k_progkey_put(path: gex_kernel::PAddr, path_len: u32, ino: u64, mtime: i64, size: u64, len: u32, a: u64, c: u64);
    fn k_xcache_on() -> u32;
    fn k_store_restore() -> i32;
    fn k_obj_is_translated(obj: u64) -> u32;
    fn k_mod_for_object(obj: u64, n: u32) -> u32;
    fn k_mod_ids_for_object(obj: u64, from: u32, out: gex_kernel::PAddr, cap: u32) -> u32;
    fn k_mod_slots(id: u32) -> u64;
    fn k_mod_len(id: u32) -> u32;
    fn k_mod_dirs_len(id: u32) -> u32;
    fn k_mod_read(id: u32, out: gex_kernel::PAddr, cap: u32) -> u32;
    fn k_mod_dirs_read(id: u32, out: gex_kernel::PAddr, cap: u32) -> u32;
    /// Has some process already filled the shared root? Only the first
    /// should: the rest would unpack the same rootfs over the top of it.
    fn k_place(key: u64, len: u64) -> u64;
    fn k_place_set(key: u64, base: u64, len: u64);
    /// The linked address space of a program, kept so the next process of it
    /// does not have to run the dynamic linker again.
    fn k_snap_store(key: u64, ptr: gex_kernel::PAddr, len: u32);
    fn k_snap_len(key: u64) -> u32;
    fn k_snap_get(key: u64, at: gex_kernel::PAddr, cap: u32) -> u32;
    fn k_fs_populated() -> i32;
    fn k_fs_populate_done();

    // Pipes. A shell pipeline is two processes and a pipe, and apt is a
    // process and four method workers on pairs of them; with a Worker per
    // process the other end is always in another memory.
    fn k_pipe_new(pid: i32) -> i32;
    fn k_pipe_open_end(id: i32, write: i32, pid: i32);
    fn k_pipe_ends(id: i32) -> i32;
    fn k_pipe_write_stats(id: i32) -> i32;
    fn k_pipe_writes(id: i32) -> i32;
    fn k_pipe_last_message(id: i32, at: gex_kernel::PAddr, cap: u32) -> i32;
    fn k_pipe_close_end(id: i32, write: i32, pid: i32);
    fn k_pipe_forget(pid: i32) -> u32;
    fn k_pipe_read(id: i32, at: gex_kernel::PAddr, len: u32) -> i32;
    fn k_pipe_write(id: i32, at: gex_kernel::PAddr, len: u32) -> i32;
    fn k_pipe_readable(id: i32) -> i32;
    fn k_pipe_writable(id: i32) -> i32;
    fn k_pipe_buffered(id: i32) -> i32;

    // Unix98 pseudoterminals. xterm holds the master, its shell the slave, and
    // with a Worker per process those are two memories -- so the pair, and the
    // terminal's line discipline, live in the global kernel like a pipe does.
    fn k_pty_open_master(pid: i32) -> i32;
    fn k_pty_open_slave(ptn: i32, pid: i32) -> i32;
    fn k_pty_open_end(id: i32, master: i32, pid: i32);
    fn k_pty_close_end(id: i32, master: i32, pid: i32);
    fn k_pty_forget(pid: i32) -> u32;
    fn k_pty_read(id: i32, master: i32, at: gex_kernel::PAddr, len: u32) -> i32;
    fn k_pty_write(id: i32, master: i32, at: gex_kernel::PAddr, len: u32) -> i32;
    fn k_pty_readable(id: i32, master: i32) -> i32;
    fn k_pty_counts(id: i32) -> u32;
    fn k_pty_for_sid(sid: i32) -> i32;
    fn k_last_kill() -> u64;
    fn k_last_kill_seq() -> u32;
    fn k_pty_available(id: i32, master: i32) -> i32;
    fn k_pty_ptn(id: i32) -> i32;
    fn k_pty_get_lock(id: i32) -> i32;
    fn k_pty_set_lock(id: i32, locked: i32);
    fn k_pty_get_termios(id: i32, at: gex_kernel::PAddr, len: u32) -> i32;
    fn k_pty_set_termios(id: i32, at: gex_kernel::PAddr, len: u32, flush: i32) -> i32;
    fn k_pty_winsize(id: i32) -> u64;
    fn k_pty_set_winsize(id: i32, packed: u64) -> i32;
    fn k_pty_fg(id: i32) -> i32;
    fn k_pty_set_fg(id: i32, pgrp: i32);
    fn k_pty_sid(id: i32) -> i32;
    fn k_pty_set_sid(id: i32, sid: i32);
    fn k_pty_flush(id: i32, which: i32);

    // Unix sockets. An X server binds a name and its client connects to it,
    // and with a Worker per process those are two memories -- so the name has
    // to be somewhere neither of them owns.
    fn k_sock_bind(path: gex_kernel::PAddr, path_len: u32) -> i32;
    fn k_sock_connect(path: gex_kernel::PAddr, path_len: u32) -> i32;
    fn k_sock_accept(listener: i32) -> i32;
    fn k_sock_pending(listener: i32) -> i32;
    fn k_sock_read(id: i32, at: gex_kernel::PAddr, len: u32) -> i32;
    fn k_sock_write(id: i32, at: gex_kernel::PAddr, len: u32) -> i32;
    fn k_sock_readable(id: i32) -> i32;
    fn k_sock_hup(id: i32) -> i32;
    fn k_sock_writable(id: i32) -> i32;
    fn k_sock_available(id: i32) -> i32;
    fn k_sock_close(id: i32);
    fn k_sock_shutdown(id: i32, how: u32) -> i32;
    fn k_sock_exists(path: gex_kernel::PAddr, path_len: u32) -> i32;
    fn k_sock_pair(out: gex_kernel::PAddr) -> i32;
    fn k_sock_dup(id: i32) -> i32;
    fn k_boot_secs(want: u32) -> u32;
    fn k_guest_clock(scale_milli: u32, wall_shift: i32) -> u64;
    fn k_sock_write_fds(id: i32, at: gex_kernel::PAddr, len: u32, descs: gex_kernel::PAddr, descs_len: u32) -> i32;
    #[allow(clippy::too_many_arguments)]
    fn k_sock_read_fds(
        id: i32,
        at: gex_kernel::PAddr,
        len: u32,
        max_fds: u32,
        peek: i32,
        pid: i32,
        out: gex_kernel::PAddr,
        out_cap: u32,
    ) -> i32;

}

/// The first slot number no dispatch table can hold: `DISPATCH_TABLES` tables
/// of whole pages each. The emitter's divisor, so the two agree on where the
/// space ends.
fn slot_space_top() -> u64 {
    let per_table =
        (gex_abi::DISPATCH_TABLE_MAX_SLOTS / gex_abi::SLOTS_PER_PAGE) * gex_abi::SLOTS_PER_PAGE;
    gex_abi::DISPATCH_TABLES as u64 * per_table
}

/// A page table whose runtime allocations come from the system-wide counter.
///
/// Statically recompiled slots stay as built (same code, same numbers in every
/// process); the mark is raised above them so nothing translated at runtime
/// lands there; everything after is drawn in kernel-issued chunks, so one
/// process can install a module another translated without slot collisions.
/// The code identity of every non-writable mapping with one, as `(start, end,
/// hasher after the object id, file offset)`: what `page_identity` computes,
/// prepared so the page table's identity hooks -- which run inside a
/// translation, with the host borrowed -- can finish the key for a page without
/// the host. Refreshed before each translation (`refresh_ident_snapshot`).
static IDENT_SNAPSHOT: SingleThreaded<Vec<(u64, u64, std::collections::hash_map::DefaultHasher, u64, String)>> =
    SingleThreaded(RefCell::new(Vec::new()));

fn refresh_ident_snapshot(p: &Px<'_>) {
    use std::hash::Hash;
    let mut snap = Vec::new();
    for v in p.mm.lock().vmas().iter() {
        if v.prot & gex_kernel::mem::prot::WRITE != 0 {
            continue;
        }
        let Some((id, off)) = v.code.as_ref() else { continue };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut hasher);
        snap.push((v.start, v.end, hasher, *off, id.clone()));
    }
    *IDENT_SNAPSHOT.0.borrow_mut() = snap;
}

/// `page_identity` for a page number, from the snapshot, with the object id
/// for narration.
fn snapshot_page_key(page: u64) -> Option<(u64, String)> {
    use std::hash::{Hash, Hasher};
    let addr = page * gex_abi::DISPATCH_PAGE;
    let snap = IDENT_SNAPSHOT.0.borrow();
    let (start, _, hasher, off, id) = snap.iter().find(|(s, e, _, _, _)| addr >= *s && addr < *e)?;
    let mut hs = hasher.clone();
    (off + (addr - start)).hash(&mut hs);
    Some((hs.finish(), id.clone()))
}

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
    // Pages numbered by identity, so every process gives a library page the
    // same base and the units they store are interchangeable. See `Identity`.
    // x86-64 only: its byte-granular code makes two processes' numberings
    // diverge within a few misses; AArch64 keeps its established numbering.
    if gex_abi::X86 {
    table.set_identity(std::sync::Arc::new(gex_xlate::Identity {
        adopt: Box::new(|page| {
            let (key, id) = snapshot_page_key(page)?;
            // SAFETY: a lookup in the kernel's own memory, borrowing nothing here.
            let base = unsafe { k_slot_get(page, key) };
            if tracing() && base != 0 {
                trace_line(&format!(
                    "[gex] page {page:#x} adopts base {base:#x} by identity {id:?} key {key:#x}\n"
                ));
            }
            (base != 0).then_some(base)
        }),
        publish: Box::new(|page, entry| {
            if let Some((key, id)) = snapshot_page_key(page) {
                if tracing() {
                    trace_line(&format!(
                        "[gex] page {page:#x} publishes base {entry:#x} as identity {id:?} key {key:#x}\n"
                    ));
                }
                // SAFETY: as above.
                unsafe { k_slot_put(page, key, entry) };
            }
        }),
    }));
    }
    unsafe { k_slot_floor(next_slot) };
    table.set_refill(std::sync::Arc::new(gex_xlate::Refill(Box::new(|n| {
        // SAFETY: as above.
        let base = unsafe { k_slot_alloc(n) };
        // The one place every runtime slot comes from, so the one place that
        // can say the space is gone. Past the top of the last dispatch table
        // `PageTable::base` divides into a table index of `DISPATCH_TABLES`,
        // and the module built from that entry imports a table no instance
        // has: the engine answers "out of bounds table index 8" and the reader
        // is left with a wasm validation error for what is really a budget.
        // Said here, before the module that cannot compile is built.
        if base + n > slot_space_top() {
            static SAID: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
                warn_line(&format!(
                    "[gex] the runtime dispatch slot space is spent: slot {base} is past \
                     the top of table {} and nothing can be translated after this. \
                     Slots are never given back, so a page's whole life shares \
                     {} of them; this run has used them all.\n",
                    gex_abi::DISPATCH_TABLES - 1,
                    slot_space_top(),
                ));
            }
        }
        base
    }))));
    // With a refill in place, forget the allocation mark: left in, it made
    // `gex_table_need(0)` report the whole closure's slots at once, growing
    // table 0 massively on first install. Entries are kept -- a page published
    // at the build's base needs its table to reach it.
    if claim_pages() {
        table.forget_mark_high();
    }
    let _ = n_blocks;
    table
}

// A wasm import is `unsafe` and `Remote` holds safe fn pointers, so each needs
// a wrapper.
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
/// The first few signals a guest sends, capped. One process signalling another
/// is where a session comes apart -- a terminal hanging up a live shell, a
/// shell killing its jobs -- and the receiver's death report can only say what
/// arrived, not who decided it.
fn signal_note(what: &str, caller: i32, target: i32, signo: u32) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SAID: AtomicU32 = AtomicU32::new(0);
    // SIGWINCH (28 on every guest) changes nothing: bash -l sends it to its
    // own group on start.
    if signo == 0 || signo == 28 || SAID.fetch_add(1, Ordering::Relaxed) >= 24 {
        return;
    }
    let line = format!("[sig] pid {caller} sends signal {signo} to {what} {target}\n");
    // SIGINT, SIGKILL and SIGTERM to another process are a parent ending a
    // child on purpose (apt stops each of its methods with SIGINT): noted,
    // not warned. A hangup stays a warning; that is a session coming apart.
    if matches!(signo, 2 | 9 | 15) && caller != target {
        trace_line(&line);
    } else {
        warn_line(&line);
    }
}

extern "C" fn r_kill(caller: i32, target: i32, signo: u32) -> i32 {
    signal_note("pid", caller, target, signo);
    unsafe { k_kill(caller, target, signo) }
}
extern "C" fn r_kill_group(caller: i32, pgid: i32, signo: u32) -> i32 {
    signal_note("group", caller, pgid, signo);
    unsafe { k_kill_group(caller, pgid, signo) }
}
extern "C" fn r_take_signals(pid: i32) -> u64 {
    unsafe { k_take_signals(pid) }
}
extern "C" fn r_take_kill_signals(pid: i32, taken: u64) -> u64 {
    unsafe { k_take_kill_signals(pid, taken) }
}
extern "C" fn r_kill_sender(pid: i32, signo: u32) -> i32 {
    unsafe { k_kill_sender(pid, signo) }
}
extern "C" fn r_tkill(caller: i32, pid: i32, tid: i32, signo: u32) -> i32 {
    signal_note("tid", caller, pid, signo);
    unsafe { k_tkill(caller, pid, tid, signo) }
}
extern "C" fn r_take_thread_signal(pid: i32, tid: i32) -> i64 {
    unsafe { k_take_thread_signal(pid, tid) }
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
extern "C" fn r_set_info(pid: i32, at: gex_kernel::PAddr, len: u32) {
    unsafe { k_proc_set_info(pid, at, len) }
}
extern "C" fn r_snapshot(at: gex_kernel::PAddr, cap: u32) -> i32 {
    unsafe { k_proc_snapshot(at, cap) }
}
extern "C" fn r_new_tid(pid: i32) -> i32 {
    unsafe { k_tid_new(pid) }
}
extern "C" fn r_end_tid(tid: i32) {
    unsafe { k_tid_end(tid) }
}
extern "C" fn r_task_owner(id: i32) -> i32 {
    unsafe { k_task_owner(id) }
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
    // Every waitable object's queue lives in the global kernel, where a
    // waiter in another process can be on it (docs/kernel-locking.md §5.2
    // step 2). Before anything that makes one.
    gex_kernel::wait::set_hooks(gex_kernel::wait::Hooks {
        new: || unsafe { k_wq_new() },
        free: |id| unsafe { k_wq_free(id) },
        notify: |id| unsafe { k_wq_notify(id) },
        chain: |from, to| unsafe { k_wq_chain(from, to) },
        unchain: |from, to| unsafe { k_wq_unchain(from, to) },
    });
    Some(Arc::new(gex_kernel::proc::ProcTable::shared(
        gex_kernel::proc::Remote {
            queue: r_queue,
            spawn: r_spawn,
            exit: r_exit,
            reap: r_reap,
            wait: r_wait,
            wait_ready: r_wait_ready,
            has_children: r_has_children,
            kill: r_kill,
            kill_group: r_kill_group,
            take_signals: r_take_signals,
            take_kill_signals: r_take_kill_signals,
            kill_sender: r_kill_sender,
            has_signals: r_has_signals,
            tkill: r_tkill,
            take_thread_signal: r_take_thread_signal,
            ppid: r_ppid,
            pgid: r_pgid,
            sid: r_sid,
            setsid: r_setsid,
            setpgid: r_setpgid,
            request_halt: r_request_halt,
            halt_code: r_halt_code,
            set_info: r_set_info,
            snapshot: r_snapshot,
            new_tid: r_new_tid,
            end_tid: r_end_tid,
            task_owner: r_task_owner,
        },
    )))
}

extern "C" fn r_queue(pid: i32) -> u32 {
    unsafe { k_proc_queue(pid) }
}
extern "C" fn rp_queue(id: i32) -> u32 {
    unsafe { k_pipe_queue(id) }
}
extern "C" fn rs_queue(id: i32) -> u32 {
    unsafe { k_sock_queue(id) }
}
extern "C" fn rpty_queue(id: i32) -> u32 {
    unsafe { k_pty_queue(id) }
}

extern "C" fn rf_open(path: gex_kernel::PAddr, path_len: u32, flags: u64, mode: u32, now: i64) -> i32 {
    unsafe { k_fs_open_now(path, path_len, flags, mode, now) }
}
extern "C" fn rf_close(handle: i32) {
    unsafe { k_fs_close(handle) }
}
extern "C" fn rf_dup(handle: i32) -> i32 {
    unsafe { k_fs_dup(handle) }
}
extern "C" fn rf_read(handle: i32, offset: u64, at: gex_kernel::PAddr, len: u32) -> i32 {
    unsafe { k_fs_read(handle, offset, at, len) }
}
extern "C" fn rf_write(handle: i32, offset: u64, at: gex_kernel::PAddr, len: u32, now: i64) -> i32 {
    unsafe { k_fs_write_now(handle, offset, at, len, now) }
}
extern "C" fn rf_meta(handle: i32, at: gex_kernel::PAddr) -> i32 {
    unsafe { k_fs_meta(handle, at) }
}
extern "C" fn rf_truncate(handle: i32, len: u64) -> i32 {
    unsafe { k_fs_truncate(handle, len) }
}
extern "C" fn rf_advise(handle: i32, offset: u64, len: u64, advice: u32) -> i32 {
    unsafe { k_fs_advise(handle, offset, len, advice) }
}
extern "C" fn rf_read_dir(handle: i32, at: gex_kernel::PAddr, cap: u32) -> i32 {
    unsafe { k_fs_read_dir(handle, at, cap) }
}
extern "C" fn rf_path_meta(path: gex_kernel::PAddr, path_len: u32, at: gex_kernel::PAddr) -> i32 {
    unsafe { k_fs_path_meta(path, path_len, at) }
}
extern "C" fn rf_read_link(path: gex_kernel::PAddr, path_len: u32, at: gex_kernel::PAddr, cap: u32) -> i32 {
    unsafe { k_fs_read_link(path, path_len, at, cap) }
}
extern "C" fn rf_walk(path: gex_kernel::PAddr, path_len: u32, mounts: gex_kernel::PAddr, mounts_len: u32, follow: u32, at: gex_kernel::PAddr, cap: u32) -> i32 {
    unsafe { k_fs_walk(path, path_len, mounts, mounts_len, follow, at, cap) }
}
extern "C" fn rf_unlink(path: gex_kernel::PAddr, path_len: u32) -> i32 {
    unsafe { k_fs_unlink(path, path_len) }
}
extern "C" fn rf_mkdir(path: gex_kernel::PAddr, path_len: u32, mode: u32) -> i32 {
    unsafe { k_fs_mkdir(path, path_len, mode) }
}
extern "C" fn rf_rmdir(path: gex_kernel::PAddr, path_len: u32) -> i32 {
    unsafe { k_fs_rmdir(path, path_len) }
}
extern "C" fn rf_rename(from: gex_kernel::PAddr, from_len: u32, to: gex_kernel::PAddr, to_len: u32, noreplace: u32) -> i32 {
    unsafe { k_fs_rename(from, from_len, to, to_len, noreplace) }
}
extern "C" fn rf_moved(handle: i32, at: gex_kernel::PAddr, cap: u32) -> i32 {
    unsafe { k_fs_moved(handle, at, cap) }
}
extern "C" fn rf_rename_epoch() -> i32 {
    unsafe { k_fs_rename_epoch() }
}
extern "C" fn rf_hold_renames(handle: i32, owner: i32, on: u32) -> i32 {
    unsafe { k_fs_hold_renames(handle, owner, on) }
}
extern "C" fn rf_chmod(path: gex_kernel::PAddr, path_len: u32, mode: u32) -> i32 {
    unsafe { k_fs_chmod(gex_kernel::PAddr::from(path), path_len, mode) }
}
extern "C" fn rf_fchmod(handle: i32, mode: u32) -> i32 {
    unsafe { k_fs_fchmod(handle, mode) }
}
extern "C" fn rf_set_mtime(path: gex_kernel::PAddr, path_len: u32, secs: i64) -> i32 {
    unsafe { k_fs_set_mtime(gex_kernel::PAddr::from(path), path_len, secs) }
}
extern "C" fn rf_touch(handle: i32, secs: i64) -> i32 {
    unsafe { k_fs_touch(handle, secs) }
}
extern "C" fn rf_set_times(path: gex_kernel::PAddr, path_len: u32, a_s: i64, a_ns: u32, m_s: i64, m_ns: u32, now: i64) -> i32 {
    unsafe { k_fs_set_times(path, path_len, a_s, a_ns, m_s, m_ns, now) }
}
extern "C" fn rf_set_times_fd(handle: i32, a_s: i64, a_ns: u32, m_s: i64, m_ns: u32, now: i64) -> i32 {
    unsafe { k_fs_set_times_fd(handle, a_s, a_ns, m_s, m_ns, now) }
}
extern "C" fn rf_symlink(target: gex_kernel::PAddr, target_len: u32, path: gex_kernel::PAddr, path_len: u32) -> i32 {
    unsafe { k_fs_symlink(target, target_len, path, path_len) }
}
extern "C" fn rf_link(from: gex_kernel::PAddr, from_len: u32, to: gex_kernel::PAddr, to_len: u32) -> i32 {
    unsafe { k_fs_link(from, from_len, to, to_len) }
}
extern "C" fn rf_mksocket(path: gex_kernel::PAddr, path_len: u32) -> i32 {
    unsafe { k_fs_mksocket(path, path_len) }
}

extern "C" fn rp_new(pid: i32) -> i32 {
    let id = unsafe { k_pipe_new(pid) };
    pipe_note("new", id, 0);
    id
}
/// This process's pid, for the calls that name it.
///
/// A plain static: the shim's memory is one process's (one Worker per process).
/// Putting it in the shared kernel meant a pipe end opened under one pid was
/// closed under another, double-closing ends.
static MY_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

fn my_pid() -> i32 {
    vfork::pid_here().unwrap_or_else(|| MY_PID.load(std::sync::atomic::Ordering::Relaxed))
}

/// Say which process this memory is ([`my_pid`]); a vfork child's pid is
/// its Worker's alone (vfork.rs).
fn set_my_pid(pid: i32) {
    if vfork::pid_here().is_none() {
        MY_PID.store(pid, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Traces every change to a pipe's ends, with who made it -- an aggregate
/// cannot show which decrement went missing, only the sequence can.
fn pipe_note(what: &str, id: i32, write: i32) {
    if !tracing() {
        return;
    }
    let e = unsafe { k_pipe_ends(id) };
    let (r, w) = if e < 0 {
        (-1, -1)
    } else {
        ((e >> 16) & 0xffff, e & 0xffff)
    };
    trace_line(&format!(
        "[pipe:{id}] {what} {} by pid {} -> {r}r {w}w\n",
        if write != 0 { "write" } else { "read" },
        my_pid(),
    ));
}

extern "C" fn rp_open_end(id: i32, write: i32) {
    unsafe { k_pipe_open_end(id, write, my_pid()) }
    pipe_note("open", id, write);
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
extern "C" fn rp_last_message(id: i32, at: gex_kernel::PAddr, cap: u32) -> i32 {
    unsafe { k_pipe_last_message(id, at, cap) }
}
extern "C" fn rp_close_end(id: i32, write: i32) {
    unsafe { k_pipe_close_end(id, write, my_pid()) }
    pipe_note("close", id, write);
}
extern "C" fn rp_read(id: i32, at: gex_kernel::PAddr, len: u32) -> i32 {
    unsafe { k_pipe_read(id, at, len) }
}
extern "C" fn rp_write(id: i32, at: gex_kernel::PAddr, len: u32) -> i32 {
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
extern "C" fn rt_get_termios(at: gex_kernel::PAddr, len: u32) -> i32 {
    unsafe { k_tty_get_termios(at, len) }
}
extern "C" fn rt_set_termios(at: gex_kernel::PAddr, len: u32) -> i32 {
    unsafe { k_tty_set_termios(at, len) }
}
extern "C" fn rt_get_locked(at: gex_kernel::PAddr, len: u32) -> i32 {
    unsafe { k_tty_get_locked(at, len) }
}
extern "C" fn rt_set_locked(at: gex_kernel::PAddr, len: u32) -> i32 {
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
extern "C" fn rt_push(at: gex_kernel::PAddr, len: u32) -> i32 {
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
        queue: rp_queue,
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

// Pty wrappers: the ones that name a holder inject this Worker's pid, the way
// the pipe wrappers do, so the global kernel can release a process's ends when
// it leaves by a trap.
/// The first few pty end open/close events, with who did them.
///
/// An end is opened and closed a handful of times in a session -- a terminal
/// takes the master, its child takes the slave, a fork hands both on -- and the
/// whole session rests on the count being right: the last master closing is a
/// SIGHUP to everything in that terminal's session, which from the outside
/// looks exactly like a shell that died for no reason. Cheap to say, and the
/// only way to see an unbalanced open.
fn pty_note(what: &str, id: i32, master: i32, pid: i32) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SAID: AtomicU32 = AtomicU32::new(0);
    if SAID.fetch_add(1, Ordering::Relaxed) >= 60 {
        return;
    }
    let counts = unsafe { k_pty_counts(id) };
    // A note (console.warn), not an error: nothing is wrong when it is said.
    warn_line(&format!(
        "[gex-note] [pty] pid {pid} {what} the {} of pty {id}: {} master(s), {} slave(s) now\n",
        if master != 0 { "master" } else { "slave" },
        counts >> 16,
        counts & 0xffff
    ));
}

extern "C" fn rpty_open_master() -> i32 {
    let r = unsafe { k_pty_open_master(my_pid()) };
    pty_note("opens", r >> 16, 1, my_pid());
    r
}
extern "C" fn rpty_open_slave(ptn: i32) -> i32 {
    let r = unsafe { k_pty_open_slave(ptn, my_pid()) };
    pty_note("opens", ptn, 0, my_pid());
    r
}
extern "C" fn rpty_open_end(id: i32, master: i32) {
    unsafe { k_pty_open_end(id, master, my_pid()) };
    pty_note("inherits", id, master, my_pid());
}
extern "C" fn rpty_close_end(id: i32, master: i32) {
    unsafe { k_pty_close_end(id, master, my_pid()) };
    pty_note("closes", id, master, my_pid());
}
extern "C" fn rpty_read(id: i32, master: i32, at: gex_kernel::PAddr, len: u32) -> i32 {
    let r = unsafe { k_pty_read(id, master, at, len) };
    // End of file on the master is how a terminal emulator learns its shell is
    // gone -- and if it is wrong, the emulator kills the session. Worth saying.
    if r == 0 && master != 0 {
        pty_note("reads end of file on", id, master, my_pid());
    }
    r
}
extern "C" fn rpty_write(id: i32, master: i32, at: gex_kernel::PAddr, len: u32) -> i32 {
    unsafe { k_pty_write(id, master, at, len) }
}
extern "C" fn rpty_readable(id: i32, master: i32) -> i32 {
    unsafe { k_pty_readable(id, master) }
}
extern "C" fn rpty_available(id: i32, master: i32) -> i32 {
    unsafe { k_pty_available(id, master) }
}
extern "C" fn rpty_ptn(id: i32) -> i32 {
    unsafe { k_pty_ptn(id) }
}
extern "C" fn rpty_get_lock(id: i32) -> i32 {
    unsafe { k_pty_get_lock(id) }
}
extern "C" fn rpty_set_lock(id: i32, locked: i32) {
    unsafe { k_pty_set_lock(id, locked) }
}
extern "C" fn rpty_get_termios(id: i32, at: gex_kernel::PAddr, len: u32) -> i32 {
    unsafe { k_pty_get_termios(id, at, len) }
}
extern "C" fn rpty_set_termios(id: i32, at: gex_kernel::PAddr, len: u32, flush: i32) -> i32 {
    unsafe { k_pty_set_termios(id, at, len, flush) }
}
extern "C" fn rpty_winsize(id: i32) -> u64 {
    unsafe { k_pty_winsize(id) }
}
extern "C" fn rpty_set_winsize(id: i32, packed: u64) -> i32 {
    unsafe { k_pty_set_winsize(id, packed) }
}
extern "C" fn rpty_for_sid(sid: i32) -> i32 {
    unsafe { k_pty_for_sid(sid) }
}
extern "C" fn rpty_fg(id: i32) -> i32 {
    unsafe { k_pty_fg(id) }
}
extern "C" fn rpty_set_fg(id: i32, pgrp: i32) {
    // Who the terminal's foreground group is, capped. A shell that is not it
    // spins raising SIGTTIN at itself (dash's `setjobctl` does exactly that),
    // which looks like a terminal that echoes and never runs anything.
    pty_note(&format!("makes group {pgrp} the foreground of"), id, 0, my_pid());
    unsafe { k_pty_set_fg(id, pgrp) }
}
extern "C" fn rpty_sid(id: i32) -> i32 {
    unsafe { k_pty_sid(id) }
}
extern "C" fn rpty_set_sid(id: i32, sid: i32) {
    unsafe { k_pty_set_sid(id, sid) }
}
extern "C" fn rpty_flush(id: i32, which: i32) {
    unsafe { k_pty_flush(id, which) }
}

/// Where a pseudoterminal pair lives, when there is a global kernel to hold it.
fn global_ptys() -> Option<gex_kernel::pty::Remote> {
    if unsafe { k_present() } == 0 {
        return None;
    }
    Some(gex_kernel::pty::Remote {
        queue: rpty_queue,
        open_master: rpty_open_master,
        open_slave: rpty_open_slave,
        open_end: rpty_open_end,
        close_end: rpty_close_end,
        read: rpty_read,
        write: rpty_write,
        readable: rpty_readable,
        available: rpty_available,
        ptn: rpty_ptn,
        get_lock: rpty_get_lock,
        set_lock: rpty_set_lock,
        get_termios: rpty_get_termios,
        set_termios: rpty_set_termios,
        winsize: rpty_winsize,
        set_winsize: rpty_set_winsize,
        fg: rpty_fg,
        for_sid: rpty_for_sid,
        set_fg: rpty_set_fg,
        sid: rpty_sid,
        set_sid: rpty_set_sid,
        flush: rpty_flush,
    })
}

extern "C" fn rs_bind(path: gex_kernel::PAddr, path_len: u32) -> i32 {
    unsafe { k_sock_bind(path, path_len) }
}
extern "C" fn rs_connect(path: gex_kernel::PAddr, path_len: u32) -> i32 {
    unsafe { k_sock_connect(path, path_len) }
}
extern "C" fn rs_accept(listener: i32) -> i32 {
    unsafe { k_sock_accept(listener) }
}
extern "C" fn rs_pending(listener: i32) -> i32 {
    unsafe { k_sock_pending(listener) }
}
extern "C" fn rs_read(id: i32, at: gex_kernel::PAddr, len: u32) -> i32 {
    unsafe { k_sock_read(id, at, len) }
}
extern "C" fn rs_write(id: i32, at: gex_kernel::PAddr, len: u32) -> i32 {
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
extern "C" fn rs_hup(id: i32) -> i32 {
    unsafe { k_sock_hup(id) }
}
extern "C" fn rs_close(id: i32) {
    unsafe { k_sock_close(id) }
}
extern "C" fn rs_shutdown(id: i32, how: u32) -> i32 {
    unsafe { k_sock_shutdown(id, how) }
}
extern "C" fn rs_exists(path: gex_kernel::PAddr, path_len: u32) -> i32 {
    unsafe { k_sock_exists(path, path_len) }
}
extern "C" fn rs_pair(out: gex_kernel::PAddr) -> i32 {
    unsafe { k_sock_pair(out) }
}
extern "C" fn rs_dup(id: i32) -> i32 {
    unsafe { k_sock_dup(id) }
}
extern "C" fn rs_write_fds(id: i32, at: gex_kernel::PAddr, len: u32, descs: gex_kernel::PAddr, descs_len: u32) -> i32 {
    unsafe { k_sock_write_fds(id, at, len, descs, descs_len) }
}
#[allow(clippy::too_many_arguments)]
extern "C" fn rs_read_fds(
    id: i32,
    at: gex_kernel::PAddr,
    len: u32,
    max_fds: u32,
    peek: i32,
    pid: i32,
    out: gex_kernel::PAddr,
    out_cap: u32,
) -> i32 {
    unsafe { k_sock_read_fds(id, at, len, max_fds, peek, pid, out, out_cap) }
}

/// Descriptors for `SCM_RIGHTS` that cannot leave this process -- an epoll
/// set, a memfd, a local socket -- parked by number for a receiver that turns
/// out to be this same process (a wineserver thread hearing from its own
/// process's client). Another process finds no entry and gets nothing.
static LOCAL_FDS: std::sync::Mutex<(u32, Vec<(u32, Arc<gex_kernel::file::OpenFile>)>)> =
    std::sync::Mutex::new((0, Vec::new()));

/// A descriptor as a line another process can rebuild it from. See
/// [`carry_name`]; what has no name to travel by is parked in [`LOCAL_FDS`].
fn scm_describe(file: &Arc<gex_kernel::file::OpenFile>) -> Option<String> {
    const IN_FLIGHT: i32 = -1;
    let name = carry_name(file, IN_FLIGHT).unwrap_or_else(|| {
        let mut t = LOCAL_FDS.lock().unwrap_or_else(|e| e.into_inner());
        t.0 += 1;
        let n = t.0;
        t.1.push((n, Arc::clone(file)));
        format!("local:{}:{n}", my_pid())
    });
    Some(format!("{} {} {}", file.flags(), file.offset(), name))
}

fn scm_rebuild(ns: &gex_kernel::vfs::Namespace, desc: &str) -> Option<Arc<gex_kernel::file::OpenFile>> {
    let mut p = desc.splitn(3, ' ');
    let flags = p.next()?.parse::<u64>().ok()?;
    let offset = p.next()?.parse::<u64>().ok()?;
    let name = p.next()?;
    match rebuild_carried(ns, name, flags, offset) {
        Ok(f) => Some(f),
        Err(e) => {
            warn_line(&format!("gex: SCM_RIGHTS lost '{name}': {e}\n"));
            None
        }
    }
}

fn shared_dns_addr(name: &str) -> u32 {
    unsafe { k_dns_addr(name.as_ptr() as gex_kernel::PAddr, name.len() as u32) }
}

fn shared_dns_name(addr: u32) -> Option<String> {
    let mut buf = [0u8; 256];
    let n = unsafe { k_dns_name(addr, buf.as_mut_ptr() as gex_kernel::PAddr, buf.len() as u32) };
    (n >= 0).then(|| String::from_utf8_lossy(&buf[..(n as usize).min(buf.len())]).into_owned())
}

/// Where a unix socket's two ends live, when there is a global kernel to put
/// them in.
fn global_sockets() -> Option<gex_kernel::unix::Remote> {
    if unsafe { k_present() } == 0 {
        return None;
    }
    // And one boot instant for every process's monotonic clocks.
    gex_kernel::share_monotonic_base(|want| unsafe { k_boot_secs(want as u32) } as u64);
    // And one table of DNS answers: a name one process resolved means the
    // same address in the process that connects to it.
    gex_kernel::net::share_names(gex_kernel::net::SharedNames { address_for: shared_dns_addr, name_for: shared_dns_name });
    // The page's benchmark clocks (`?timescale=`, `?fixedtime=`), if set.
    let gc = unsafe { k_guest_clock(0, 0) };
    if gc != 0 {
        gex_kernel::set_guest_clock((gc >> 32) as f64 / 1000.0, gc as u32 as i32 as i64);
    }
    Some(gex_kernel::unix::Remote {
        queue: rs_queue,
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
        hup: rs_hup,
        shutdown: rs_shutdown,
        exists: rs_exists,
        pair: rs_pair,
        dup: rs_dup,
        write_fds: rs_write_fds,
        read_fds: rs_read_fds,
        describe: scm_describe,
        rebuild: scm_rebuild,
    })
}

/// The root every process shares, if the host gave us a global kernel.
///
/// Mounted at `/` in place of this process's own `TmpFs`. Without it a forked
/// child arrives with an empty root and its `execve` cannot find the program.
/// (The removed switcher gave the child its parent's whole address space, so
/// the question never came up.)
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
            advise: rf_advise,
            read_dir: rf_read_dir,
            path_meta: rf_path_meta,
            read_link: rf_read_link,
            walk: rf_walk,
            unlink: rf_unlink,
            mkdir: rf_mkdir,
            rmdir: rf_rmdir,
            rename: rf_rename,
            moved: rf_moved,
            hold_renames: rf_hold_renames,
            rename_epoch: rf_rename_epoch,
            chmod: rf_chmod,
            fchmod: rf_fchmod,
            symlink: rf_symlink,
            link: rf_link,
            mksocket: rf_mksocket,
            set_mtime: rf_set_mtime,
            touch: rf_touch,
            set_times: rf_set_times,
            set_times_fd: rf_set_times_fd,
        },
    )))
}

/// Put the page's bundle into the shared root, once for the whole system: the
/// first process to load fills it, later ones find it filled and do nothing.
fn install_bundle_remote() {
    // The files went in as they arrived -- see `gex_add_file` -- so all that
    // is left is to say the bundle is complete. `FILES` is empty on this path
    // and clearing it is only tidiness.
    FILES.0.borrow_mut().clear();
    DIRS.0.borrow_mut().clear();
    MODES.0.borrow_mut().clear();
    MTIMES.0.borrow_mut().clear();
    if unsafe { k_fs_populated() } == 0 {
        unsafe { k_fs_populate_done() };
    }
}

/// Back-edges a guest may take between safepoint calls, matching the native
/// host's interval.
const SAFEPOINT_INTERVAL: u32 = 1024;

/// The host's clock, in the shape `gex-kernel` wants.
pub(crate) fn browser_clock() -> (u64, u32) {
    let ns = host_now_ns();
    (ns / 1_000_000_000, (ns % 1_000_000_000) as u32)
}

thread_local! {
    /// This Worker's `clock_origin()`: each Worker's `performance.now()`
    /// counts from its own start, so the origin is per thread, not shared.
    static CLOCK_ORIGIN: Cell<i64> = const { Cell::new(i64::MIN) };
}

/// The host clock in ns, as the `now` import reads it (the same value:
/// `hostNowNs` in gex.mjs is `ORIGIN_NS + round(performance.now() * 1e6)`),
/// without a BigInt per call.
pub(crate) fn host_now_ns() -> u64 {
    let ms = unsafe { perf_now() };
    // try_with: a clock read during thread teardown (after the TLS is
    // destroyed) asks the host again rather than panicking.
    let origin = CLOCK_ORIGIN
        .try_with(|c| {
            let o = c.get();
            if o != i64::MIN {
                return o;
            }
            let o = unsafe { clock_origin() };
            c.set(o);
            o
        })
        .unwrap_or_else(|_| unsafe { clock_origin() });
    (origin + (ms * 1e6).round() as i64) as u64
}

/// The guest's standard input, as a queue the page appends to.
///
/// A terminal's bytes do not exist until someone types them, so this is a
/// queue: `gex_push_stdin` adds, a read takes, a read that finds it empty parks
/// the thread (see `Waiting::Read`).
///
/// `eof` distinguishes "nothing yet" from "nothing ever": without it a shell
/// reading its script from stdin waits forever at the end.
///
/// Shared by the process's Workers, and asked without the kernel lock by a
/// parked thread's idle turn (`waitq`), so it is locked on its own.
#[derive(Debug, Default)]
struct StdinQueue {
    bytes: std::sync::Mutex<VecDeque<u8>>,
    eof: std::sync::atomic::AtomicBool,
}

impl StdinQueue {
    fn push(&self, data: &[u8]) {
        self.bytes.lock().unwrap_or_else(|e| e.into_inner()).extend(data.iter().copied());
        self.notify();
    }

    fn set_eof(&self) {
        self.eof.store(true, std::sync::atomic::Ordering::Release);
        self.notify();
    }

    /// A reader parked on standard input (`stdin_queue`, the same queue a
    /// keystroke into the kernel's queue notifies).
    fn notify(&self) {
        if unsafe { k_present() } != 0 {
            unsafe { k_wq_notify(k_stdin_queue()) };
        }
    }
}

impl FileHandle for StdinQueue {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        // What this Worker was handed directly first -- a host that gives the
        // whole of stdin up front still does, and a run of one process needs
        // no kernel at all.
        {
            let mut q = self.bytes.lock().unwrap_or_else(|e| e.into_inner());
            let n = buf.len().min(q.len());
            if n > 0 {
                for (i, b) in q.drain(..n).enumerate() {
                    buf[i] = b;
                }
                return Ok(n as u64);
            }
        }
        // Then the shared queue, where a keystroke goes now: in the kernel so
        // whoever has it can write and wake a parked reader, since a Worker
        // asleep inside wasm cannot receive a `postMessage`.
        // SAFETY: scalars, and the kernel refuses a range outside this
        // process's memory rather than writing it.
        if unsafe { k_present() } != 0 {
            let n = unsafe { k_stdin_read(buf.as_mut_ptr() as gex_kernel::PAddr, buf.len() as u32) };
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
            nlink: 1,
            rdev: 0,
            times: Default::default(),
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
        if self.eof.load(std::sync::atomic::Ordering::Acquire)
            || !self.bytes.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
        {
            return true;
        }
        // SAFETY: takes and returns scalars.
        unsafe { k_present() != 0 && k_stdin_ready() != 0 }
    }

    fn wait_queue(&self) -> u32 {
        if unsafe { k_present() } != 0 {
            unsafe { k_stdin_queue() }
        } else {
            0
        }
    }
}

/// Mount `/dev`, and bind the Wayland socket if the page brought a canvas.
/// Returns the compositor, which the `Host` holds.
///
/// A function because every kernel this shim builds needs it -- both the
/// process `gex_load` places and every child `gex_adopt_child` adopts, since a
/// child with only the root filesystem has no `/dev/null` and no GPU.
fn mount_devices(kernel: &mut Kernel) -> Option<crate::wl::WaylandDevice> {
    let mut devfs = gex_kernel::vfs::DevFs::new();
    devfs.add("tty", Arc::new(Tty));
    if unsafe { k_present() } != 0 {
        // `/dev/pts`: the pairs some process still holds an end of (the global
        // kernel numbers them from 0 and the id is the pts number).
        devfs.add_pts(Arc::new(|| {
            let mut out = Vec::new();
            for id in 0.. {
                if unsafe { k_pty_ptn(id) } < 0 {
                    break;
                }
                if unsafe { k_pty_counts(id) } != 0 {
                    out.push(id as u32);
                }
            }
            out
        }));
    }
    if crate::gl::have_gl() {
        devfs.add("gex-gl", crate::gl::GlDevice::new());
    }
    // Audio: OSS's /dev/dsp and /dev/mixer, a null output, as natively
    // (shims/native). W3's wineoss.drv is a pure Unix library linked into
    // the loader; without the device its test_connect says Unavailable,
    // mmdevapi picks no driver and a program finds no audio device
    // (waveOutGetNumDevs 0; tests/w3/audio.sh).
    devfs.add_oss();
    kernel.vfs.mount("/dev", Arc::new(devfs));
    // POSIX shared memory (shm_open) is files in /dev/shm, one filesystem
    // for every process so a name one creates another opens (Chromium's
    // processes, in a CEF game). The shim's memory is the processes' shared
    // memory, so one static is one filesystem.
    static SHM: std::sync::OnceLock<Arc<gex_kernel::vfs::TmpFs>> = std::sync::OnceLock::new();
    kernel.vfs.mount("/dev/shm", SHM.get_or_init(|| Arc::new(gex_kernel::vfs::TmpFs::new())).clone());
    // The Wayland compositor: bound only when the page
    // brought a canvas for it, so a guest without one finds no `wayland-0` and
    // libwayland fails to connect cleanly rather than blocking on a dead name.
    //
    // And not for a vfork child running in its parent's Worker (vfork.rs):
    // `set_compositor` is the memory's, which is the parent's, so the
    // child's device replaced the parent's for good, and the parent's own
    // clients were never pumped again (winlogon, after spawning explorer in
    // place, hung on its first Wayland round trip). The child only execs;
    // its program gets a compositor in the Worker the exec starts.
    let wayland = (crate::wl::have_wl() && vfork::pid_here().is_none()).then(|| {
        let device = crate::wl::WaylandDevice::new();
        kernel.host_names.bind_host(
            crate::wl::DEFAULT_SOCKET,
            Arc::new(device.clone()) as Arc<dyn gex_kernel::unix::HostListener>,
        );
        // A node too, for a client that stats the path before connecting.
        let _ = kernel.vfs.mksocket(crate::wl::DEFAULT_SOCKET);
        // And where the GL device can find it: a client rendering on the host
        // names the surface it draws into through /dev/gex-gl.
        crate::wl::set_compositor(&device);
        device
    });
    wayland
}

/// Mount `/proc`. Returns the source it reads from, which the `Host` holds
/// and the scheduler keeps up to date.
///
/// Matters more than it looks: a program that cannot open `/proc/self/maps`
/// gets a null `FILE*`, and the next `fgets` calls through it -- surfacing as a
/// dispatch miss at pc=0 with nothing to blame the filesystem.
fn mount_procfs(kernel: &mut Kernel) -> Arc<gex_kernel::KernelProcSource> {
    let proc_state: Arc<gex_kernel::KernelProcSource> = Arc::new(gex_kernel::KernelProcSource::new());
    kernel.vfs.mount(
        "/proc",
        Arc::new(gex_kernel::vfs::ProcFs::new(proc_state.clone())),
    );
    proc_state
}

/// Give a kernel the three standard descriptors, pointed at the page.
///
/// `Kernel::new`'s own descriptors write through `std::io`, which on
/// `wasm32-unknown-unknown` goes nowhere, so every kernel this shim builds must
/// replace them -- both `gex_load`'s process and every `gex_adopt_child` child,
/// or a forked child's output vanishes.
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
/// A shell with job control opens this before running anything, as do `less`,
/// `ssh` and `sudo`; none accepts a substitute.
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
            nlink: 1,
            rdev: 0,
            times: Default::default(),
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

    fn wait_queue(&self) -> u32 {
        STDIN.wait_queue()
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
        // `c_oflag` first, when this console is a terminal: `ONLCR` maps `\n` to
        // CR-LF, and `OPOST` off (`less`, `vi`) passes bytes through untouched,
        // so it cannot be hard-coded in the page.
        if unsafe { STDIN_TTY } {
            if let Some(cooked) = console_tty().output(buf) {
                unsafe { write(self.fd, cooked.as_ptr() as usize, cooked.len() as u32) };
                // Return what the guest asked to write, not the translated
                // length: a count above the input sends libc looping over a
                // buffer that has already gone.
                return Ok(buf.len() as u64);
            }
        }
        // The buffer is already in the shared memory the host can see, so its
        // address is all that has to cross.
        unsafe { write(self.fd, buf.as_ptr() as usize, buf.len() as u32) };
        Ok(buf.len() as u64)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: gex_kernel::vfs::FileType::Fifo,
            size: 0,
            mode: 0o600,
            ino: 1,
            mtime: 0,
            nlink: 1,
            rdev: 0,
            times: Default::default(),
        })
    }

    /// A console is not seekable, so writes must not advance an offset.
    fn is_stream(&self) -> bool {
        true
    }

    /// Not a terminal: saying so makes musl line-buffer stdout, so bytes would
    /// interleave differently from the same program under a pipe -- what the
    /// suite compares against.
    fn is_tty(&self) -> bool {
        false
    }
}

// The shim's own heap: a fixed region above this module's data, because the
// memory is the guest's and cannot grow (its top is fixed so the guest's stack
// lands where the ABI says).
//
// Talc rather than a first-fit list: the translator allocates constantly, and
// binned free lists keep allocation from degrading as the heap fragments
// (lifting tcc's largest source: 15.2s first-fit vs 0.3s).
//
// Two builds: a plain memory is single-threaded and a cell suffices; a shared
// memory can have several of a process's Worker-threads inside this module at
// once, so it needs a real lock -- `TalcSyncCell` refuses to compile with
// atomics rather than being quietly unsound.
#[cfg(not(feature = "shared-memory"))]
type Heap<const BIG: bool> = talc::cell::TalcSyncCell<ShimRegion<BIG>, talc::wasm::WasmBinning>;
#[cfg(not(feature = "shared-memory"))]
const HEAP: Heap<false> = talc::cell::TalcSyncCell::new_wasm(ShimRegion);
#[cfg(not(feature = "shared-memory"))]
const BIG_HEAP: Heap<true> = talc::cell::TalcSyncCell::new_wasm(ShimRegion);

#[cfg(feature = "shared-memory")]
type Heap<const BIG: bool> =
    talc::sync::TalcLock<spinning_top::RawSpinlock, ShimRegion<BIG>, talc::wasm::WasmBinning>;
#[cfg(feature = "shared-memory")]
const HEAP: Heap<false> = talc::sync::TalcLock::new(ShimRegion);
#[cfg(feature = "shared-memory")]
const BIG_HEAP: Heap<true> = talc::sync::TalcLock::new(ShimRegion);

/// Allocations this big or bigger come from a heap of their own.
///
/// One heap for everything fragmented to exhaustion: Noita at world load had
/// 510 MiB live in 2 GiB of spans, because the megabyte-sized buffers a
/// translation and a GL frame churn through (encoding buffers, module copies,
/// readbacks) came back to holes that small objects had since split, and the
/// next one took a fresh span. Kept apart, big blocks only meet big blocks.
const BIG_MIN: usize = 64 * 1024;

#[global_allocator]
static ALLOC: Counting = Counting(HEAP, std::sync::atomic::AtomicUsize::new(0), BIG_HEAP);

/// The heap, plus how much of it is out on loan.
///
/// The total is worth having because the heap is fixed: running out has no
/// symptom of its own (the page just faults at whatever the guest touched
/// next), so the only way to know is to ask. Without `heap-counters` the
/// counting arms compile away, leaving a delegating newtype.
struct Counting(Heap<false>, std::sync::atomic::AtomicUsize, Heap<true>);

/// Live bytes by size class, so a full heap can say *what* is filling it: a few
/// enormous buffers and a million small ones want different fixes.
///
/// Classes: <64 KiB, <1 MiB, <4 MiB, <16 MiB, >=16 MiB.
static LIVE_BYTES: [std::sync::atomic::AtomicUsize; 5] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; 5];
static LIVE_COUNT: [std::sync::atomic::AtomicUsize; 5] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; 5];

fn size_class(n: usize) -> usize {
    match n {
        ..=65_535 => 0,
        ..=1_048_575 => 1,
        ..=4_194_303 => 2,
        ..=16_777_215 => 3,
        _ => 4,
    }
}

/// The distinct sizes seen in the 1-4 MiB class, with a count each, to name a
/// megabyte-sized block allocated over and over. First eight distinct sizes win
/// a slot, so the hot path walks at most eight words.
static BIG_SIZES: [std::sync::atomic::AtomicUsize; 8] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; 8];
static BIG_SIZE_N: [std::sync::atomic::AtomicUsize; 8] =
    [const { std::sync::atomic::AtomicUsize::new(0) }; 8];

#[cfg(feature = "heap-counters")]
fn note_big(n: usize) {
    for i in 0..8 {
        let s = BIG_SIZES[i].load(std::sync::atomic::Ordering::Relaxed);
        if s == n {
            BIG_SIZE_N[i].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        if s == 0
            && BIG_SIZES[i]
                .compare_exchange(
                    0,
                    n,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
        {
            BIG_SIZE_N[i].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
    }
}

#[cfg(feature = "heap-counters")]
fn live_add(n: usize) {
    let c = size_class(n);
    LIVE_BYTES[c].fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    LIVE_COUNT[c].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if c == 2 {
        note_big(n);
    }
}

#[cfg(feature = "heap-counters")]
fn live_sub(n: usize) {
    let c = size_class(n);
    LIVE_BYTES[c].fetch_sub(n, std::sync::atomic::Ordering::Relaxed);
    LIVE_COUNT[c].fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
}

// SAFETY: both inner allocators are `Sync` on their intended target, and the
// counter is atomic.
unsafe impl Sync for Counting {}

/// An address whose every allocation and free is reported with the wasm
/// stack (`gex_watch_alloc`), zero for none: who held the memory a stray
/// write landed in.
pub(crate) static WATCH_ALLOC: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `GEX_NT_BIG_ALLOC=<bytes>`: report every allocation and free at least
/// this big (the allocations with the stack), to find who fragments the heap.
pub(crate) static BIG_ALLOC: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn watch_alloc(what: &str, p: *mut u8, size: usize) {
    let w = WATCH_ALLOC.load(std::sync::atomic::Ordering::Relaxed);
    let big = BIG_ALLOC.load(std::sync::atomic::Ordering::Relaxed);
    if p.is_null() || (w == 0 && big == 0) {
        return;
    }
    let (a, e) = (p as u64, p as u64 + size as u64);
    let hit = w != 0 && a <= w && w < e;
    let is_big = big != 0 && size as u64 >= big;
    if !hit && !is_big {
        return;
    }
    if is_big && !hit {
        use core::fmt::Write as _;
        struct Buf([u8; 120], usize);
        impl core::fmt::Write for Buf {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let n = s.len().min(self.0.len() - self.1);
                self.0[self.1..self.1 + n].copy_from_slice(&s.as_bytes()[..n]);
                self.1 += n;
                Ok(())
            }
        }
        let mut b = Buf([0; 120], 0);
        let tag = if what.starts_with("free") || what.starts_with("realloc from") { "[gex-big]" } else { "[gex-stack] [gex-big]" };
        let _ = write!(b, "{tag} {what} {a:#x} {size}");
        // SAFETY: `warn` reads the bytes and keeps none.
        unsafe { warn(b.0.as_ptr() as u32, b.1 as u32) };
        return;
    }
    use core::fmt::Write as _;
    struct Buf([u8; 200], usize);
    impl core::fmt::Write for Buf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let n = s.len().min(self.0.len() - self.1);
            self.0[self.1..self.1 + n].copy_from_slice(&s.as_bytes()[..n]);
            self.1 += n;
            Ok(())
        }
    }
    let mut b = Buf([0; 200], 0);
    let _ = write!(b, "[gex-stack] {what} {a:#x}..{e:#x} ({size} bytes) covers watched {w:#x}");
    // SAFETY: `warn` reads the bytes and keeps none.
    unsafe { warn(b.0.as_ptr() as u32, b.1 as u32) };
}

unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        let p = unsafe { if l.size() >= BIG_MIN { self.2.alloc(l) } else { self.0.alloc(l) } };
        watch_alloc("alloc", p, l.size());
        // A null becomes `handle_alloc_error` -> abort -> a bare wasm
        // `unreachable` with nothing said. The heap is fixed, so running out is
        // reachable; say so before the abort.
        if p.is_null() {
            // The request can fail because the arena hit its ceiling or because
            // the memory would not grow; the numbers say which.
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
            live_add(l.size());
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        watch_alloc("free", p, l.size());
        #[cfg(feature = "heap-counters")]
        {
            self.1.fetch_sub(l.size(), std::sync::atomic::Ordering::Relaxed);
            live_sub(l.size());
        }
        unsafe {
            if l.size() >= BIG_MIN {
                self.2.dealloc(p, l)
            } else {
                self.0.dealloc(p, l)
            }
        }
    }

    // Forwarded, not left to the default's allocate-copy-free: talc grows in
    // place where it can, and the lifter pushes onto many vectors.
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, new: usize) -> *mut u8 {
        watch_alloc("realloc from", p, l.size());
        let q = unsafe {
            match (l.size() >= BIG_MIN, new >= BIG_MIN) {
                (false, false) => self.0.realloc(p, l, new),
                (true, true) => self.2.realloc(p, l, new),
                // Across heaps: a fresh block in the other, the bytes, and
                // the old one back.
                (_, to_big) => {
                    let nl = std::alloc::Layout::from_size_align_unchecked(new, l.align());
                    let q = if to_big { self.2.alloc(nl) } else { self.0.alloc(nl) };
                    if !q.is_null() {
                        std::ptr::copy_nonoverlapping(p, q, l.size().min(new));
                        if to_big {
                            self.0.dealloc(p, l)
                        } else {
                            self.2.dealloc(p, l)
                        }
                    }
                    q
                }
            }
        };
        watch_alloc("realloc to", q, new);
        #[cfg(feature = "heap-counters")]
        if !q.is_null() {
            if new >= l.size() {
                self.1.fetch_add(new - l.size(), std::sync::atomic::Ordering::Relaxed);
            } else {
                self.1.fetch_sub(l.size() - new, std::sync::atomic::Ordering::Relaxed);
            }
            // The block may have changed class, so move the whole thing
            // rather than adjusting the delta in place.
            live_sub(l.size());
            live_add(new);
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
struct ShimRegion<const BIG: bool>;

/// Make the memory cover everything below `end`.
///
/// Both growing consumers -- the guest's mmap arena and this module's heap --
/// come through here, so the memory never exceeds what a run asked for.
/// Returns false when the engine will give no more, so `mmap` reports ENOMEM
/// rather than the process trapping on an address that was never backed.
fn grow_memory_to(end: u64) -> bool {
    // One grower at a time, so a sibling Worker's grow is never taken for a
    // foreign one (`OWN_TOP`).
    let _g = GrowLock::take();
    const PAGE: u64 = 65536;
    note_foreign_growth();
    let have = warch::memory_size(0) as u64 * PAGE;
    // DIAGNOSTIC (GEX_PREGROW): grow to a large floor on the first grow so that
    // runtime `memory.grow`s stop happening during the racy startup window --
    // probes whether the SMP crash is a stale shared-memory bound after another
    // Worker grew the SharedArrayBuffer (the "OOB after syscall 222/mmap"
    // signature). Off by default; on, the memory reaches the floor once.
    let target = if pregrow() { end.max(768 * 1024 * 1024) } else { end };
    if target <= have {
        return true;
    }
    // A megabyte at a time past what was asked for: `memory.grow` may copy, so
    // a guest mapping a page at a time should not pay per page.
    let want = (target - have).div_ceil(PAGE).max(16);
    let prev = warch::memory_grow(0, want as usize);
    if prev == usize::MAX {
        return false;
    }
    // Whoever grew between the read above and this grow owns what they grew.
    let prev = prev as u64 * PAGE;
    if prev > have {
        foreign(have.max(OWN_TOP.load(std::sync::atomic::Ordering::Relaxed)), prev);
    }
    OWN_TOP.fetch_max(prev + want * PAGE, std::sync::atomic::Ordering::AcqRel);
    true
}

static GROW_LOCK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
struct GrowLock;
impl GrowLock {
    fn take() -> GrowLock {
        while GROW_LOCK
            .compare_exchange(0, 1, std::sync::atomic::Ordering::Acquire, std::sync::atomic::Ordering::Relaxed)
            .is_err()
        {
            std::hint::spin_loop();
        }
        GrowLock
    }
}
impl Drop for GrowLock {
    fn drop(&mut self) {
        GROW_LOCK.store(0, std::sync::atomic::Ordering::Release);
    }
}

/// The memory size as of this module's own last grow. Memory above it that
/// this module did not grow is someone else's: std's `System` allocator
/// (dlmalloc, under every `thread_local!` with a destructor and every
/// `std::thread::current()`) grows the memory itself and uses the new pages,
/// and the arena once handed the same pages to the heap -- a Worker's
/// thread-local destructor list written into a translation.
static OWN_TOP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bytes of memory grown outside the arena so far (GEX_NT_STATS).
pub(crate) static FOREIGN_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// GEX_NT_TRACE's look, around each NT call: memory grown outside the arena
/// since the last look is recorded now, with the wasm stack of whoever is
/// here (the grower ran just before, on this Worker or another).
pub(crate) fn foreign_now(when: &str, call: &str) {
    let mem = warch::memory_size(0) as u64 * 65536;
    let known = OWN_TOP.load(std::sync::atomic::Ordering::Acquire);
    if known == 0 || mem <= known {
        return;
    }
    warn_line(&format!("[gex-stack] memory grown outside the arena noticed {when} {call}: {known:#x}..{mem:#x}\n"));
    let _g = GrowLock::take();
    note_foreign_growth();
}

/// The arena keeps out of memory someone else grows: grow a page behind its
/// back, then take a span, and see where the span lands.
pub(crate) fn arena_selftest() -> String {
    let mem = warch::memory_size(0) as u64 * 65536;
    if warch::memory_grow(0, 1) == usize::MAX {
        return "skipped (the memory would not grow)".into();
    }
    let (fs, fe) = (mem, mem + 65536);
    let at = arena_take(1 << 20);
    match at {
        Some(a) if a < fe && fs < a + (1 << 20) => format!("FAIL: a span at {a:#x} overlaps the foreign page {fs:#x}..{fe:#x}"),
        Some(a) => format!("ok (foreign page {fs:#x}..{fe:#x}, the next span at {a:#x})"),
        None => "FAIL: the arena could not take a span".into(),
    }
}

/// Pages grown by someone else, kept out of the arena (`memmap::skip_foreign`).
fn foreign(start: u64, end: u64) {
    if end <= start {
        return;
    }
    FOREIGN_BYTES.fetch_add(end - start, std::sync::atomic::Ordering::Relaxed);
    memmap::claim(memmap::Kind::Foreign, 0, start, end, "grown outside the arena (std's System allocator?)");
    static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
        use core::fmt::Write as _;
        struct Buf([u8; 200], usize);
        impl core::fmt::Write for Buf {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let n = s.len().min(self.0.len() - self.1);
                self.0[self.1..self.1 + n].copy_from_slice(&s.as_bytes()[..n]);
                self.1 += n;
                Ok(())
            }
        }
        let mut b = Buf([0; 200], 0);
        let _ = write!(b, "[gex] memory {start:#x}..{end:#x} was grown outside the arena; the arena keeps out of it\n");
        // SAFETY: `warn` reads the bytes and keeps none.
        unsafe { warn(b.0.as_ptr() as u32, b.1 as u32) };
    }
}

/// The memory grown since this module last looked, by someone else.
fn note_foreign_growth() {
    let mem = warch::memory_size(0) as u64 * 65536;
    let known = OWN_TOP.load(std::sync::atomic::Ordering::Acquire);
    if known == 0 {
        // The first look: everything already there (the loader's, a forked
        // child's image) is accounted for elsewhere.
        let _ = OWN_TOP.compare_exchange(0, mem, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    if mem > known && OWN_TOP.compare_exchange(known, mem, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Relaxed).is_ok() {
        foreign(known, mem);
    }
}

static PREGROW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[no_mangle]
pub extern "C" fn gex_set_pregrow(on: i32) {
    PREGROW.store(on != 0, std::sync::atomic::Ordering::Relaxed);
}
fn pregrow() -> bool {
    PREGROW.load(std::sync::atomic::Ordering::Relaxed)
}

/// Turn on gex-wayland protocol tracing (buffers, commits, "produced no frame")
/// -- the shim has no env, so `GEX_WL_TRACE` can't reach the compositor here.
#[no_mangle]
pub extern "C" fn gex_set_wl_trace(on: i32) {
    // gex-wayland's traces use `eprintln!`, a no-op in the wasm shim; route them
    // to the console instead so `?wltrace=1` actually shows the protocol.
    fn sink(s: &str) {
        warn_line(&format!("{s}\n"));
    }
    gex_wayland::set_trace_sink(sink);
    gex_wayland::set_trace(on != 0);
}

/// Dump every thread's register context on a crash -- called from the JS trap
/// handler so the common OOB/"trapped inside" crash (not just guest SIGABRT)
/// names all threads. TWO threads with the SAME `ctx=` is issue #126: two
/// Workers on one register context, which clobbers stacks and looks like
/// corruption anywhere. Reads the thread table, which needs no kernel lock
/// (L23), so it answers even when a crash left the kernel held.
#[no_mangle]
pub extern "C" fn gex_dump_threads() {
    if let Some(home) = sigs::home() {
        let mut s = String::from("[gex] threads at crash:");
        for (i, t) in home.table().iter() {
            let r = t.run();
            s.push_str(&format!(
                " t{i}(tid={} ctx={:#x} pc={:#x} {})",
                t.tid(),
                t.ctx_addr(),
                r.pc,
                waiting_name(&r.waiting),
            ));
        }
        // Per-Worker JS phase (0 init, 3 compiling, 4 installing, 6 in-enter,
        // 7 post-enter, 8 parked) -- says which Workers are stuck in a JS await
        // vs the scheduler, without the console logging that perturbs the
        // timing. klock too. Same process memory, so these are this pid's.
        s.push_str(&format!(" klock={}\n", klock_holder()));
        warn_line(&s);
    }
}

/// Whether the thread whose context is at `ctx` has exited: a Worker whose
/// own thread ended while the process goes on (a wasm32 program's worker
/// thread, joined) is done, not stuck.
#[no_mangle]
pub extern "C" fn gex_ctx_exited(ctx: u32) -> i32 {
    // The thread table, without the kernel lock (L23).
    sigs::home().is_some_and(|home| {
        home.table().iter().any(|(_, t)| t.ctx_addr() == ctx as u64 && matches!(t.run().waiting, Waiting::Exited))
    }) as i32
}

/// How many of an object's stored units one dispatch miss may take, merged
/// into a single module. See the use site in the miss path.
static PER_MISS_N: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(16);

/// The most blocks one dispatch miss lifts (discovery follows successors,
/// call returns and folded call targets breadth-first up to this). 1 lifts
/// only the block that missed, so the blocks lifted are the blocks run.
static LIFT_MAX: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(2048);

/// What the runtime translator did in this process, for `gex_xlate_stat`:
/// 0 guest instructions lifted, 1 wasm bytes emitted, 2 translations (misses
/// that lifted), 3 self-modifying-code page invalidations, 4 pages retired by
/// unmap/reprotect, 5 retire calls, 6 blocks lifted by misses on a page that
/// was retired or invalidated before (re-lifts), 7 blocks lifted outside any
/// named object (a guest JIT's code), 8 wasm bytes of the re-lifts, 9 wasm
/// bytes of the blocks outside any named object.
pub(crate) static XLATE_STAT: [std::sync::atomic::AtomicU64; 10] = [const { std::sync::atomic::AtomicU64::new(0) }; 10];

/// `gex_set_xlate_report(n)` (`?xlatereport=N`, `GEX_XLATE_REPORT=N`): a
/// progress line every `n` translations (0: none), so a run that never exits
/// (a game) still says what it translated, from where, and how much again.
static XLATE_REPORT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `gex_set_t1_fold(1)` (`?t1fold=1`, GEX_T1_FOLD): tier-1's tee folding
/// (gex-emit `set_t1_fold`).
#[no_mangle]
pub extern "C" fn gex_set_t1_fold(on: u32) {
    gex_emit::tier1::set_t1_fold(on != 0);
}

/// `gex_set_discover_skip(bits)` (`GEX_DISCOVER_SKIP`): edges the runtime
/// translator's discovery does not follow (gex-xlate `set_discover_skip`).
#[no_mangle]
pub extern "C" fn gex_set_discover_skip(bits: u32) {
    gex_xlate::set_discover_skip(bits);
}

/// The discovery log's text (`GEX_DISCOVER_SKIP` bit 8), for `gex_dl_text`.
static DL_TEXT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Copy the discovery log's text to `ptr` (at most `cap` bytes) and return its
/// length; with `cap` 0 only the length.
#[no_mangle]
pub extern "C" fn gex_dl_text(ptr: u32, cap: u32) -> u32 {
    let t = DL_TEXT.lock().unwrap_or_else(|e| e.into_inner());
    let n = t.len().min(cap as usize);
    if n > 0 {
        // SAFETY: the caller allocated `cap` bytes at `ptr` (gex_alloc).
        unsafe { std::ptr::copy_nonoverlapping(t.as_ptr(), ptr as usize as *mut u8, n) };
    }
    t.len() as u32
}

#[no_mangle]
pub extern "C" fn gex_set_xlate_report(n: u32) {
    XLATE_REPORT.store(n as u64, std::sync::atomic::Ordering::Relaxed);
}
pub(crate) fn xlate_add(i: usize, n: u64) {
    XLATE_STAT[i].fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}

#[no_mangle]
pub extern "C" fn gex_xlate_stat(i: u32) -> i64 {
    XLATE_STAT.get(i as usize).map_or(-1, |c| c.load(std::sync::atomic::Ordering::Relaxed) as i64)
}

/// `gex_set_inline_dispatch(1)` (`?inlinedispatch=1`, `GEX_INLINE_DISPATCH=1`
/// under run-node): blocks translated after this carry their dispatch inline
/// instead of tail-calling their module's `enter` (gex-emit
/// `set_inline_dispatch`).
#[no_mangle]
pub extern "C" fn gex_set_inline_dispatch(on: i32) {
    gex_emit::tier1::set_inline_dispatch(on != 0);
}

/// Set that, from the host (`?liftmax=N`, `GEX_LIFT_MAX`).
#[no_mangle]
pub extern "C" fn gex_set_lift_max(n: u32) {
    if n > 0 {
        LIFT_MAX.store(n as usize, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Set that, from the host.
#[no_mangle]
pub extern "C" fn gex_set_per_miss(n: u32) {
    if n > 0 {
        PER_MISS_N.store(n as usize, std::sync::atomic::Ordering::Relaxed);
    }
}

/// What the arena has been spent on, by whoever asked: `[0]` guest mappings,
/// `[1]` this module's heap, `[2]` per-Worker stacks, `[3]` largest single
/// claim. Three consumers share one cursor; only `[0]` goes through the map.
static ARENA_SPENT: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// nfds, readfds and writefds of the select in flight; nfds 0 means none.
/// Where `clock_gettime` was told to write, so the trace can read it back.
// Per Worker: set on the way into a trap or a check and read back by the
// same Worker (see `CTX_ADDR` for what a process-wide one did).
thread_local! {
    static CLOCK_ARG: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

static SELECT_ARGS: [std::sync::atomic::AtomicU64; 3] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// The syscall number this thread is inside, or `u64::MAX` between calls.
/// Distinct from `IN_SYSCALL`, which is a bool about re-entry.
static SYSCALL_NR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX);

/// Which syscall each counter in `HEAP_BY_CALL` is for; `u64::MAX - 1` is
/// an unused slot.
static HEAP_CALL_NR: [std::sync::atomic::AtomicU64; 8] = [
    std::sync::atomic::AtomicU64::new(u64::MAX - 1),
    std::sync::atomic::AtomicU64::new(u64::MAX - 1),
    std::sync::atomic::AtomicU64::new(u64::MAX - 1),
    std::sync::atomic::AtomicU64::new(u64::MAX - 1),
    std::sync::atomic::AtomicU64::new(u64::MAX - 1),
    std::sync::atomic::AtomicU64::new(u64::MAX - 1),
    std::sync::atomic::AtomicU64::new(u64::MAX - 1),
    std::sync::atomic::AtomicU64::new(u64::MAX - 1),
];

/// Refills, counted by the syscall that was running, paired with
/// `HEAP_CALL_NR`.
static HEAP_BY_CALL: [std::sync::atomic::AtomicU64; 16] = [
    std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0),
];
/// The syscall that was running for the largest allocation, and which refill
/// it was. See the note where they are set.
static HEAP_BIGGEST_CALL: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX);
static HEAP_BIGGEST_AT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The syscall number of the last refill, exactly rather than folded.
static HEAP_LAST_CALL: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX);

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
    let cur = ARENA_NEXT.load(std::sync::atomic::Ordering::Acquire);
    // First claim starts above both the placement window (where shared objects
    // sit at a common address) and whatever the memory already reaches. The
    // cursor is a per-instance static, zero in a fresh shim -- but a forked
    // child's fresh shim runs over a memory already holding its parent's image,
    // so starting at `ARENA_BOTTOM` would hand out spans the parent still uses
    // (nothing checks: the arena is meant to be the one allocator that cannot
    // collide). The memory is grown to cover the inherited image first, so its
    // size is the floor; an unforked process is below `ARENA_BOTTOM` as before.
    let at = if cur == 0 {
        // A forked child starts above the mappings it inherited -- the top of
        // its *image*, which `gex_adopt_vma_blob` has just seen every region
        // of, not the top of its *memory*.
        //
        // The two are not the same and the difference is most of the address
        // space. A child's memory is grown to cover the parent's image, and the
        // parent's memory reaches wherever the parent's own arena had climbed
        // to -- guest mappings and the parent's runtime heap alike, and the
        // heap is the larger part by far. None of that heap is inherited: the
        // image is `[0, LOW_END)` plus the VMAs and nothing else, so the child's
        // fresh allocator will never read a byte of it. Starting above it threw
        // the space away anyway.
        //
        // What that cost: apt's forked `http` method arrived with a floor of
        // 1682 MiB against an `MMAP_TOP` of 2048, spent 315 MiB on its own
        // heap, and then could not claim the 79.5 MiB for its inherited image.
        // An arena that cannot grow is `handle_alloc_error`, which aborts past
        // the panic hook as a bare `unreachable` -- so `apt-get install`
        // reported "Method http has died unexpectedly!  Sub-process http
        // received signal 9" and nothing about memory at all.
        let inherited = ADOPTED_TOP.load(std::sync::atomic::Ordering::Acquire);
        if inherited != 0 {
            gex_abi::ARENA_BOTTOM.max(inherited)
        } else {
            // Not forked: the memory is its own and nothing else is in it. Kept
            // as the floor rather than assumed, since the loader grows it.
            gex_abi::ARENA_BOTTOM.max(warch::memory_size(0) as u64 * 65536)
        }
    } else {
        cur
    };
    // Never into memory someone else grew (and is using).
    {
        let _g = GrowLock::take();
        note_foreign_growth();
    }
    let at = memmap::skip_foreign(at, len);
    let end = at.checked_add(len)?;
    // No line from here: this is the allocator's refill path (see below).
    HEAP_REACH.fetch_max(end, std::sync::atomic::Ordering::Relaxed);
    if end > nt::arena_ceiling() || !grow_memory_to(end) {
        // Out loud, because the caller turns `None` into `handle_alloc_error`,
        // which aborts past the panic hook as a bare `unreachable` with nothing
        // printed. This is the only place that knows the arena ran out -- and
        // it must say so without allocating: it *is* the allocator's refill
        // path (`ShimRegion::acquire`), so a `format!` would refill, fail, and
        // recurse. A fixed stack buffer and `core::fmt` touch no allocator.
        use core::fmt::Write as _;
        struct Buf([u8; 1400], usize);
        impl core::fmt::Write for Buf {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let room = self.0.len() - self.1;
                let n = s.len().min(room);
                self.0[self.1..self.1 + n].copy_from_slice(&s.as_bytes()[..n]);
                self.1 += n;
                Ok(())
            }
        }
        // Sized generously: `write!` into a full buffer stops silently, so the
        // last field added is the first lost and reads as absent, not truncated.
        let mut b = Buf([0; 1400], 0);
        let _ = write!(
            b,
            "[gex:{}] the arena cannot grow: wanted {len} at {at:#x}, ceiling \
             {:#x}. Spent: {} MiB on guest mappings, {} MiB on this module's \
             heap, {} MiB on Worker stacks; largest single claim {}. Live in \
             the heap right now: {} KiB (-1 means the counters are off), of \
             which {} MiB is kept module \
             bytes and {} MiB is files the kernel holds. {} refills, \
             biggest allocation {} (-1 KiB means the counters are off; build \
             with GEX_HEAP_COUNTERS=1). {} MiB queued for the host in {} \
             modules at most. An abort here is not a panic and prints \
             nothing of its own. Last refill in syscall {}, most in syscall {}; biggest was refill {} in syscall {}. \
             Live by size class -- <64K: {} KiB in {}, <1M: {} KiB in {}, \
             <4M: {} KiB in {}, <16M: {} KiB in {}, >=16M: {} KiB in {}. \
             1-4M sizes: {}x{} {}x{} {}x{} {}x{} {}x{} {}x{} {}x{} {}x{}.\n",
            // Whose arena: every Worker logs to the same console. A plain
            // atomic, safe here where borrowing the host is not.
            my_pid(),
            gex_abi::MMAP_TOP,
            ARENA_SPENT[0].load(std::sync::atomic::Ordering::Relaxed) >> 20,
            ARENA_SPENT[1].load(std::sync::atomic::Ordering::Relaxed) >> 20,
            ARENA_SPENT[2].load(std::sync::atomic::Ordering::Relaxed) >> 20,
            ARENA_SPENT[3].load(std::sync::atomic::Ordering::Relaxed),
            // KiB, undivided: without `heap-counters` this is the sentinel -1,
            // and `-1 / 1024` truncates to 0 and reads as a leak. Printed raw,
            // the sentinel survives.
            gex_heap_used_kib(),
            REGISTRY_BYTES.load(std::sync::atomic::Ordering::Relaxed) >> 20,
            // A kernel static, read directly: must not borrow the host here.
            gex_kernel::vfs::tmpfs::TMPFS_BYTES
                .load(std::sync::atomic::Ordering::Relaxed)
                >> 20,
            HEAP_REFILLS.load(std::sync::atomic::Ordering::Relaxed),
            HEAP_BIGGEST.load(std::sync::atomic::Ordering::Relaxed),
            PENDING_BYTES.load(std::sync::atomic::Ordering::Relaxed) >> 20,
            PENDING_MAX.load(std::sync::atomic::Ordering::Relaxed),
            HEAP_LAST_CALL.load(std::sync::atomic::Ordering::Relaxed) as i64,
            {
                let mut best = (u64::MAX, 0u64);
                for i in 0..8 {
                    let v = HEAP_BY_CALL[i].load(std::sync::atomic::Ordering::Relaxed);
                    if v > best.1 {
                        best = (
                            HEAP_CALL_NR[i].load(std::sync::atomic::Ordering::Relaxed),
                            v,
                        );
                    }
                }
                best.0 as i64
            },
            HEAP_BIGGEST_AT.load(std::sync::atomic::Ordering::Relaxed),
            HEAP_BIGGEST_CALL.load(std::sync::atomic::Ordering::Relaxed) as i64,
            LIVE_BYTES[0].load(std::sync::atomic::Ordering::Relaxed) >> 10,
            LIVE_COUNT[0].load(std::sync::atomic::Ordering::Relaxed),
            LIVE_BYTES[1].load(std::sync::atomic::Ordering::Relaxed) >> 10,
            LIVE_COUNT[1].load(std::sync::atomic::Ordering::Relaxed),
            LIVE_BYTES[2].load(std::sync::atomic::Ordering::Relaxed) >> 10,
            LIVE_COUNT[2].load(std::sync::atomic::Ordering::Relaxed),
            LIVE_BYTES[3].load(std::sync::atomic::Ordering::Relaxed) >> 10,
            LIVE_COUNT[3].load(std::sync::atomic::Ordering::Relaxed),
            LIVE_BYTES[4].load(std::sync::atomic::Ordering::Relaxed) >> 10,
            LIVE_COUNT[4].load(std::sync::atomic::Ordering::Relaxed),
            BIG_SIZE_N[0].load(std::sync::atomic::Ordering::Relaxed),
            BIG_SIZES[0].load(std::sync::atomic::Ordering::Relaxed),
            BIG_SIZE_N[1].load(std::sync::atomic::Ordering::Relaxed),
            BIG_SIZES[1].load(std::sync::atomic::Ordering::Relaxed),
            BIG_SIZE_N[2].load(std::sync::atomic::Ordering::Relaxed),
            BIG_SIZES[2].load(std::sync::atomic::Ordering::Relaxed),
            BIG_SIZE_N[3].load(std::sync::atomic::Ordering::Relaxed),
            BIG_SIZES[3].load(std::sync::atomic::Ordering::Relaxed),
            BIG_SIZE_N[4].load(std::sync::atomic::Ordering::Relaxed),
            BIG_SIZES[4].load(std::sync::atomic::Ordering::Relaxed),
            BIG_SIZE_N[5].load(std::sync::atomic::Ordering::Relaxed),
            BIG_SIZES[5].load(std::sync::atomic::Ordering::Relaxed),
            BIG_SIZE_N[6].load(std::sync::atomic::Ordering::Relaxed),
            BIG_SIZES[6].load(std::sync::atomic::Ordering::Relaxed),
            BIG_SIZE_N[7].load(std::sync::atomic::Ordering::Relaxed),
            BIG_SIZES[7].load(std::sync::atomic::Ordering::Relaxed),
        );
        // SAFETY: `warn` reads the bytes and keeps none of them.
        unsafe { warn(b.0.as_ptr() as u32, b.1 as u32) };
        return None;
    }
    // A compare-and-swap, not a bare store: two Workers that loaded the same
    // cursor would otherwise both be handed `at` and, since a program's threads
    // share this one arena, write over each other -- the out-of-bounds crash a
    // second Mesa thread (the shader-cache worker) hit. Only the Worker still
    // seeing `cur` wins the span; a loser recomputes from the advanced cursor
    // and claims its own bytes. Any grow already done is harmless -- this memory
    // only ever grows, so a span reserved and then lost is just skipped.
    // Grown by someone else while this grew: look again.
    // The check and the move under the memory map's lock, which an island
    // (`arena_back`) is registered under too.
    if memmap::take_span(&ARENA_NEXT, cur, at, len) {
        Some(at)
    } else {
        arena_take(len)
    }
}

/// Make the memory reach `end`, and keep the arena above it.
///
/// For a mapping the guest placed itself (`MAP_FIXED` or an honoured hint): the
/// range is the guest's, and the heap takes from the same cursor, so the cursor
/// must not stay below it.
///
/// Answers where the cursor stood before, so the kernel can keep the bytes it
/// stepped over as arena of its own (see `MemoryMap::grows_on_demand`), or
/// `None` when the memory would not grow.
fn arena_back(start: u64, end: u64) -> Option<u64> {
    // The high arena of a 64-bit memory (docs/mem64.md §3): the memory grows
    // to cover it (V8 commits lazily), but the low arena's cursor -- which
    // this module's heap also takes from, and whose addresses cross to JS as
    // u32 -- stays below 4 GiB.
    if end > gex_abi::HIGH_ARENA_BASE && gex_abi::mem64() {
        if !grow_memory_to(end) {
            return None;
        }
        let prev = HIGH_NEXT.fetch_max(end, std::sync::atomic::Ordering::Relaxed).max(gex_abi::HIGH_ARENA_BASE);
        if end > prev {
            memmap::claim(memmap::Kind::Guest, 0, prev, end, "guest mapping (high arena)");
        }
        return Some(prev);
    }
    // Nothing past the guest's slice (`GUEST_LIMIT`): the kernel could map
    // it, but every access to it (an anonymous mapping's zeroing first)
    // faults. Wine's reservation of everything above 0x7ffe0000 (W3) then
    // shrinks to what the guest can name, as reserve_area halves on a
    // failure.
    if end > GUEST_LIMIT {
        return None;
    }
    // W3's compact space: KUSER_SHARED_DATA's island at 0x7ffe0000 takes the
    // memory to 2 GiB, the guard's limit, as the split did -- the 64 KiB
    // above it (Windows' no-access region) a fault there, not a trap past
    // the end of the memory.
    let grow_to = if nt::w3_compact() && end > 0x7f00_0000 { end.max(0x8000_0000) } else { end };
    w3_note_reach(grow_to, "a fixed mapping", start, end);
    if !grow_memory_to(grow_to) {
        return None;
    }
    // W3's compact space: a mapping wholly above the cursor (KUSER_SHARED_DATA
    // at 0x7ffe0000, an image at its base) is an island the cursor steps over
    // when it gets there, not a jump that leaves everything below it unused --
    // for the shim's heap too, whose ceiling is 2 GiB. The kernel does not
    // take the island as arena: what is unmapped there stays free for the
    // image pinned there, not for the next bottom-up mapping.
    // One inside an island already (the executables' slot, an inherited
    // region) is the guest's as it stands.
    let cur = ARENA_NEXT.load(std::sync::atomic::Ordering::Acquire);
    if nt::w3_compact()
        && (memmap::within_island(start, end)
            || cur != 0 && start >= cur && memmap::claim_island(start, end, &ARENA_NEXT, "guest fixed mapping above the arena"))
    {
        // Backed, and never the kernel's arena: a pinned image's slot stays
        // free for it once it is unmapped.
        return Some(gex_kernel::mem::BACKED_ISLAND);
    }
    let prev = ARENA_NEXT.fetch_max(end, std::sync::atomic::Ordering::Relaxed);
    if end > prev {
        memmap::claim(memmap::Kind::Guest, 0, prev, end, "guest fixed mapping (arena)");
    }
    Some(prev)
}

/// W3's compact space: say what makes a process's memory reach past 1 GiB
/// (it should stay well under; each step costs the tab's data segment). Once
/// per 256 MiB step a process reaches. A rare event, so on by default.
fn w3_note_reach(to: u64, why: &str, start: u64, end: u64) {
    static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 30);
    // Unguarded units fold only constant KUSER_SHARED_DATA addresses (cgen):
    // a computed one into Windows' old page is caught by the engine's bounds
    // check only while the memory ends below it. Past it, such a read would
    // succeed with whatever is there, so say so, once, by default. The
    // heap's own reach (`HEAP_REACH`, recorded where nothing may allocate)
    // is checked here too, at the next mapping.
    static KUSER_SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let reach = to.max(HEAP_REACH.load(std::sync::atomic::Ordering::Relaxed));
    if nt::w3_compact()
        && gex_abi::kuser_fold()
        && reach > gex_abi::KUSER_WINDOWS
        && !KUSER_SAID.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        warn_line(&format!(
            "gex: CRITICAL: W3 pid {}: memory reached the old KUSER page ({:#x}, now {:#x}, for {why}); unguarded units may read it\n",
            my_pid(),
            gex_abi::KUSER_WINDOWS,
            reach,
        ));
    }
    if !nt::w3_compact() || to <= SAID.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let step = (to + (256 << 20) - 1) & !((256u64 << 20) - 1);
    if SAID.fetch_max(step, std::sync::atomic::Ordering::Relaxed) >= step {
        return;
    }
    warn_line(&format!(
        "gex: W3 pid {}: memory reaches {} MiB for {why} [{start:#x}, {end:#x}); arena cursor {:#x}, highest arena span end {:#x}\n",
        my_pid(),
        to >> 20,
        ARENA_NEXT.load(std::sync::atomic::Ordering::Relaxed),
        HEAP_REACH.load(std::sync::atomic::Ordering::Relaxed),
    ));
}

/// The end of the highest span `arena_take` has handed out (the shim's heap
/// and the kernel's arena both), for [`w3_note_reach`].
static HEAP_REACH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How far the high arena has been backed (claimed in `memmap`).
static HIGH_NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What the kernel's memory map calls to claim a span of arena.
///
/// The same cursor this module's heap takes from, so the two can never hand
/// out the same bytes. Zero means the memory would not grow.
fn arena_grow(len: u64) -> u64 {
    arena_note(0, len);
    let at = arena_take(len).unwrap_or(0);
    if at != 0 {
        w3_note_reach(at + len, "the kernel's arena", at, at + len);
    }
    if at != 0 {
        memmap::claim(memmap::Kind::Guest, 0, at, at + len, "guest mapping (arena)");
    }
    at
}

// SAFETY: `acquire` claims a region of this module's own linear memory and
// touches no other allocator.
/// Where this module's heap currently ends, so a new span can extend it.
///
/// `talc` keeps each claimed span separate; only `extend` joins new memory to
/// one it owns, and that needs the old end. Zero means nothing claimed yet.
static HEAP_END: [std::sync::atomic::AtomicUsize; 2] = [const { std::sync::atomic::AtomicUsize::new(0) }; 2];

unsafe impl<const BIG: bool> talc::source::Source for ShimRegion<BIG> {
    fn acquire<B: talc::base::binning::Binning>(
        talc: &mut talc::base::Talc<Self, B>,
        _layout: std::alloc::Layout,
    ) -> Result<(), ()> {
        // A span at a time, from the same arena as the guest's mappings, each
        // growing the memory behind it. (It once claimed everything up to a
        // fixed 4 GiB ceiling at once, which is why the memory had to be 4 GiB.)
        const CHUNK: u64 = 16 * 1024 * 1024;
        let len = (_layout.size() as u64 + _layout.align() as u64).max(CHUNK);
        let len = len.next_multiple_of(65536);
        // A single allocation bigger than a chunk, said out loud. talc cannot
        // serve a request from two spans, so one of these claims a span of its
        // own from an arena that never takes a span back -- which makes a
        // hundred-megabyte allocation a hundred megabytes of address space gone
        // for the process's life, and a repeated one fatal. There is no other
        // way to find out which allocation it was: the biggest-layout counter
        // names the syscall that was running, and the ones that matter here
        // happen while a forked child is being built, before it makes any.
        //
        // Rendered into a stack buffer, never `format!`: this is the
        // allocator's own refill path, so allocating here refills, fails and
        // recurses -- the same care `arena_take` takes, and the run simply
        // stopped with nothing printed when this was written the easy way.
        // The first few, and only well above a chunk: this is a real event worth
        // a line, not a stream. A run that made one of these per miss printed
        // two hundred of them and drowned everything else in the console.
        use core::fmt::Write as _;
        struct Buf([u8; 192], usize);
        impl core::fmt::Write for Buf {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let room = self.0.len() - self.1;
                let n = s.len().min(room);
                self.0[self.1..self.1 + n].copy_from_slice(&s.as_bytes()[..n]);
                self.1 += n;
                Ok(())
            }
        }
        static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        // Said once the span is in hand: a large single allocation that
        // succeeds is legitimate (apt maps its 42 MiB package list), so it is
        // a note; one the arena cannot serve is an error.
        let mut said = None;
        if _layout.size() as u64 > 2 * CHUNK
            && SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 4
        {
            let mut b = Buf([0; 192], 0);
            let _ = write!(
                b,
                "[gex-note] [gex:{}] a single {} MiB allocation takes a span of its own from \
                 the arena, which never gives one back (in {}, syscall {}, \
                 {} refills so far)\n",
                my_pid(),
                _layout.size() >> 20,
                phase_name(PHASE.load(std::sync::atomic::Ordering::Relaxed)),
                LAST_SYSCALL.load(std::sync::atomic::Ordering::Relaxed) as i32,
                HEAP_REFILLS.load(std::sync::atomic::Ordering::Relaxed),
            );
            said = Some(b);
        }
        arena_note(1, len);
        // Count of refills, plus the biggest single layout that forced one --
        // which separates one doubling buffer from many live objects.
        HEAP_REFILLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        {
            // Exact syscall numbers, not folded: `nr % 15` collided `read`,
            // `readlinkat` and `socket`. Eight pairs, first come first served.
            let n = SYSCALL_NR.load(std::sync::atomic::Ordering::Relaxed);
            HEAP_LAST_CALL.store(n, std::sync::atomic::Ordering::Relaxed);
            for i in 0..8 {
                let who = HEAP_CALL_NR[i].load(std::sync::atomic::Ordering::Relaxed);
                if who == n {
                    HEAP_BY_CALL[i].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
                if who == u64::MAX - 1
                    && HEAP_CALL_NR[i]
                        .compare_exchange(
                            u64::MAX - 1,
                            n,
                            std::sync::atomic::Ordering::Relaxed,
                            std::sync::atomic::Ordering::Relaxed,
                        )
                        .is_ok()
                {
                    HEAP_BY_CALL[i].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
            }
        }
        {
            // Who asked for the biggest one: a new maximum is a single event,
            // so the last-entered syscall is the best name available for it.
            let sz = _layout.size() as u64;
            let prev =
                HEAP_BIGGEST.fetch_max(sz, std::sync::atomic::Ordering::Relaxed);
            if sz > prev {
                HEAP_BIGGEST_CALL.store(
                    SYSCALL_NR.load(std::sync::atomic::Ordering::Relaxed),
                    std::sync::atomic::Ordering::Relaxed,
                );
                HEAP_BIGGEST_AT.store(
                    HEAP_REFILLS.load(std::sync::atomic::Ordering::Relaxed),
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
        }
        let Some(base) = arena_take(len) else {
            if let Some(mut b) = said {
                // Not a note any more: the arena could not serve it. Drop the
                // `[gex-note] ` prefix (11 bytes) so it reaches console.error.
                let _ = write!(b, "  ... and the arena could not serve it\n");
                // SAFETY: `warn` reads the bytes and keeps none of them.
                unsafe { warn(b.0.as_ptr().add(11) as u32, (b.1 - 11) as u32) };
            }
            return Err(());
        };
        let base = base as *mut u8;
        if let Some(b) = said {
            // SAFETY: as above.
            unsafe { warn(b.0.as_ptr() as u32, b.1 as u32) };
        }
        memmap::claim(memmap::Kind::Heap, 0, base as u64, base as u64 + len, if BIG { "heap span (big)" } else { "heap span" });
        // Extend the last span when this one abuts it, rather than claiming a
        // separate one: talc cannot serve a request larger than any single span
        // from two adjacent ones, so a doubling buffer would otherwise strand
        // every chunk behind it. Adjacency is checked, not assumed -- the
        // guest's mappings and Worker stacks draw from the same cursor.
        let end = HEAP_END[BIG as usize].load(std::sync::atomic::Ordering::Relaxed);
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
        HEAP_END[BIG as usize].store(new_end.as_ptr() as usize, std::sync::atomic::Ordering::Relaxed);
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
#[derive(Clone)]
enum Waiting {
    /// Ready to be picked up.
    Runnable,
    /// Parked on a futex, until it is woken or the deadline passes.
    Futex {
        parked: gex_kernel::futex::Parked,
        deadline: Option<u64>,
        /// The word value the wait expected, kept as a backup wake signal: a
        /// waker changes the word before waking, so a word that no longer holds
        /// `val` means a wake happened. It is only a backup because it is a
        /// guess -- a contended condvar counter can be back at `val` by the time
        /// this thread is scheduled to look -- superseded by `wake_gen`.
        val: u32,
        /// The shared per-address wake generation (`k_futex_gen`) at park time.
        /// Each Worker has its own `FutexTable`, so a cross-Worker `FUTEX_WAKE`
        /// cannot bump this waiter's local `Parked` generation; the shared
        /// counter can, and a change in it is an EXACT cross-Worker wake --
        /// bumped on every wake whatever the word does. Zero when there is no
        /// global kernel (a lone process needs no cross-Worker relay).
        wake_gen: u32,
    },
    /// Sleeping until this many nanoseconds on the host clock.
    Sleep(u64),
    /// Waiting on a stream with nothing in it: a terminal nobody has typed at
    /// yet. Ready as soon as the handle says a read would return.
    Read(Arc<dyn gex_kernel::vfs::FileHandle>),
    /// Waiting for room in a stream: a pipe nobody has drained yet.
    Write(Arc<dyn gex_kernel::vfs::FileHandle>),
    /// Waiting in a blocking `accept` for a client. Must not park as
    /// `Runnable`: `finish_parked` would answer EAGAIN (right only for a
    /// non-blocking accept) and the server would give up.
    Accept(Arc<dyn gex_kernel::vfs::FileHandle>),
    /// Waiting in `wait4` for a child. Possible only since processes take
    /// turns: a parent can now reach `wait4` while its child still runs.
    Child,
    /// Waiting on the network. Readiness is the operation's question, held in
    /// `blocked_on`, so this only carries the deadline.
    Net(Option<u64>),
    /// In `sigsuspend`: only a signal ends it (`wait_ready`'s first test).
    Signal,
    /// In `rt_sigtimedwait` (`BlockOp::SigWait`, which says for which
    /// signals): until one of its set is pending, until this deadline, or a
    /// signal outside the set ends it.
    SigWait(Option<u64>),
    /// Waiting to take a priority-inheritance lock held by another thread:
    /// until its futex is woken (`gen`, as `Futex::wake_gen`), the lock word
    /// shows no owner, or `deadline`.
    Pi { addr: u64, gen: u32, deadline: Option<u64> },
    /// Finished; the slot stays so a `join` can still find the tid.
    Exited,
    /// Running in another Worker of this process: the entry is real (`join`,
    /// futexes, `live_threads` want it) but this Worker must never pick it up.
    Elsewhere,
}
/// One guest thread. Its registers live in its context in guest memory, so
/// switching to it is a call to the guest entry with a different context
/// pointer; there is no wasm stack to save.
/// A process's threads (docs/kernel-locking.md §3.7, L23): each one's tid,
/// context and run state, added and read from any Worker without the kernel
/// lock. A thread's index is its own for its life (`cur()`).
pub(crate) type Threads = gex_atomic::threads::ThreadTable<ThreadCell>;

/// A process's table with its one thread.
fn threads_of(t: ThreadState, pc: u64, waiting: Waiting) -> Arc<Threads> {
    let table = Arc::new(Threads::default());
    table.push(t.tid, t.ctx_addr, ThreadCell::new(Run::new(pc, waiting)));
    table
}

/// What a slot of [`Threads`] says about its thread.
pub(crate) trait ThreadSlot {
    fn kstate(&self) -> ThreadState;
    /// Where it stands: its own Worker's, which changes it without the
    /// kernel lock (docs/kernel-locking.md §3.7).
    fn run(&self) -> std::sync::MutexGuard<'_, Run>;
    /// Its [`ThreadCell::gone`].
    fn gone(&self) -> &std::sync::atomic::AtomicBool;
    /// Its [`ThreadCell::halted`].
    fn halted(&self) -> &std::sync::atomic::AtomicBool;
}

impl ThreadSlot for gex_atomic::threads::Slot<ThreadCell> {
    fn kstate(&self) -> ThreadState {
        ThreadState::new(self.ctx_addr(), self.tid())
    }

    fn run(&self) -> std::sync::MutexGuard<'_, Run> {
        self.state().run.lock().unwrap_or_else(|e| e.into_inner())
    }


    fn gone(&self) -> &std::sync::atomic::AtomicBool {
        &self.state().gone
    }

    fn halted(&self) -> &std::sync::atomic::AtomicBool {
        &self.state().halted
    }
}

/// A slot's state in [`Threads`]: the thread's run, behind its own lock,
/// and the two flags an `execve` from another thread sets and reads without
/// it (Linux's de_thread).
pub(crate) struct ThreadCell {
    run: Mutex<Run>,
    /// Set when another thread's `execve` took the process from under this
    /// one: the thread has gone with the old program, while the process goes
    /// on. Its Worker reads it through [`thread_gone`].
    gone: std::sync::atomic::AtomicBool,
    /// Set by the thread itself once it is `gone` and has stopped: it runs
    /// no more guest code. An `execve` from another thread waits for this
    /// before it replaces the image (`exec_waits`), as Linux's de_thread
    /// waits for the other threads to die.
    halted: std::sync::atomic::AtomicBool,
}

impl ThreadCell {
    fn new(run: Run) -> ThreadCell {
        ThreadCell { run: Mutex::new(run), gone: Default::default(), halted: Default::default() }
    }

    fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, Run>> {
        self.run.lock()
    }
}

/// A thread's place in its run: what it waits for and what its parked call
/// still owes. Behind its own small lock, not the kernel's: the Worker that
/// owns the thread finishes a parked call it finds ready with no other lock,
/// and the rest (the idle look across threads, `exit_group`, the censuses)
/// read or mark it under this one.
pub(crate) struct Run {
    /// Where to resume, which for a thread that blocked is the instruction
    /// after its `svc`.
    pc: u64,
    waiting: Waiting,
    /// The syscall to finish before it runs again.
    blocked_on: Option<gex_kernel::BlockOp>,
    /// What else that call owes, taken from the kernel as it parked.
    owed: Owed,
}

/// What a parked call owes besides its answer, taken from the kernel when it
/// parks (`gex_kernel::PendingCall`), so that finishing it needs no kernel.
#[derive(Clone, Copy, Debug, Default)]
struct Owed {
    /// `rt_sigsuspend`'s mask to put back, through the handler's frame.
    restore_sigmask: Option<u64>,
    /// A write: an `EPIPE` answer raises `SIGPIPE` on the writer.
    write_blocked: bool,
    /// The call is made again once its wait is over (an `accept`, a
    /// `recvmsg`, a transfer), from its instruction, rather than finished.
    reissue: bool,
}

impl Run {
    fn new(pc: u64, waiting: Waiting) -> Run {
        Run { pc, waiting, blocked_on: None, owed: Owed::default() }
    }
}

/// One emitted module, waiting for JavaScript to instantiate it: the shim can
/// lift and encode a module but not compile one (nothing in wasm can hand bytes
/// to the engine), so a miss produces it here and leaves it for JS.
struct PendingModule {
    wasm: Vec<u8>,
    /// The pages it is the first to claim, published only once its blocks are
    /// actually in the table.
    dirs: Vec<(u64, u32)>,
    /// How many slots the dispatch table must hold for it.
    slots: u64,
    /// Its dense elements, numbered (`dense::register`), published with it.
    dense: Option<gex_emit::dense::Section>,
}

/// Queue a module for JavaScript to install, numbering its dense blocks;
/// `id` is its id in the shared store, if it has one.
fn push_pending(j: &mut Jit, dirs: Vec<(u64, u32)>, slots: u64, mut wasm: Vec<u8>, id: u32) {
    let dense = dense::register(&mut wasm);
    if let (Some(d), true) = (&dense, id != 0) {
        j.dense_base.insert(id, d.base);
    }
    j.pending.push_back(PendingModule { dirs, slots, wasm: exact(wasm), dense });
}

/// A process's translation state, behind its own lock (docs/kernel-locking.md
/// §4.3, step L13) rather than inside the `Host` the kernel lock guards.
///
/// One per process: it lives in the process's memory, so the Workers of the
/// process share it, a vfork child running in a Worker of its parent
/// (vfork.rs) included -- the child runs the parent's code in the parent's
/// memory and this Worker's tables, which is what this state describes; a
/// copy would hand out the same slots twice.
///
/// Boxed behind a pointer, not a static lock in place, so a forked child
/// starts from a fresh one: its image is a copy of the parent's, the lock's
/// word included, taken or not when the copy was made.
static JIT: std::sync::atomic::AtomicPtr<gex_kernel::sync::Mutex<Jit>> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// The process's JIT lock (`JIT`).
fn jit_lock() -> &'static gex_kernel::sync::Mutex<Jit> {
    let p = JIT.load(std::sync::atomic::Ordering::Acquire);
    assert!(!p.is_null(), "gex: the JIT state was asked for before the process had one");
    // SAFETY: set once per process by `install_jit` from a leaked box, never
    // freed (a replaced one is leaked too: a fork's copy may hold it taken).
    unsafe { &*p }
}

/// Make `jit` the process's JIT state, before its `Host` is set.
fn install_jit(jit: Jit) {
    let b = Box::new(gex_kernel::sync::Mutex::ranked(jit, gex_kernel::sync::rank::JIT));
    JIT.store(Box::into_raw(b), std::sync::atomic::Ordering::Release);
}

thread_local! {
    /// The JIT lock as this thread holds it for a kernel call (`JitHere`),
    /// from the first touch of `Host::jit` to the kernel lock's release.
    static JIT_HELD: RefCell<Option<gex_kernel::sync::MutexGuard<'static, Jit>>> =
        const { RefCell::new(None) };
}

/// The JIT state, taking its lock for this thread if it does not hold it.
/// Whoever takes it here gives it back with `jit_give` (`ExecLock`'s release,
/// the end of an SP `with_exec_lock`, `with_jit_only`).
#[track_caller]
fn jit_held() -> *mut Jit {
    let here = std::panic::Location::caller();
    JIT_HELD.with(|g| {
        let mut g = g.borrow_mut();
        let g = g.get_or_insert_with(|| {
            JIT_TAKER.with(|t| t.set(Some(here)));
            jit_lock().lock()
        });
        &mut **g as *mut Jit
    })
}

thread_local! {
    /// Where this thread last took the JIT lock (`jit_held`), for the trap's
    /// "holding the JIT lock" line: the call that kept it.
    static JIT_TAKER: std::cell::Cell<Option<&'static std::panic::Location<'static>>> = const { std::cell::Cell::new(None) };
}

/// Whether this thread holds the JIT lock (`jit_held`).
fn jit_holds() -> bool {
    JIT_HELD.with(|g| g.borrow().is_some())
}

/// Give back the JIT lock if this thread holds it.
fn jit_give() {
    let g = JIT_HELD.with(|g| g.borrow_mut().take());
    drop(g);
}

/// `Host::jit`: the process's JIT state, under its own lock. A kernel call
/// that touches it takes the lock on the first touch and keeps it until it
/// gives the kernel lock back, in that order: the kernel lock, then the JIT
/// lock, then the memory map's (§4.3). A path outside the kernel takes the
/// JIT lock alone (`with_jit_only`).
struct JitHere;

impl std::ops::Deref for JitHere {
    type Target = Jit;
    #[track_caller]
    fn deref(&self) -> &Jit {
        // SAFETY: the lock is this thread's until `jit_give`, which no kernel
        // call does while it holds a borrow of the `Host` this sits in.
        unsafe { &*jit_held() }
    }
}

impl std::ops::DerefMut for JitHere {
    #[track_caller]
    fn deref_mut(&mut self) -> &mut Jit {
        // SAFETY: as `deref`; one `&mut` at a time through the `Host`.
        unsafe { &mut *jit_held() }
    }
}

struct Jit {
    /// Which guest pages own which slots in the dispatch table.
    page_table: gex_xlate::PageTable,
    /// Whether the module said where it had got to. Without it, slots would be
    /// handed out from zero and overwrite the image module's own table blocks.
    described: bool,
    /// Addresses whose slot now holds real code.
    /// Which translation provides each translated address
    /// (`TranslatedModule::module_id`), for `PAGE_STALE`.
    provider: std::collections::HashMap<u64, u64>,
    translated: std::collections::HashSet<u64>,
    /// A sorted snapshot of `translated`, materialised by `gex_coverage_snapshot`
    /// so JS can read the executed block PCs out of linear memory and write them
    /// as a profile seed for a reachability-trimmed closure build.
    coverage_snap: Vec<u64>,
    /// Every translated module's bytes, in install order. A dispatch table
    /// belongs to a module instance and is not shared between Workers, so a
    /// joining Worker instantiates these into its own tables to catch up. Order
    /// matters: each module's page assignments seed the next's.
    registry: Vec<Vec<u8>>,
    /// Per registry entry: where its bytes are in the process's JS-side module
    /// store (`Gex#moduleStore`) as `(offset, len)`, or `None` when they are in
    /// `registry` itself. An entry kept there holds no bytes in this module's
    /// heap: the registry grew without bound in a long fjit run (980 MiB of
    /// Muck's 1670 MiB heap at the 2 GiB arena's OOM).
    registry_ext: Vec<Option<(u64, u32, u64)>>,
    /// The bytes of the last modules kept in the JS-side store, still alive:
    /// a sibling Worker that read the pending queue's front just before this
    /// one popped it is copying them out (`gex_jit_pending_refresh`).
    store_grace: std::collections::VecDeque<Vec<u8>>,
    /// Per registry module, a hash of each 256-byte chunk as it was pushed,
    /// so a stray write into the bytes can be found (`verify_registry`).
    registry_sums: Vec<Vec<u64>>,
    /// Each registry module's dense range `(base, n)`, `(0, 0)` for none:
    /// how a Worker missing index `d` finds the module to instantiate
    /// (`gex_dense_owner`), whatever order it caught up in.
    registry_dense: Vec<(u32, u32)>,
    /// Whether this process has ever run a second thread-Worker. The registry
    /// exists only so a joining Worker can catch up; a single-threaded process
    /// never gains one, so retaining its modules is pure loss against a fixed
    /// ~2 GiB heap. Set once at the first `SpawnThread` and never cleared while
    /// the program lives, because the join can come long after the modules.
    ever_threaded: bool,
    /// Slots that code of a given identity was translated into, system-wide.
    /// Keyed by page address and byte identity (file, offset, mapping length),
    /// so two processes mapping the same library at the same address share the
    /// slots. Not part of saved state -- it outlives `fork` and `execve`.
    shared: std::collections::HashMap<(u64, u64), u32>,
    /// Shared modules this Worker has put into its own tables. Describes the
    /// Worker, not the program, and survives `execve` as the tables do.
    installed_mods: std::collections::HashSet<u32>,
    /// Per object, how many of the kernel's modules for it (in its order) are
    /// all in `installed_mods`: a miss reads the list from there on, not from
    /// the start. See `shared_modules_of_object`.
    obj_cursor: std::collections::HashMap<u64, u32>,
    /// The dense base each stored module (by store id) was given here: the
    /// store keeps the bytes as built, and `backfill_registry` must keep
    /// them as installed (`dense`).
    dense_base: std::collections::HashMap<u32, u32>,
    /// Pages whose translations were retired (code unmapped, reprotected or
    /// flushed: Detours patching noita.exe). A stored module covering one may
    /// predate the change, so it is never served here again; see
    /// `gex_jit_installed` for the pending side.
    pub(crate) retired_pages: std::collections::HashSet<u64>,
    /// Translations by pc (`remiss`): a pc translated again and again is a livelock.
    remiss: std::collections::HashMap<u64, u32>,
    /// How the shared-slot lookup went. Three outcomes: the page had a base in
    /// this process's table, the identity map knew it, or it did not. See
    /// `gex_reuse_stats`.
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
    /// Guest addresses that are `memcpy` and friends, served by a wasm
    /// bulk-memory instruction rather than translated. Filled from an object's
    /// symbols and relocations the first time a miss lands in it -- not at map
    /// time (a mapping is not evidence anything runs).
    intrinsics: std::collections::HashMap<u64, gex_xlate::Intrinsic>,
    /// How many times each object has been through [`note_intrinsics`], so a
    /// library is parsed once when the answer is settled and a bounded number
    /// of times while it is not.
    intrinsics_scanned: std::collections::HashMap<String, u32>,
    /// Objects the guest has mapped but not yet recompiled, and how many misses
    /// have landed in each. An object waits until it proves itself: translating
    /// at map time pays for every function whether or not the run calls one
    /// (gears demo, 1.49M blocks vs 352K on demand), while libc is entered
    /// enough to earn recompilation almost at once.
    candidates: Vec<(gex_kernel::CodeMap, u32)>,
    /// Modules JS has yet to instantiate, oldest first. A queue because
    /// recompiling one object emits many modules (a wasm module holds at most a
    /// million functions, one per block), installed in emission order since
    /// each one's page assignments seed the next's.
    pending: std::collections::VecDeque<PendingModule>,
    blocks: u64,
    /// Blocks lifted on a page a closure library already claims -- says whether
    /// a closure is missing something the total alone cannot.
    lift_in_closure: u64,
    /// Blocks lifted per object, with the address range: the range says whether
    /// a closure is missing an object or only part of one.
    by_object: std::collections::BTreeMap<String, (u64, u64, u64)>,
    /// Wasm bytes emitted, by object (as `by_object`). See `xlate_report`.
    obj_wasm: std::collections::BTreeMap<String, u64>,
    compiles: u64,
    /// Nanoseconds lifting and encoding -- the shim's share of a miss; the
    /// engine's compile of the output is JS's.
    nanos: u64,
}

/// Written at both ends of [`Host`] and checked on the way into every
/// syscall. See [`HOST_MAGIC_HEAD`].
const HOST_MAGIC: u64 = 0x6765_785f_484f_5354;

struct Host {
    /// A canary: this shim's failures are memory corruption found far from
    /// where they happened. Two words, one at each end, so a write landing
    /// anywhere across the structure trips one. Checked at syscall entry (a
    /// load and a compare), turning "wrong, elsewhere, later" into "the call
    /// before this did it".
    magic_head: u64,
    kernel: Kernel,
    /// The Wayland compositor, when the page brought a canvas. Held here, not
    /// in the kernel, because the scheduler pumps it once per turn.
    wayland: Option<crate::wl::WaylandDevice>,
    /// The root filesystem, kept so files can be added before the guest runs.
    root: Arc<gex_kernel::vfs::TmpFs>,
    /// The snapshot `/proc` reads from, refreshed before each syscall that
    /// could look at it.
    proc_state: Arc<gex_kernel::KernelProcSource>,
    /// Every guest thread, the running one included.
    threads: Arc<Threads>,
    jit: JitHere,
    /// When an `execve` from a threaded process began waiting for its other
    /// threads to stop (`exec_waits`).
    exec_waited_since: Option<u64>,
    /// The other end of the canary. See `magic_head`.
    magic_tail: u64,
}

/// Check the canary, and say so loudly once if it has gone. Reported not
/// trapped (a trap would replace the crash being diagnosed); once, because a
/// corrupted `Host` fails every later syscall and a flood is not a diagnosis.
/// Whether the host structure is intact (its guard words), checked with the
/// host held.
fn check_host_magic_in(h: &Host, where_: &str) {
    let (head, tail) = (h.magic_head, h.magic_tail);
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
/// A plain `static`, not a thread-local: with `+atomics` each Worker has its
/// own TLS block, so a thread-local would give each its own file table and
/// memory map -- a fork, not a thread. The shim's data lives in the guest's
/// memory, a `SharedArrayBuffer` under a shared build, so a static is already
/// visible to every Worker; only mutual exclusion was missing, hence the lock.
/// A spinlock because there is no OS to park on and a syscall holds it briefly.
#[cfg(feature = "shared-memory")]
static mut HOST: Option<Host> = None;

/// How much of the address space the guest may use.
///
/// Not all of it: `GuestMem` is a slice, and a slice may not exceed
/// `isize::MAX` (just under 2 GiB on wasm32). A 4 GiB slice miscompiles rather
/// than failing cleanly. The guest gets everything below this line; the shim
/// takes what is above through ordinary pointers. The guest's stack, its
/// highest thing, is placed here rather than at `STACK_TOP`.
#[cfg(target_arch = "wasm32")]
const GUEST_LIMIT: u64 = 0x7fff_0000;
/// A wasm64 shim's slice may span the whole memory64, high arena included
/// (docs/mem64.md step 6).
#[cfg(target_arch = "wasm64")]
const GUEST_LIMIT: u64 = gex_abi::LIMIT64;

/// The guest's memory, as a slice.
///
/// This module's memory *is* the guest's, so the address space below
/// [`GUEST_LIMIT`] is addressable directly.
#[allow(clippy::mut_from_ref)]
#[inline(never)]
fn guest_bytes() -> &'static mut [u8] {
    // The *pointer* (not just its address) goes through `black_box`: otherwise
    // LLVM sees a constant-zero pointer aliasing nothing and deletes stores
    // through it, or folds bounds checks to always-out-of-range -- miscompiles
    // that returned EFAULT from the kernel path but succeeded from a small fn.
    //
    // SAFETY: address zero is an ordinary linear-memory location where the
    // guest's address space starts; this module's data sits above `STACK_TOP`
    // where the guest never maps; the memory is imported and never grows.
    let p: *mut u8 = std::hint::black_box(std::ptr::with_exposed_provenance_mut::<u8>(0));
    // Length hidden for the same reason. It spans the whole nameable address
    // space; what exists is smaller and moves, so `GuestMem` reads
    // `memory.size` live on every access. See `GuestMem::limit`.
    let len = std::hint::black_box(GUEST_LIMIT as usize);
    unsafe { std::slice::from_raw_parts_mut(p, len) }
}

/// Write to the guest's stderr -- in a page, the only channel the shim has to
/// say anything.
///
/// # Safety
/// `write` only reads the bytes.
pub(crate) unsafe fn write_stderr(text: &str) {
    write(2, text.as_ptr() as usize, text.len() as u32);
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
            // what `with_exec_lock` panics about -- so the hook took the borrow a
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
            unsafe { write(2, text.as_ptr() as usize, text.len() as u32) };
        }));
    });
}

/// Install the kernel this Worker will run against.
#[cfg(not(feature = "shared-memory"))]
fn set_host(host: Host) {
    // A new process: its registry starts empty, whatever a fork copied.
    regview::reset();
    regview::publish_marks(&jit_lock().lock().page_table);
    // A fork copies the parent's memory, translation in flight or not; the
    // child's JIT state is the `host` given here.
    jitout::OUT.store(0, std::sync::atomic::Ordering::Release);
    *jitout::BOX.0.borrow_mut() = None;
    HOST.with(|h| {
        *h.borrow_mut() = Some(host);
        // The new process's descriptor table (`FDS`), which stays in place
        // inside the cell until the next `set_host`.
        let fds = h
            .borrow_mut()
            .as_mut()
            .map_or(std::ptr::null_mut(), |h| Arc::as_ptr(&h.kernel.fds) as *mut gex_kernel::file::FdTable);
        FDS.store(fds, std::sync::atomic::Ordering::Release);
        if let Some(h) = h.borrow_mut().as_mut() {
            publish_process(h);
        }
    });
}

#[cfg(feature = "shared-memory")]
fn set_host(host: Host) {
    regview::reset();
    regview::publish_marks(&jit_lock().lock().page_table);
    // A fork copies the parent's memory, translation in flight or not; the
    // child's JIT state is the `host` given here.
    jitout::OUT.store(0, std::sync::atomic::Ordering::Release);
    *jitout::BOX.0.borrow_mut() = None;
    // SAFETY: see `with_exec_lock` -- one Worker is inside the kernel at a time.
    unsafe { *core::ptr::addr_of_mut!(HOST) = Some(host) };
    // The new process's descriptor table (`FDS`).
    // SAFETY: as above; the table stays in place until the next `set_host`.
    let fds = unsafe {
        (*core::ptr::addr_of_mut!(HOST))
            .as_mut()
            .map_or(std::ptr::null_mut(), |h| Arc::as_ptr(&h.kernel.fds) as *mut gex_kernel::file::FdTable)
    };
    FDS.store(fds, std::sync::atomic::Ordering::Release);
    // SAFETY: as above.
    if let Some(h) = unsafe { (*core::ptr::addr_of_mut!(HOST)).as_mut() } {
        publish_process(h);
    }
}

/// The host of a process this Worker alone has: a fork child's or an exec
/// child's, between its adoption and its first guest instruction (F, L25c),
/// before any other Worker of it exists. Its fields are written directly,
/// with no lock: nothing else can reach them yet (docs/kernel-locking.md
/// §3.12 F).
fn with_own_host<R>(f: impl FnOnce(&mut Host) -> R) -> R {
    #[cfg(not(feature = "shared-memory"))]
    let r = HOST.with(|h| f(h.borrow_mut().as_mut().expect("gex: an adoption before its process")));
    // SAFETY: the process has one Worker, this one, and it is not inside
    // any other call into the shim (an adoption is its own export).
    #[cfg(feature = "shared-memory")]
    let r = f(unsafe { (*core::ptr::addr_of_mut!(HOST)).as_mut() }.expect("gex: an adoption before its process"));
    debug_assert!(sigs::home().is_none_or(|h| h.table().len() == 1), "an own host with more than one thread");
    // The JIT lock, if the call took it (`JitHere`), as `with_exec_lock`
    // gives it back. `do_exec` takes it (`jit_in_held(&mut h.jit)`) under an
    // adoption (`gex_adopted_exec`): kept, it went into the new program with
    // the thread and every exec'd process's first trap said "BUG: trapped
    // out to JS holding the JIT lock" (B18, 9 in a Win7 shell boot), its
    // other threads waiting on the lock meanwhile.
    jit_give();
    r
}

/// Forget the host a fork's copy of its parent's memory holds, dropping
/// nothing of it: its descriptors, sockets and ends are the parent's, and
/// each closes its end in the global kernel when it drops (how a process
/// leaving by a trap still releases them), charged to whichever pid this
/// Worker answers as. That is what killed a terminal's session: the shell's
/// slave end went with the copy, the master read end of file, and the whole
/// foreground group took a SIGHUP one command in. The copy is leaked, into
/// memory that is this process's own image anyway. No lock: the copy is
/// this Worker's alone.
fn forget_copied_host() {
    #[cfg(not(feature = "shared-memory"))]
    let old = HOST.with(|h| h.borrow_mut().take());
    // SAFETY: as `with_own_host`.
    #[cfg(feature = "shared-memory")]
    let old = unsafe { (*core::ptr::addr_of_mut!(HOST)).take() };
    std::mem::forget(old);
}

/// What a new process's Workers read without the kernel lock: its signal
/// state (`sigs`), with its one thread as the one this Worker runs, and
/// whether it is a wasm32 program (`WASM32`).
fn publish_process(h: &mut Host) {
    WASM32.store(h.kernel.wasm32, std::sync::atomic::Ordering::Release);
    sigs::publish(&h.kernel, &h.threads, &h.proc_state.0);
    wl::publish_process(&h.kernel);
    h.kernel.publish_proc(&h.proc_state.0);
    bind_mine(h, 0);
}

/// Whether every thread of this process is parked on a futex with no
/// deadline, and not one of them already woken: a deadlock, if it lasts
/// (`DEADLOCK_CONFIRM`). No kernel lock: the thread table and each run
/// state lock themselves.
pub(crate) fn all_futex_blocked() -> bool {
    let Some(home) = sigs::home() else { return false };
    let table = home.table();
    if table.is_empty() {
        return false;
    }
    let now = turn_now();
    table.iter().all(|(_, t)| {
        let r = t.run();
        matches!(r.waiting, Waiting::Futex { deadline: None, .. }) && !wait_ready(&r.waiting, &r.blocked_on, now, false)
    })
}

/// [`publish_process`] for a vfork child entering its parent's Worker:
/// what reads its process without the kernel reads the child's, from this
/// Worker only (vfork.rs); the parent's other Workers keep the parent's.
fn publish_vfork_child(h: &mut Host) {
    sigs::publish_here(Some((&h.kernel, &h.threads, &h.proc_state.0)));
    vfork::set_fds_here(&h.kernel.fds);
    h.kernel.publish_proc(&h.proc_state.0);
    bind_mine(h, 0);
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
        // SAFETY: a read of an `Option`'s discriminant; see `with_exec_lock`.
        unsafe { (*core::ptr::addr_of!(HOST)).is_some() as i32 }
    }
}

/// Where the call currently inside `with_exec_lock` came from.
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
static FTX_PC: [std::sync::atomic::AtomicU64; FTX_RING] =
    [const { std::sync::atomic::AtomicU64::new(0) }; FTX_RING];
static FTX_TID: [std::sync::atomic::AtomicI32; FTX_RING] =
    [const { std::sync::atomic::AtomicI32::new(0) }; FTX_RING];
static FTX_AT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `op` is the raw futex operation, or `!0` for the resumption of a wait
/// that had parked -- the answer the syscall itself never recorded.
fn ftx_note(op: u64, addr: u64, ret: i64, pc: u64, tid: i32) {
    use std::sync::atomic::Ordering::Relaxed;
    let i = FTX_AT.fetch_add(1, Relaxed) % FTX_RING;
    FTX_OP[i].store(op, Relaxed);
    FTX_ADDR[i].store(addr, Relaxed);
    FTX_RET[i].store(ret, Relaxed);
    FTX_PC[i].store(pc, Relaxed);
    FTX_TID[i].store(tid, Relaxed);
}

fn ftx_recent() -> String {
    use std::sync::atomic::Ordering::Relaxed;
    let n = FTX_AT.load(Relaxed);
    let mut out = String::new();
    for j in n.saturating_sub(FTX_RING)..n {
        // Chromium truncates a long console argument. Keep each physical line
        // short enough that the worker's line-splitting console wrapper can
        // preserve the complete ring in a crash report.
        if j > n.saturating_sub(FTX_RING) && (j - n.saturating_sub(FTX_RING)) % 6 == 0 {
            out.push_str("\n           ");
        }
        let i = j % FTX_RING;
        let op = FTX_OP[i].load(Relaxed);
        let (addr, ret) = (FTX_ADDR[i].load(Relaxed), FTX_RET[i].load(Relaxed));
        let (pc, tid) = (FTX_PC[i].load(Relaxed), FTX_TID[i].load(Relaxed));
        if op == !0 {
            out.push_str(&format!(" t{tid}@{pc:#x} resume({addr:#x})={ret}"));
        } else {
            out.push_str(&format!(" t{tid}@{pc:#x} {addr:#x} op={op:#x}={ret}"));
        }
    }
    out
}

/// The last bytes the process wrote to fd 2, captured at the write itself.
///
/// A guest that aborts writes glibc's own fatal message -- `free(): invalid
/// pointer`, a failed assertion, `malloc(): corrupted top size` -- to stderr
/// and then raises SIGABRT; but an apt method or any forked helper has that fd
/// wired to a pipe its parent drains, so the one line that says *why* it died
/// never reaches a console. Recorded here at the write, when the buffer is
/// unarguably live, and read back when the process dies -- reading it out of
/// the syscall ring at the death instead failed, the pointer no longer
/// resolving. Per-Worker like the other rings, so it is this process's own.
const STDERR_CAP: usize = 256;
static mut STDERR_BUF: [u8; STDERR_CAP] = [0; STDERR_CAP];
static mut STDERR_LEN: usize = 0;

/// A write to standard output or error (`a1`, `a2` as `write` or `writev`
/// has them), kept for a later abort's report, and said when it fell short.
fn stderr_capture(gm: &GuestMem<'_>, fd: u64, vectored: bool, a1: u64, a2: u64, ret: u64) {
    // (base, length) pairs: one for `write`, `a2` of them for
    // `writev`, so the two cases differ only in where the list is.
    let parts: Vec<(u64, u64)> = if !vectored {
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
    // The bytes themselves, gathered once. Kept for a later abort (the
    // fatal message is a full write, not the short one below), and
    // reused for the short-write warning.
    let mut said = Vec::new();
    for (p, n) in &parts {
        if let Ok(b) = gm.read(*p, (*n).min(200) as usize) {
            said.extend_from_slice(b);
        }
    }
    if !said.is_empty() {
        stderr_note(&said);
    }
    let lost = (ret as i64) < 0 && ret as i64 != -(Errno::EAGAIN.0 as i64);
    if want > 0 && (lost || ret == 0) {
        // Printable text as is, anything else escaped, so a binary
        // write cannot fill the console with replacement characters.
        let text: String = said
            .iter()
            .flat_map(|&c| match c {
                b' '..=b'~' => vec![c as char],
                b'\n' => vec!['\\', 'n'],
                _ => format!("\\x{c:02x}").chars().collect(),
            })
            .collect();
        warn_line(&format!(
            "[gex] pid {} could not write to {} ({} for {want} bytes in {} part{}): {text}",
            my_pid(),
            if fd == 1 { "stdout" } else { "stderr" },
            ret as i64,
            parts.len(),
            if parts.len() == 1 { "" } else { "s" },
        ));
    }
}

fn stderr_note(bytes: &[u8]) {
    // Keep the last non-blank write: a fatal diagnostic is often followed by a
    // shorter write (a newline, a prompt redraw) that would otherwise erase it.
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return;
    }
    // Written without the kernel lock (a `write` takes none): a writer that
    // finds another one here leaves the note to it.
    static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if BUSY.swap(true, std::sync::atomic::Ordering::Acquire) {
        return;
    }
    unsafe {
        let n = bytes.len().min(STDERR_CAP);
        STDERR_BUF[..n].copy_from_slice(&bytes[..n]);
        STDERR_LEN = n;
    }
    BUSY.store(false, std::sync::atomic::Ordering::Release);
}

fn last_stderr() -> String {
    unsafe {
        String::from_utf8_lossy(&STDERR_BUF[..STDERR_LEN])
            .trim_end()
            .replace('\n', " ")
    }
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
    // From the oldest entry still held: before the ring wraps that is 0, not
    // `n % RING`, or an unwrapped ring reads nothing but empty slots.
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

/// The last few `with_exec_lock` call sites, oldest first: a re-entrancy's
/// interesting part is the path, not just who holds the kernel.
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
fn with_exec_lock<R>(f: impl FnOnce(&mut Host) -> R) -> R {
    install_panic_hook();
    let here = std::panic::Location::caller();
    if let Some(child) = vfork::host_here() {
        // SAFETY: the child's boxed host (vfork.rs), which only this Worker
        // reaches, until `gex_vfork_end` (outside any call here) drops it.
        let r = f(unsafe { &mut *child });
        jit_give();
        return r;
    }
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
        // Named: bare "not initialised" says an export ran before `gex_init`
        // but not which, and the glue calls thirty.
        let r = f(b.as_mut().unwrap_or_else(|| {
            panic!("gex: not initialised, called from {}:{}", here.file(), here.line())
        }));
        HOST_HOLDER.store(0, std::sync::atomic::Ordering::Relaxed);
        // The JIT lock, if the call took it (`JitHere`).
        jit_give();
        r
    })
}

/// The same, under the exec lock ([`ExecLock`]). Re-entrancy is a real hazard
/// -- a host call that asks the guest to write can come back through here, a
/// deadlock under a spinlock -- but every such path already had to avoid it.
#[cfg(feature = "shared-memory")]
#[track_caller]
fn with_exec_lock<R>(f: impl FnOnce(&mut Host) -> R) -> R {
    install_panic_hook();
    // A plain static borrowed in place, neither locked-across-`f` nor moved.
    // The shim traps out of `with_exec_lock` as ordinary control flow (that is how
    // the stack goes back to JS) and a trap does not unwind, so a guard held
    // across `f` would never drop; moving the `Host` out and back copies it
    // twice per syscall, indistinguishable from a hang. The static is shared
    // between Workers for free (the shim's data is in the guest's
    // `SharedArrayBuffer` memory).
    //
    // SAFETY: `ExecLock` makes one Worker inside the kernel at a time true rather
    // than hoped for; taken here, given back before return; any non-return path
    // out of `f` is a trap that ends the process.
    let _guard = ExecLock::take();
    // A vfork child running in this Worker has a process of its own
    // (vfork.rs); the lock still, for the translation state it shares.
    if let Some(child) = vfork::host_here() {
        // SAFETY: the child's boxed host, which only this Worker reaches,
        // until `gex_vfork_end` (outside any call here) drops it.
        return f(unsafe { &mut *child });
    }
    unsafe {
        f((*core::ptr::addr_of_mut!(HOST))
            .as_mut()
            .expect("gex: not initialised"))
    }
}

/// Room for the Workers running threads of this process: a stack and a
/// thread-local block each, both per Worker.
///
/// The stack pointer is a wasm global (per instance), so without this every
/// Worker started at the same address and two ran on one stack; TLS needs
/// `__wasm_init_tls` per instance, which nothing was calling.
///
/// Taken from the growing arena, never from `.bss`: `wasm-ld` refuses to link
/// a `--shared-memory` module whose data does not fit `--initial-memory`, so a
/// static 64-slot array made the shared build silently fail to build.
#[cfg(feature = "shared-memory")]
mod slots {
    /// A megabyte of stack each. The shim's deepest call is a translation,
    /// which is recursive over a control-flow graph.
    pub const STACK: usize = 1 << 20;
    /// And room for the thread-locals, which are a handful of words today.
    pub const TLS: usize = 1 << 16;
    pub const COUNT: usize = 64;
    /// Bytes of guard pattern at the bottom of each stack.
    pub const GUARD: usize = 4096;
}

#[cfg(feature = "shared-memory")]
extern "C" {
    /// Generated by wasm-ld for a module built with `+atomics`: point this
    /// instance's `__tls_base` at `ptr` and copy the initial values in.
    fn __wasm_init_tls(ptr: *mut u8);
}

/// Which slots a live instance holds (bit `i`), and where each slot's span
/// of arena is once one was taken for it (0: never).
#[cfg(feature = "shared-memory")]
static SLOT_USED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "shared-memory")]
static SLOT_BASE: [std::sync::atomic::AtomicU64; slots::COUNT] =
    [const { std::sync::atomic::AtomicU64::new(0) }; slots::COUNT];

/// Claim a slot for this Worker and return the top of its stack, or 0 when
/// all `COUNT` are held.
///
/// Called once per Worker instance before anything else. The caller writes
/// the answer into `__stack_pointer` (a wasm global cannot be assigned from
/// inside its own module). A slot is given back by `gex_worker_release` when
/// the instance is done: a process that starts threads one after another
/// (handles.exe's children make 150) gets a new instance per thread, and
/// with slots never returned the 65th ran with no thread-local block --
/// `__tls_base` zero, its thread-locals over the guest's low memory.
#[cfg(feature = "shared-memory")]
#[no_mangle]
pub extern "C" fn gex_worker_init() -> u32 {
    use std::sync::atomic::Ordering;
    let i = loop {
        let used = SLOT_USED.load(Ordering::Acquire);
        let i = (!used).trailing_zeros() as usize;
        if i >= slots::COUNT {
            return 0;
        }
        if SLOT_USED.compare_exchange(used, used | 1 << i, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            break i;
        }
    };
    let mut base = SLOT_BASE[i].load(Ordering::Acquire);
    if base == 0 {
        // This slot's own span of arena. The cursor is atomic and the bytes
        // above it belong to nobody, so two Workers arriving together get two
        // spans without a lock between them.
        arena_note(2, (slots::STACK + slots::TLS) as u64);
        let Some(b) = arena_take((slots::STACK + slots::TLS) as u64) else {
            SLOT_USED.fetch_and(!(1 << i), Ordering::AcqRel);
            return 0;
        };
        base = b;
        SLOT_BASE[i].store(base, Ordering::Release);
        // SAFETY: as below; the claims name the span once, when it is taken.
        unsafe {
            extern "C" {
                static __heap_base: u8;
            }
            memmap::claim_fixed(core::ptr::addr_of!(__heap_base) as u64);
        }
        memmap::claim(memmap::Kind::Stack, i as u32, base, base + slots::STACK as u64, "shim stack");
        memmap::claim(
            memmap::Kind::Tls,
            i as u32,
            base + slots::STACK as u64,
            base + (slots::STACK + slots::TLS) as u64,
            "thread-locals",
        );
    }
    // SAFETY: `base` is this Worker's alone -- its bit in `SLOT_USED` says so
    // until `gex_worker_release` -- and the memory behind it has been grown to
    // cover it.
    unsafe {
        let at = base as u32 as *mut u8;
        __wasm_init_tls(at.add(slots::STACK));
        CLAIMED.with(|c| c.set(true));
        // The thread-locals exist now, so this is the first thing that can be
        // said about which Worker this is.
        WORKER.with(|w| w.set(i as u32));
        gex_kernel::sync::set_thread_id(i as u32 + 1);
        // A guard at the bottom of the stack: a stack that outgrows its
        // megabyte runs on into whatever the arena handed out below it (a
        // heap span, so translations, the registry...). Checked at every NT
        // call (`stack_guard_check`).
        std::slice::from_raw_parts_mut(at as *mut u32, slots::GUARD / 4).fill(STACK_GUARD);
        STACK_LO.with(|l| l.set(at as u32));
        (at as u32) + slots::STACK as u32
    }
}

#[cfg(feature = "shared-memory")]
const STACK_GUARD: u32 = 0x5747_5d0d;
/// A forked child's first instance, before it claims: the memory it was
/// given is its parent's, and so are the slot bits -- held by the parent's
/// Workers, none of which run here. The child starts with every slot free
/// (their arena spans stay, for reuse); a parent with many live threads
/// otherwise left its child too few to start its own.
#[cfg(feature = "shared-memory")]
#[no_mangle]
pub extern "C" fn gex_worker_slots_reset() {
    SLOT_USED.store(0, std::sync::atomic::Ordering::Release);
}

/// A forked child's inherited region ending at `end`, told before its first
/// arena claim (`gex_worker_init` takes a stack there, before
/// `gex_adopt_vma_blob` has run). Only regions under the arena's ceiling
/// matter: the arena never hands out above it. Without this the first claim
/// floored at the child's memory size -- its parent's, grown over every
/// inherited mapping -- and a mem64 parent's threads' stacks above 4 GiB put
/// that over the ceiling: the child got no slot and ran with no
/// thread-locals.
#[no_mangle]
pub extern "C" fn gex_note_inherited(end: u64) {
    if end <= nt::arena_ceiling() {
        ADOPTED_TOP.fetch_max(end, std::sync::atomic::Ordering::Release);
    }
}

/// [`gex_note_inherited`] with the region's start. In W3's compact space
/// (`nt::w3_compact`, set by `gex_adopt_windows_space` just before) an
/// inherited region in the arena is an island the child's cursor steps
/// over, not a floor: the parent's fixed mappings reach 2 GiB
/// (KUSER_SHARED_DATA), so a floor above them left the child's heap no room,
/// and everything between them is the parent's heap, which the child never
/// reads.
#[no_mangle]
pub extern "C" fn gex_note_inherited_range(start: u64, end: u64) {
    w3_note_reach(end, "an inherited region", start, end);
    if nt::w3_compact() && end > gex_abi::ARENA_BOTTOM && end <= nt::arena_ceiling() {
        let start = start.max(gex_abi::ARENA_BOTTOM);
        if ARENA_NEXT.load(std::sync::atomic::Ordering::Acquire) == 0
            && memmap::claim_island(start, end, &ARENA_NEXT, "inherited guest mapping")
        {
            return;
        }
    }
    gex_note_inherited(end);
}

/// Give this instance's slot back (`gex_worker_init`): the last call the
/// Worker makes into this instance, once its run is over or it trapped. The
/// next instance to claim the slot runs on the same stack, so nothing of
/// this one may run after: this function keeps nothing on the shim stack.
#[cfg(feature = "shared-memory")]
#[no_mangle]
pub extern "C" fn gex_worker_release() {
    if !CLAIMED.with(|c| c.replace(false)) {
        return;
    }
    let i = worker_id() as usize;
    SLOT_USED.fetch_and(!(1u64 << i), std::sync::atomic::Ordering::AcqRel);
}


/// This Worker's id, as the lock knows it.
#[cfg(feature = "shared-memory")]
pub(crate) fn worker_id() -> u32 {
    WORKER.with(|w| w.get())
}

#[cfg(feature = "shared-memory")]
thread_local! {
    /// The bottom of this Worker's shim stack (its guard), zero for none.
    static STACK_LO: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// The least headroom seen above the guard.
    static STACK_LEAST: std::cell::Cell<u32> = const { std::cell::Cell::new(u32::MAX) };
}

/// Whether this Worker's shim stack has run into its guard: `Some((bottom,
/// how many guard bytes are overwritten, least headroom seen))` the first time
/// it has. Also keeps the least headroom, read from the address of a local.
#[cfg(feature = "shared-memory")]
pub(crate) fn stack_guard_check() -> Option<(u32, u32, u32)> {
    let lo = STACK_LO.with(|l| l.get());
    if lo == 0 {
        return None;
    }
    let here = 0u8;
    let sp = &here as *const u8 as u32;
    let room = sp.wrapping_sub(lo + slots::GUARD as u32);
    STACK_LEAST.with(|l| l.set(l.get().min(room)));
    // SAFETY: this Worker's own guard words, inside the span it claimed.
    let g = unsafe { std::slice::from_raw_parts(lo as *const u32, slots::GUARD / 4) };
    // A stack running down into the guard hits its top words first: those
    // are checked every call, the whole guard only once they have changed.
    if g[g.len() - 16..].iter().all(|w| *w == STACK_GUARD) {
        return None;
    }
    let top_bad = g.iter().rposition(|w| *w != STACK_GUARD)?;
    STACK_LO.with(|l| l.set(0));
    Some((lo, (top_bad as u32 + 1) * 4, STACK_LEAST.with(|l| l.get())))
}

#[cfg(feature = "shared-memory")]
pub(crate) fn stack_least() -> u32 {
    STACK_LEAST.with(|l| l.get())
}

/// Which Worker this is. Per-Worker via TLS, exactly right here (unlike the
/// kernel, hence `HOST` is a plain static). Zero is "not yet told", so the
/// lock's owner word is id-plus-one and zero means free.
#[cfg(feature = "shared-memory")]
thread_local! {
    static WORKER: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// This instance holds a `gex_worker_init` slot.
    static CLAIMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// This Worker's kernel-lock recursion depth: a re-entrant `with_exec_lock`
    /// (the panic handler writing to fd 2) would otherwise self-deadlock.
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
    gex_kernel::sync::set_thread_id(id + 1);
}


/// Who holds the kernel: a Worker id plus one, or zero for nobody.
#[cfg(feature = "shared-memory")]
static KLOCK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Who holds the kernel lock right now: 0 = nobody, else the holder's Worker id
/// plus one. For deadlock diagnosis -- a Worker spinning in `ExecLock::take` while
/// this stays non-zero and unchanged is blocked on whoever holds it.
fn klock_holder() -> u32 {
    #[cfg(feature = "shared-memory")]
    { KLOCK.load(std::sync::atomic::Ordering::Relaxed) }
    #[cfg(not(feature = "shared-memory"))]
    { 0 }
}
#[no_mangle]
pub extern "C" fn gex_klock_holder() -> u32 {
    klock_holder()
}

/// The exec lock, held for one call into `with_exec_lock`: what is left of the
/// kernel lock (docs/kernel-locking.md §3.12). Only the process calls take it
/// -- `execve`, and `clone`, `clone3`, `fork` and `vfork` making a process,
/// and the dynamic linker's `SNAP_SYSCALL` that keeps a linked image -- which
/// replace, copy or record the whole `Host` (its image, its memory map, its JIT
/// state): it keeps a fork from copying a process an exec is replacing, as
/// Linux's `exec_update_lock` does. Every other call is the free path's.
///
/// A `memory.atomic.wait32` rather than a spin, to not burn a core on a
/// foreground tab. Blocking is legal here because every guest thread runs in a
/// Worker; only the page's main thread may not block.
#[cfg(feature = "shared-memory")]
struct ExecLock;

#[cfg(feature = "shared-memory")]
impl ExecLock {
    fn take() -> ExecLock {
        let me = WORKER.with(|w| w.get()) + 1;
        if DEPTH.with(|d| d.get()) > 0 {
            // Already ours. Counted rather than taken again.
            DEPTH.with(|d| d.set(d.get() + 1));
            return ExecLock;
        }
        // Outermost of the locks that nest: not under the JIT state's or
        // the memory map's (docs/kernel-locking.md §4.3).
        gex_kernel::sync::rank_take(gex_kernel::sync::rank::KERNEL);
        let mut spins: u32 = 0;
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
                    // A Worker that spins here for seconds is blocked on whoever
                    // holds the lock -- a deadlock if that holder is itself
                    // waiting. Say so, once per second, naming the holder.
                    spins += 1;
                    if spins % 1000 == 0 {
                        warn_line(&format!(
                            "[gex] worker {} has spun {}s for KLOCK, held by {}\n",
                            me - 1,
                            spins / 1000,
                            held.wrapping_sub(1),
                        ));
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
        ExecLock
    }
}

#[cfg(feature = "shared-memory")]
impl Drop for ExecLock {
    fn drop(&mut self) {
        let d = DEPTH.with(|d| d.get());
        DEPTH.with(|c| c.set(d.saturating_sub(1)));
        if d > 1 {
            return;
        }
        // The JIT lock, if the call took it, before the kernel lock (§4.3).
        jit_give();
        KLOCK.store(0, std::sync::atomic::Ordering::Release);
        gex_kernel::sync::rank_give(gex_kernel::sync::rank::KERNEL);
        // SAFETY: as above.
        unsafe {
            warch::memory_atomic_notify(KLOCK.as_ptr() as *mut i32, 1);
        }
    }
}

/// The JIT's own state while a translation runs outside `ExecLock`
/// (docs/kernel-locking.md, step 5).
///
/// A dispatch miss lifts with the kernel lock let go: the translator takes the
/// page table, the translated set and the intrinsics out of `Host::jit`, marks
/// them out here, gives the lock back, translates, and leaves what it produced
/// in `BOX` with `DONE` set. Whoever next wants the JIT state under the lock
/// (`jit_in`) puts it back, the translator itself included. Nothing else in the
/// kernel waits for a translation any more: a sibling's syscall, futex or
/// schedule pass goes ahead while it lifts.
mod jitout {
    use std::sync::atomic::AtomicU32;
    /// 0 when the JIT state is in `Host::jit`; otherwise the translating
    /// Worker's id plus one, with `DONE` once its result is in `BOX`.
    pub(crate) static OUT: AtomicU32 = AtomicU32::new(0);
    pub(crate) const DONE: u32 = 1 << 31;
    /// Bumped by `retire_code`, each range it retired logged in `RETIRED`
    /// under the new value: a translator that sees it moved installs its unit
    /// and then retires those ranges again, the order the two would have had
    /// under one lock (a unit queued, then its code retired).
    pub(crate) static EPOCH: AtomicU32 = AtomicU32::new(0);
    pub(crate) static RETIRED: super::SingleThreaded<std::collections::VecDeque<(u32, u64, u64)>> =
        super::SingleThreaded(std::cell::RefCell::new(std::collections::VecDeque::new()));
    /// Bumped by `do_exec`: a unit of the program it replaced is dropped.
    pub(crate) static EXEC_EPOCH: AtomicU32 = AtomicU32::new(0);
    pub(crate) type Parts = (
        gex_xlate::PageTable,
        std::collections::HashSet<u64>,
        std::collections::HashMap<u64, gex_xlate::Intrinsic>,
    );
    /// The translator's result, written before `DONE` (release) and taken
    /// after seeing it (acquire), so never touched by two Workers at once.
    pub(crate) static BOX: super::SingleThreaded<Option<Parts>> =
        super::SingleThreaded(std::cell::RefCell::new(None));
}

/// This Worker as `jitout::OUT` names it.
fn jit_me() -> u32 {
    #[cfg(feature = "shared-memory")]
    { worker_id() + 1 }
    #[cfg(not(feature = "shared-memory"))]
    { 1 }
}

/// Put the JIT state back into `h` if a translation has finished with it.
/// False while another Worker is still translating with it out.
fn jit_in(j: &mut Jit) -> bool {
    use std::sync::atomic::Ordering::{Acquire, Release};
    let s = jitout::OUT.load(Acquire);
    if s == 0 {
        return true;
    }
    if s & jitout::DONE == 0 {
        if s == jit_me() {
            // This Worker is the translator, between taking the state out and
            // putting its result in the box: nothing runs there that asks.
            panic!("gex: the JIT state was asked for in the middle of this Worker's own translation");
        }
        return false;
    }
    let Some((table, known, intr)) = jitout::BOX.0.borrow_mut().take() else {
        panic!("gex: a translation said it was done and left nothing");
    };
    j.page_table = table;
    j.translated = known;
    j.intrinsics = intr;
    regview::publish_marks(&j.page_table);
    jitout::OUT.store(0, Release);
    #[cfg(feature = "shared-memory")]
    // SAFETY: the address of a `static AtomicU32` in the shared memory.
    unsafe {
        warch::memory_atomic_notify(jitout::OUT.as_ptr() as *mut i32, u32::MAX);
    }
    true
}

/// `jit_in`, for a caller already inside the kernel: it cannot give the lock
/// back, so it waits holding it. The translator needs nothing under the lock to
/// finish, so this ends.
fn jit_in_held(j: &mut Jit) {
    let mut waits = 0u32;
    while !jit_in(j) {
        #[cfg(feature = "shared-memory")]
        jit_wait();
        waits += 1;
        jit_wait_said(waits, true);
        #[cfg(not(feature = "shared-memory"))]
        panic!("gex: the JIT state is out with no other Worker to bring it back");
    }
}

/// Once a second of waiting (a wait is at most a millisecond), say so, naming
/// the translator: a Worker stuck here is blocked on a translation that does
/// not finish.
fn jit_wait_said(waits: u32, held: bool) {
    if waits % 1000 == 0 {
        warn_line(&format!(
            "[gex] worker {} has waited {}s for the JIT state ({}), out with worker {}\n",
            jit_me() - 1,
            waits / 1000,
            if held { "holding the kernel lock" } else { "lock given back" },
            (jitout::OUT.load(std::sync::atomic::Ordering::Relaxed) & !jitout::DONE).wrapping_sub(1),
        ));
    }
}

/// Wait (briefly) for `jitout::OUT` to move while a translation is under way.
#[cfg(feature = "shared-memory")]
fn jit_wait() {
    let s = jitout::OUT.load(std::sync::atomic::Ordering::Acquire);
    if s != 0 && s & jitout::DONE == 0 {
        // SAFETY: as `jit_in`.
        unsafe {
            warch::memory_atomic_wait32(jitout::OUT.as_ptr() as *mut i32, s as i32, 1_000_000);
        }
    }
}

/// For anything that reads or changes the JIT's page table, translated set,
/// intrinsics or closure maps: they may be out with a translator (`jitout`),
/// and they are under the JIT's lock alone, not the kernel's
/// (docs/kernel-locking.md §4.3; the kernel-lock form, `with_jit`, went with
/// its last caller in L16). Inside a kernel call it uses the
/// JIT lock the call holds or takes it under the kernel lock; outside one,
/// it waits for a translation in flight with the JIT lock given back. `f`
/// may not enter the kernel: the kernel lock ranks above the JIT lock.
#[track_caller]
fn with_jit_only<R>(f: impl FnOnce(&mut Jit) -> R) -> R {
    let held = jit_holds();
    #[cfg(feature = "shared-memory")]
    let nested = held || DEPTH.with(|d| d.get()) > 0;
    #[cfg(not(feature = "shared-memory"))]
    let nested = true;
    let mut waits = 0u32;
    loop {
        // SAFETY: this thread holds the JIT lock until `jit_give` below (or
        // the kernel call's own release), and `j` does not outlive `f`.
        let j = unsafe { &mut *jit_held() };
        if nested {
            jit_in_held(j);
        } else if !jit_in(j) {
            jit_give();
            #[cfg(feature = "shared-memory")]
            jit_wait();
            waits += 1;
            jit_wait_said(waits, false);
            continue;
        }
        let r = f(j);
        if !held {
            jit_give();
        }
        return r;
    }
}

/// What translation reads of its process besides the JIT state: the
/// memory map (its own lock, ranked under the JIT's), the namespace and the
/// pid (docs/kernel-locking.md §3.6, L16). From the kernel while a kernel
/// call has it (`Px::of`), else from the process's published `Home`
/// (`with_px`), with no kernel lock.
pub(crate) struct Px<'a> {
    pub(crate) mm: &'a gex_kernel::mem::Mm,
    pub(crate) ns: &'a gex_kernel::vfs::Namespace,
    pub(crate) pid: i32,
}

impl<'a> Px<'a> {
    fn of(h: &'a Host) -> Px<'a> {
        Px { mm: &h.kernel.mem, ns: &h.kernel.vfs, pid: h.kernel.pid }
    }
}

/// `f` with this process's [`Px`] from its published `Home`.
fn with_px<R>(f: impl FnOnce(&Px<'_>) -> R) -> R {
    let home = sigs::home().expect("gex: no process published");
    let ns = home.ns();
    f(&Px { mm: home.mm(), ns: &ns, pid: home.pid() })
}

/// Sockets the host holds one end of, so JS can carry a guest's stream
/// somewhere the guest cannot reach -- another Worker, in particular. Their
/// own lock: the browser's Xwayland<->compositor bridge calls send and recv
/// from several Workers at once while others connect and accept.
static HOST_SOCKETS: gex_kernel::sync::Mutex<Vec<Arc<gex_kernel::unix::UnixSocket>>> =
    gex_kernel::sync::Mutex::new(Vec::new());

fn host_socket(handle: i32) -> Option<Arc<gex_kernel::unix::UnixSocket>> {
    HOST_SOCKETS.lock().get(handle.max(0) as usize).cloned()
}

fn host_socket_add(sock: Arc<gex_kernel::unix::UnixSocket>) -> i32 {
    let mut table = HOST_SOCKETS.lock();
    table.push(sock);
    (table.len() - 1) as i32
}

/// Connect the host to a unix socket the *guest* has bound: the mirror of
/// `bind_host`, needed so a guest that *serves* a socket (an X server in one
/// Worker, its client in another) can be reached from outside.
///
/// Returns a handle for [`gex_host_send`]/[`gex_host_recv`], or -1 when nothing
/// is bound at that path.
#[no_mangle]
pub extern "C" fn gex_host_connect(path: u32, path_len: u32) -> i32 {
    let Ok(name) = std::str::from_utf8(shim_slice(path, path_len)) else {
        return -1;
    };
    let Some(home) = sigs::home() else { return -1 };
    let sock = Arc::new(gex_kernel::unix::UnixSocket::new(true));
    if home.socket_names().connect(&sock, name).is_err() {
        return -1;
    }
    host_socket_add(sock)
}

/// Write to the guest through a handle from [`gex_host_connect`].
#[no_mangle]
pub extern "C" fn gex_host_send(handle: i32, ptr: u32, len: u32) -> i32 {
    let bytes = shim_slice(ptr, len);
    // The socket cloned out of the table, the I/O outside its lock.
    match host_socket(handle) {
        Some(s) => s.send(bytes).map(|n| n as i32).unwrap_or(-1),
        None => -1,
    }
}

/// Read whatever the guest has written. Returns 0 when there is nothing yet,
/// which is not the same as end of stream -- the caller comes back.
#[no_mangle]
pub extern "C" fn gex_host_recv(handle: i32, ptr: u32, max: u32) -> i32 {
    let Some(s) = host_socket(handle) else { return -1 };
    let out = shim_slice_mut(ptr, max);
    s.recv(out, 0, false).map(|n| n as i32).unwrap_or(0)
}

/// A socket the host serves, whose accepted connections JS collects: the other
/// half of [`gex_host_connect`], letting two guest processes in two Workers
/// talk. JS holds a handle at each end and moves bytes between them.
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

/// The host's listeners, behind their own lock as [`HOST_SOCKETS`] is.
static HOST_LISTENERS: gex_kernel::sync::Mutex<Vec<Arc<JsListener>>> = gex_kernel::sync::Mutex::new(Vec::new());

/// Serve a unix socket at `path` from the host. Returns a listener id.
///
/// A guest connecting to it succeeds immediately; the connection waits in
/// this listener until [`gex_host_accept`] takes it.
#[no_mangle]
pub extern "C" fn gex_host_bind(path: u32, path_len: u32) -> i32 {
    let Ok(name) = std::str::from_utf8(shim_slice(path, path_len)) else {
        return -1;
    };
    let Some(home) = sigs::home() else { return -1 };
    let listener = Arc::new(JsListener::default());
    home.socket_names().bind_host(name, Arc::clone(&listener) as Arc<dyn gex_kernel::unix::HostListener>);
    // Also in the filesystem: `connect` goes through the name table, but
    // anything that checks the path exists first (Xlib, `test -S`; not
    // Mesa) needs a node. Through the mount, since with a global kernel the
    // root is this process's own `TmpFs` with nothing mounted on it.
    let _ = home.ns().mksocket(name);
    let mut table = HOST_LISTENERS.lock();
    table.push(listener);
    (table.len() - 1) as i32
}

/// Take one waiting connection, as a handle for [`gex_host_send`] and
/// [`gex_host_recv`]. -1 when nothing is waiting.
#[no_mangle]
pub extern "C" fn gex_host_accept(listener: i32) -> i32 {
    let l = HOST_LISTENERS.lock().get(listener.max(0) as usize).cloned();
    let Some(l) = l else { return -1 };
    let sock = l.accepted.lock().unwrap_or_else(|e| e.into_inner()).pop();
    let Some(sock) = sock else { return -1 };
    host_socket_add(sock)
}

/// How many regions of guest memory this process actually owns. Anything that
/// moves a process (a `fork` handing the child to another Worker) needs to know
/// which parts are real. Paired with [`gex_vma_at`]: count first, then ask.
#[no_mangle]
pub extern "C" fn gex_vma_count() -> i32 {
    named_mm().map_or(0, |mm| mm.lock().vmas().len() as i32)
}

/// The whole memory map as bytes, for a child to rebuild from. Each region's
/// name and code identity (file and offset) go too, or the child cannot find a
/// page in the shared store and re-translates its parent's code from cold.
///
///   u32 count, then per region:
///   u64 start, u64 end, u32 prot,
///   u32 name_len, name, u32 code_len, code, u64 code_off
///
/// A zero `code_len` means no code identity. `origin` is deliberately absent:
/// it is a claim about *this* process's mappings, not something to inherit.
fn vma_blob() -> Vec<u8> {
    let Some(mm) = named_mm() else { return 0u32.to_le_bytes().to_vec() };
    let mm = mm.lock();
    let vmas = mm.vmas();
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
    vma_blob().len() as u32
}

#[no_mangle]
pub extern "C" fn gex_vma_blob_read(out: u32, cap: u32) -> u32 {
    let blob = vma_blob();
    if blob.len() as u32 > cap {
        return 0;
    }
    shim_slice_mut(out, blob.len() as u32).copy_from_slice(&blob);
    blob.len() as u32
}

/// Rebuild the child's map from what [`vma_blob`] wrote.
#[no_mangle]
pub extern "C" fn gex_adopt_vma_blob(ptr: u32, len: u32) -> i32 {
    phase(2);
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
    // The map locks itself (no kernel lock, L25c).
    let mm = sigs::proc_home().mm();
    {
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
            ADOPTED_TOP.fetch_max(end, std::sync::atomic::Ordering::Release);
            mm.lock().adopt(start, end, prot, &name, code);
        }
        0
    }
}

/// The highest address this process inherited a mapping at, or zero if it
/// inherited none (it was not forked).
///
/// This is the floor a forked child's arena must start above, and knowing it
/// exactly is the difference between a child that can allocate and one that
/// cannot. See `arena_take`.
static ADOPTED_TOP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Describe region `i`, writing `start`, `end` and `prot` as three `u64` at
/// `out` in this module's own region.
///
/// Returns 0, or -1 if there is no such region. Three words at `out` rather
/// than a return value, since a wasm function hands back only one.
#[no_mangle]
pub extern "C" fn gex_vma_at(i: i32, out: u32) -> i32 {
    let Some(mm) = named_mm() else { return -1 };
    let found = mm.lock().vmas().get(i.max(0) as usize).map(|v| (v.start, v.end, v.prot as u64));
    let Some((start, end, prot)) = found else {
        return -1;
    };
    let dst = shim_slice_mut(out, 24);
    dst[0..8].copy_from_slice(&start.to_le_bytes());
    dst[8..16].copy_from_slice(&end.to_le_bytes());
    dst[16..24].copy_from_slice(&prot.to_le_bytes());
    0
}

/// The runs of 4 KiB pages in `[start, end)` that are not all zero, for a
/// fork's image (`#processImage` in gex.mjs): `(at, len)` u32 pairs at `out`,
/// at most `cap` of them. Returns how many, or -1 when they do not fit (the
/// caller asks again with more room). The scan was a JS loop over an
/// `Int32Array`, a word at a time; here it is 16 bytes a load.
#[no_mangle]
pub extern "C" fn gex_image_runs(start: u32, end: u32, out: u32, cap: u32) -> i32 {
    const PAGE: u32 = 4096;
    let dst = shim_slice_mut(out, cap.saturating_mul(8));
    let mut n = 0u32;
    let mut i = start;
    let mut run: Option<u32> = None;
    while i < end {
        let pe = end.min(i.saturating_add(PAGE));
        if unsafe { page_blank(i, pe) } {
            if let Some(at) = run.take() {
                if n == cap {
                    return -1;
                }
                let o = n as usize * 8;
                dst[o..o + 4].copy_from_slice(&at.to_le_bytes());
                dst[o + 4..o + 8].copy_from_slice(&(i - at).to_le_bytes());
                n += 1;
            }
        } else if run.is_none() {
            run = Some(i);
        }
        i = pe;
    }
    if let Some(at) = run {
        if n == cap {
            return -1;
        }
        let o = n as usize * 8;
        dst[o..o + 4].copy_from_slice(&at.to_le_bytes());
        dst[o + 4..o + 8].copy_from_slice(&(end - at).to_le_bytes());
        n += 1;
    }
    n as i32
}

/// Whether `[a, b)` of this memory is all zero bytes.
#[cfg_attr(target_arch = "wasm32", target_feature(enable = "simd128"))]
unsafe fn page_blank(a: u32, b: u32) -> bool {
    let p = |x: u32| std::ptr::with_exposed_provenance::<u8>(std::hint::black_box(x as usize));
    let mut i = a;
    while i < b && i % 16 != 0 {
        if unsafe { p(i).read_volatile() } != 0 {
            return false;
        }
        i += 1;
    }
    #[cfg(target_arch = "wasm32")]
    {
        use std::arch::wasm32::*;
        // Four vectors a test: an early out per 64 bytes is often enough.
        while i + 64 <= b {
            let q = p(i) as *const v128;
            let v = { v128_or(v128_or(v128_load(q), v128_load(q.add(1))), v128_or(v128_load(q.add(2)), v128_load(q.add(3)))) };
            if v128_any_true(v) {
                return false;
            }
            i += 64;
        }
    }
    while i < b {
        if unsafe { p(i).read_volatile() } != 0 {
            return false;
        }
        i += 1;
    }
    true
}

/// How much of the shim's heap is in use, in KiB. -1 without the counters.
///
/// Worth measuring because the heap is fixed, so exhausting it is a real
/// failure with no symptom of its own (the page faults at whatever the guest
/// touched next). Needs `--features heap-counters`, which charges every
/// allocation and is not what the page ships.
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
/// `total - used` is an upper bound on what is left, not the answer: talc hands
/// out contiguous spans, so a fragmented heap refuses allocations while this
/// still reports room. A lift failing with a gigabyte nominally free is a
/// fragmentation problem, not a size one.
#[no_mangle]
pub extern "C" fn gex_heap_total_kib() -> i32 {
    let base = (&raw const __heap_base) as usize;
    ((gex_abi::GUEST_MEM_BYTES as usize - base - 65536) / 1024) as i32
}

/// Reserve `len` bytes of this module's own heap and return a guest address
/// JS can write to. Used to hand the ELF across.
#[no_mangle]
pub extern "C" fn gex_alloc(len: u32) -> u32 {
    phase(8);
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
    // A Windows program has nothing placed here: the NT layer maps it and
    // Wine's ntdll below (see `nt::boot`, after the kernel is built).
    let nt_abi = nt::exe_abi(bytes);
    let program = match nt_abi {
        Some(abi) => nt::empty_program(abi),
        None => match gex_ld::load_program(bytes, gex_abi::PIE_BASE, INTERP_BASE, &read_file) {
            Ok(p) => p,
            Err(e) => {
                warn_line(&format!("gex_load: load_program failed: {e:?}"));
                return 0;
            }
        },
    };

    // The break follows the executable, as Linux and the native host have it:
    // `Program::brk` is above the interpreter too, and the interpreter sits
    // at INTERP_BASE == BRK_TOP, so a break there could never grow and every
    // dynamic program's malloc fell back to mmap -- a different path through
    // libc than the same program takes natively.
    let mut kernel = Kernel::new(gex_abi::ctx_addr(0), program.exe.brk, String::new());
    // A 32-bit (i386) program: i386 ABI in the kernel, compatibility-mode
    // decoding in the translator.
    kernel.compat = program.compat();
    gex_xlate::set_compat(program.compat());
    PROCESS_STARTED.store(now_ns(), std::sync::atomic::Ordering::Relaxed);
    // The clock calls' process constants (`clock_syscall`), before the
    // process's first syscall.
    kernel.started = now_ns() as u128;
    clock_constants(program.compat(), kernel.started);
    // And the same for the first process, which opens its console before it
    // makes a syscall. See the note beside the other one.
    set_my_pid(kernel.pid);
    // The staged layout, now that there is somewhere to put it. Taken rather
    // than copied: this runs once, and a second `gex_load` in the same Worker
    // is an `execve`, which gets its placement from the global table like
    // every other process.
    // The interpreter, placed by gex's loader rather than mapped by the guest,
    // announced as its `mmap` would have been: its function-tier unit
    // (`?elfunits=ld-linux-x86-64.so.2`) is placed after the first syscall.
    if let (Some(path), Some(_)) = (&program.exe.interp, &program.interp) {
        if let Some(bytes) = read_file(path) {
            kernel.mem.lock().log.new_code.extend(gex_kernel::CodeMap::of_placed(path, &bytes, program.interp_base()));
        }
    }
    kernel.pins = Arc::new(std::mem::take(&mut *PINS.0.borrow_mut()).into_iter().collect());
    // This host's memory has to be allocated up to whatever the map hands
    // out, so the map asks before it hands anything out.
    kernel.mem.lock().grows_on_demand(arena_grow, arena_back);
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
    kernel.ptys = global_ptys();
    kernel.tty = global_tty();
    match &shared_root {
        Some(r) => kernel.vfs.mount("/", r.clone()),
        None => kernel.vfs.mount("/", root.clone()),
    }
    // The graphics device, when the page said it has a canvas to draw on.
    // Absent otherwise, so a guest sees a machine with no GL rather than one
    // whose drawing vanishes.
    let wayland = mount_devices(&mut kernel);
    // `/tmp` in the root, not a mount of its own, so a page may bundle a file
    // at any path: a mount would shadow a bundled `/tmp/x`.
    root.add_dir("/tmp");
    // `/proc`: a program that cannot open `/proc/self/maps` gets a null
    // `FILE*` and faults at pc=0, blaming nothing.
    root.add_dir("/proc");
    let proc_state: Arc<gex_kernel::KernelProcSource> = Arc::new(gex_kernel::KernelProcSource::new());
    kernel.vfs.mount(
        "/proc",
        Arc::new(gex_kernel::vfs::ProcFs::new(proc_state.clone())),
    );
    kernel.clock = browser_clock;
    // And the clock a write stamps a file with.
    gex_kernel::vfs::tmpfs::set_clock(browser_clock);
    kernel.net = page_net().cloned();
    {
        let cwd = std::mem::take(&mut *CWD.0.borrow_mut());
        if !cwd.is_empty() {
            let _ = kernel.set_cwd(&cwd);
        }
    }
    kernel.creds.set_uid(unsafe { IDENTITY.0 });
    kernel.creds.set_gid(unsafe { IDENTITY.1 });
    // Replace the descriptors `Kernel::new` opened on `std::io` with ones the
    // host can actually see.
    install_console(&mut kernel);

    let data = guest_bytes();
    for (seg, from) in program.placements() {
        let (a, b) = (seg.vaddr as usize, (seg.vaddr + seg.mem_size) as usize);
        // Only what is not zero already: a segment's bss over memory nothing
        // touched would otherwise be committed, zeroes and all.
        gex_kernel::zero_dirty_bytes(&mut data[a..b]);
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
        // An image segment is built, not mapped (see `do_exec`), so it has no
        // file -- but it still gets a code identity when one exists, or the
        // next process re-lifts the same object. `gex_load` gets no path, so
        // the identity comes from the interpreter's name and, for the image
        // itself, from `IMAGE_PROGRAM`; `image_pages_are_ours` needs it before
        // it will hand this process the closure's image pages.
        let whose = from.map(|f| f.to_string()).or_else(|| {
            let w = IMAGE_PROGRAM.0.borrow();
            (!w.is_empty()).then(|| w.clone())
        });
        kernel.mem.lock().insert_loaded(
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
    // The environment pid 1 starts with, inherited by the whole run. Debian's
    // own default for root, with `sbin` (dpkg looks there for `ldconfig`); a
    // page overrides with `gex_add_env`.
    let env: Vec<String> = {
        let given = std::mem::take(&mut *ENV.0.borrow_mut());
        // Debian's defaults mean nothing to a Windows program.
        if given.is_empty() && nt_abi.is_none() {
            vec![
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
                "HOME=/root".to_string(),
            ]
        } else {
            given
        }
    };
    // `/proc/self/{cmdline,exe}` read these. A relative argv[0] is resolved
    // against the root, which is where a bundled program lives -- unless the
    // embedder named the program's path (`gex_set_exe`), as an ejected
    // project does: its argv[0] is what was recorded, and glibc's `$ORIGIN`
    // is the directory of `/proc/self/exe`, not of argv[0].
    let named = std::mem::take(&mut *EXE_PATH.0.borrow_mut());
    kernel.exe_path = if !named.is_empty() {
        named
    } else if args[0].starts_with('/') {
        args[0].clone()
    } else if args[0].contains('/') {
        // `./prog` names a path from the working directory, as execve's
        // argument would (tests/gexcc/longargs: `--argv0 ./longargs
        // --cwd /t` re-execs `/proc/self/exe`, which was `/longargs`).
        kernel.vfs.absolute(&args[0])
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
        // `STACK_TOP`, not the top of the addressable range: a stack at the
        // top makes the memory allocate up to it, four gigabytes per process.
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
    kernel.mem.lock().insert(
        gex_abi::STACK_TOP - gex_abi::STACK_SIZE,
        gex_abi::STACK_TOP,
        gex_kernel::mem::prot::READ | gex_kernel::mem::prot::WRITE,
        "[stack]",
    );

    let c = gex_abi::ctx_addr(0) as usize;
    // Registers first, then the stack pointer: on x86-64 `SP` is one of them.
    for i in 0..31u8 {
        write_u64(data, c + ctx::x(i), 0);
    }
    // (an i386-ABI process's is `esp` in every build: `ctx::sp_slot`)
    write_u64(data, c + ctx::sp_slot(program.compat()), layout.sp);
    write_u64(data, c + ctx::PC, program.entry());
    data[c + ctx::SAFEPOINT_COUNTER..c + ctx::SAFEPOINT_COUNTER + 4]
        .copy_from_slice(&1024i32.to_le_bytes());
    // The x87 control word and MXCSR at their power-on defaults. (`MXCSR` is
    // A64's `FPCR` slot, which starts at zero.)
    if gex_abi::X86 {
        data[c + ctx::X87_CW..c + ctx::X87_CW + 4].copy_from_slice(&0x37fu32.to_le_bytes());
        data[c + ctx::MXCSR..c + ctx::MXCSR + 4].copy_from_slice(&0x1f80u32.to_le_bytes());
    }
    if hook != 0 {
        snap_arm(&mut kernel, snap_key_1, &program.exe, program.interp_base());
    }

    let meta = gex_xlate::Metadata::decode(&METADATA.0.borrow()).unwrap_or_default();
    let entry = match nt_abi {
        Some(abi) => match nt::boot(&mut kernel, abi, &args, &env) {
            Ok(pc) => pc,
            Err(e) => {
                warn_line(&format!("gex_load: {e}\n"));
                return 0;
            }
        },
        None => program.entry(),
    };
    // A native wasm32 program: published before it runs (cgen/wasmprog.rs).
    cgen::wasmprog::stage(nt_abi.is_none().then_some(&program));
    install_jit(fresh_jit(&meta));
    set_host(Host {
        magic_head: HOST_MAGIC,
            kernel,
            wayland,
            root,
            proc_state,
            threads: threads_of(ThreadState::new(gex_abi::ctx_addr(0), 1), entry as u64, Waiting::Runnable),
            jit: JitHere,
            exec_waited_since: None,
        magic_tail: HOST_MAGIC,
    });
    entry
}

/// Place `path` at `base`: closure modules have their object's guest addresses
/// baked in, so the address is a precondition, and `ld.so` takes whatever
/// `mmap(NULL, span)` returns.
///
/// Two tables must agree: `Kernel::pins` for this process, and the global
/// placement table for every other, so a `fork`/`execve` of the same program
/// lands the library identically and can share what was built for it.
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
            // The span is not optional: told zero, the next auto-placed object
            // lands inside the pinned one and every module built for the
            // displaced library is then wrong.
            k_place_set(hasher.finish(), base, span);
        }
    }
}

/// Pins already reported, so each is said once and not once per syscall.
static PINS_SAID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Say where any newly reserved pin landed. A misplacement is loud whatever the
/// tracing settings -- the guest is about to branch into code built for another
/// address -- but a pin that took is only worth a line when tracing.
fn report_new_pins_in(h: &Host) {
    let said = PINS_SAID.load(std::sync::atomic::Ordering::Relaxed);
    let fresh = {
        let mm = h.kernel.mem.lock();
        mm.log.pin_landed.get(said..).map_or_else(Vec::new, <[_]>::to_vec)
    };
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

/// Where each pinned object actually landed. A pin that silently does not take
/// runs code at the wrong address, so a mismatch is written loudly.
#[no_mangle]
pub extern "C" fn gex_pin_report() {
    let landed = sigs::home().map(|h| h.mm().lock().log.pin_landed.clone()).unwrap_or_default();
    for (path, want, got) in landed {
        let how = if want == got { "ok" } else { "MISPLACED" };
        trace_line(&format!("[gex] pin {how}: {path} wanted {want:#x}, landed {got:#x}\n"));
    }
}

/// [`gex_add_file`] a piece at a time: `data` is the file's bytes from
/// `offset`, and offset zero starts the file. JS sends a large file this way
/// so that it never needs a buffer of the file's size in this module's heap:
/// the heap never gives a span back, and before a Windows program boots
/// whatever the heap has taken is address space the program cannot have
/// (`nt::boot`). Noita's music bank is 324 MiB.
#[no_mangle]
pub extern "C" fn gex_add_file_part(path: u32, path_len: u32, offset: u64, data: u32, data_len: u32) {
    if offset == 0 {
        gex_add_file(path, path_len, data, data_len);
        return;
    }
    if unsafe { k_present() } != 0 {
        if unsafe { k_fs_populated() } != 0 {
            return;
        }
        // SAFETY: scalars and pointers into this module's memory, which the
        // kernel copies from.
        unsafe {
            let h = k_fs_open(gex_kernel::PAddr::from(path), path_len, 1 /* O_WRONLY */, 0o644);
            if h < 0 {
                warn_line(&format!("gex_add_file_part: cannot reopen a staged file ({h})\n"));
                return;
            }
            let mut done = 0u32;
            while done < data_len {
                let n = k_fs_write(h, offset + done as u64, gex_kernel::PAddr::from(data + done), data_len - done);
                if n <= 0 {
                    warn_line("gex_add_file_part: a staged write failed\n");
                    break;
                }
                done += n as u32;
            }
            k_fs_close(h);
        }
        return;
    }
    let Ok(name) = std::str::from_utf8(shim_slice(path, path_len)) else {
        return;
    };
    let mut files = FILES.0.borrow_mut();
    match files.last_mut() {
        Some((n, bytes)) if n == name && bytes.len() as u64 == offset => {
            bytes.extend_from_slice(shim_slice(data, data_len));
        }
        _ => warn_line("gex_add_file_part: a piece out of order\n"),
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
    // Straight into the shared root, not staged: a wasm memory never gives
    // anything back, so staging the whole bundle in this process's memory first
    // cost the rootfs's size in every process that mounted it.
    if unsafe { k_present() } != 0 {
        if unsafe { k_fs_populated() } == 0 {
            unsafe { k_fs_add(gex_kernel::PAddr::from(path), path_len, gex_kernel::PAddr::from(data), data_len) };
        }
        return;
    }
    // No global kernel: this process is the whole system and its own `TmpFs`
    // is the filesystem. Staged as before, because that `TmpFs` does not
    // exist yet -- `gex_load` builds it.
    let bytes = shim_slice(data, data_len).to_vec();
    FILES.0.borrow_mut().push((name.to_string(), bytes));
}

/// A file the guest reads from `url` (same origin) a megabyte at a time, as
/// it reads (`TmpFs::add_lazy` in the global kernel): data too big to
/// bundle. Only with a global kernel; a path the bundle has is left alone.
#[no_mangle]
pub extern "C" fn gex_add_lazy(path: u32, path_len: u32, url: u32, url_len: u32, size: u64) {
    // SAFETY: scalars naming ranges of this module's memory, which the
    // kernel copies from.
    if unsafe { k_present() } != 0 {
        if unsafe { k_fs_populated() } == 0 {
            unsafe { k_fs_add_lazy(gex_kernel::PAddr::from(path), path_len, gex_kernel::PAddr::from(url), url_len, size) };
        }
        return;
    }
    warn_line("gex_add_lazy: no global kernel; a lazy file is left out\n");
}

/// A bundled hard link: `path` becomes another name for the already-added
/// `target`, one inode as in the bundle. glibc's loader knows a loaded
/// library by device and inode, so two copies where the host had two names
/// for one file load it twice (two `libGL`s, each with its own state).
/// Returns 1 when linked; 0 tells the caller to add a copy instead (no
/// global kernel: this process's filesystem does not exist yet).
#[no_mangle]
pub extern "C" fn gex_add_hardlink(target: u32, target_len: u32, path: u32, path_len: u32) -> i32 {
    // SAFETY: scalars naming ranges of this module's memory, no borrow held.
    if unsafe { k_present() } != 0 {
        if unsafe { k_fs_populated() } == 0 {
            return (unsafe { k_fs_link(gex_kernel::PAddr::from(target), target_len, gex_kernel::PAddr::from(path), path_len) } == 0) as i32;
        }
        return 1;
    }
    0
}

/// A bundled symbolic link: `path` names `target` (text, resolved when the
/// guest follows it), as in the bundle. A rootfs whose links became copies
/// carries a single-binary coreutils once per command name. Returns 1 when
/// made; 0 tells the caller to add a copy instead (no global kernel).
#[no_mangle]
pub extern "C" fn gex_add_symlink(target: u32, target_len: u32, path: u32, path_len: u32) -> i32 {
    // SAFETY: scalars naming ranges of this module's memory, no borrow held.
    if unsafe { k_present() } != 0 {
        if unsafe { k_fs_populated() } == 0 {
            return (unsafe { k_fs_symlink(gex_kernel::PAddr::from(target), target_len, gex_kernel::PAddr::from(path), path_len) } == 0) as i32;
        }
        return 1;
    }
    0
}

/// A bundled file's modification time, as the host's filesystem had it, so a
/// program that records one (`gzip`, `make`, `tar`) sees what it would
/// natively. Like `gex_chmod`.
#[no_mangle]
pub extern "C" fn gex_set_mtime(path: u32, path_len: u32, secs: i64) {
    let Ok(name) = std::str::from_utf8(shim_slice(path, path_len)) else {
        return;
    };
    // SAFETY: scalars naming a range of this module's memory, no borrow held.
    if unsafe { k_present() } != 0 {
        if unsafe { k_fs_populated() } == 0 {
            unsafe { k_fs_set_mtime(gex_kernel::PAddr::from(path), path_len, secs) };
        }
        return;
    }
    MTIMES.0.borrow_mut().push((name.to_string(), secs));
}

/// Make a bundled file executable: the VFS default is 0644 and the tar's mode
/// did not travel with its bytes, so dpkg reports present programs as missing.
/// Only modes differing from the default are sent.
#[no_mangle]
pub extern "C" fn gex_chmod(path: u32, path_len: u32, mode: u32) {
    let Ok(name) = std::str::from_utf8(shim_slice(path, path_len)) else {
        return;
    };
    // SAFETY: scalars naming a range of this module's memory, no borrow held.
    if unsafe { k_present() } != 0 {
        if unsafe { k_fs_populated() } == 0 {
            unsafe { k_fs_chmod(gex_kernel::PAddr::from(path), path_len, mode) };
        }
        return;
    }
    MODES.0.borrow_mut().push((name.to_string(), mode));
}

/// A directory the bundle contains, even an empty one: `gex_add_file` makes the
/// directories on the way to each file, but an empty directory has no file to
/// carry it and a program expecting one (apt's cache dirs) errors without it.
#[no_mangle]
pub extern "C" fn gex_add_dir(path: u32, path_len: u32) {
    let Ok(name) = std::str::from_utf8(shim_slice(path, path_len)) else {
        return;
    };
    // SAFETY: as `gex_add_file` -- scalars, no borrow held.
    if unsafe { k_present() } != 0 {
        if unsafe { k_fs_populated() } == 0 {
            unsafe { k_fs_mkdir(gex_kernel::PAddr::from(path), path_len, 0o755) };
        }
        return;
    }
    DIRS.0.borrow_mut().push(name.to_string());
}

/// The directory the closure's modules published, kept so an `execve` can have
/// it back. `do_exec` clears the guest's dispatch directory (right for
/// per-program translations), but a closure's modules are still in the
/// dispatch tables `execve` does not touch -- only the directory pointing at
/// them was lost. Safe to replay because the layout is pinned.
static CLOSURE_DIRS: SingleThreaded<Vec<(u64, u32)>> =
    SingleThreaded(const { RefCell::new(Vec::new()) });

/// Which closure library covers each page, in add order, so the host
/// instantiates a library only when a branch reaches it rather than all at
/// process start (37s of a 61s apt run, for libraries it mostly never calls).
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
    // Under the JIT lock, which keeps the closure maps (`IMAGE_PAGES`,
    // `CLOSURE_OWNER`, ...): `gex_add_closure_module` writes them as libraries
    // stream in, and this runs on every Worker's miss path.
    with_px(|px| with_jit_only(|_| closure_owner_held(px, page)))
}

/// Which closure object owns an image page, for a caller that holds the JIT
/// lock (the closure maps are under it).
///
/// Once this said "KLOCK": that lock was not re-entrant, and
/// `gex_fill_table_for` calling the exported `gex_closure_owner` under it
/// killed a forked child filling its first image page ("the kernel was
/// re-entered"). `with_jit_only` nests, but a holder may as well ask here.
fn closure_owner_held(p: &Px<'_>, page: u64) -> i32 {
    if page * gex_abi::DISPATCH_PAGE < gex_abi::INTERP_BASE {
        let claims = IMAGE_PAGES.0.borrow();
        let Some(idxs) = claims.get(&page) else {
            return -1;
        };
        return idxs
            .iter()
            .find(|i| image_is_ours(p, **i))
            .map(|i| *i as i32)
            .unwrap_or(-1);
    }
    CLOSURE_OWNER
        .0
        .borrow()
        .get(&page)
        .map(|v| *v as i32)
        .unwrap_or(-1)
}

/// The `k`th closure object covering `page` that belongs to this process.
/// `gex_closure_owner` gives only the first, which is not enough for an image:
/// a program's translation is split by block, not page, so one page can be
/// covered by several sibling objects and asking only the first strands the
/// rest of the page on the translator.
#[no_mangle]
pub extern "C" fn gex_closure_owner_at(page: u64, k: u32) -> i32 {
    if page * gex_abi::DISPATCH_PAGE < gex_abi::INTERP_BASE {
        // Under the JIT lock: `IMAGE_PAGES` is written at runtime by streaming
        // closures.
        return with_px(|px| with_jit_only(|_| {
            let claims = IMAGE_PAGES.0.borrow();
            let Some(idxs) = claims.get(&page) else {
                return -1;
            };
            idxs.iter()
                .filter(|i| image_is_ours(px, **i))
                .nth(k as usize)
                .map(|i| *i as i32)
                .unwrap_or(-1)
        }));
    }
    // Above the interpreter a page number is unique to an object, so there is
    // never a second owner to ask.
    if k != 0 {
        return -1;
    }
    // Under the JIT lock, like the branch above: `CLOSURE_OWNER` is a
    // `SingleThreaded<RefCell>` in shared memory that `gex_add_closure_module`
    // `borrow_mut`s as libraries stream in. This runs on every Worker's miss
    // path; unlocked, its `borrow` here races that writer on the RefCell's
    // non-atomic borrow flag (the flag lives in shared memory, so a concurrent
    // reader and writer corrupt it) -> a spurious "already mutably borrowed"
    // panic surfacing at whichever access reads the corrupted flag next.
    with_jit_only(|_| {
        CLOSURE_OWNER
            .0
            .borrow()
            .get(&page)
            .map(|v| *v as i32)
            .unwrap_or(-1)
    })
}

/// Where the closure build says each object goes, staged until there is a
/// `Host`: `gex_pin_object` runs before `gex_load` (the layout must precede the
/// loader), when no `Host` exists yet. The global table is told straight away.
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

/// Bundled files' modification times. See `gex_set_mtime`.
static MTIMES: SingleThreaded<Vec<(String, i64)>> =
    SingleThreaded(const { RefCell::new(Vec::new()) });

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
    // And the modes and times, after the files exist to carry them.
    // The page names them as the guest does, with a leading slash; the tmpfs
    // keys its nodes without one (it is mounted at `/`).
    for (name, mode) in std::mem::take(&mut *MODES.0.borrow_mut()) {
        let _ = gex_kernel::vfs::FileSystem::chmod(root, name.trim_start_matches('/'), mode);
    }
    for (name, secs) in std::mem::take(&mut *MTIMES.0.borrow_mut()) {
        let _ = gex_kernel::vfs::FileSystem::set_mtime(root, name.trim_start_matches('/'), secs);
    }
}

/// Where the dynamic linker is placed, clear of the executable's mappings. The
/// same address `gexc` uses, so a program looks identical under both hosts.
/// Inside the fixed low region, image and break below, stack above.
const INTERP_BASE: u64 = gex_abi::INTERP_BASE;

/// A file read back out of the guest's filesystem, kept alive for JS.
static EXTRACTED: SingleThreaded<Vec<u8>> = SingleThreaded(const { RefCell::new(Vec::new()) });

/// Read a file out of the guest's filesystem, and return its length. The other
/// half of bundling (a page running a compiler wants its output back). Zero is
/// both empty and missing, which is all a caller can act on.
#[no_mangle]
pub extern "C" fn gex_get_file(path: u32, path_len: u32) -> u32 {
    let Ok(name) = std::str::from_utf8(shim_slice(path, path_len)) else {
        return 0;
    };
    let bytes = sigs::home().and_then(|h| h.read_file(name)).unwrap_or_default();
    let len = bytes.len() as u32;
    *EXTRACTED.0.borrow_mut() = bytes;
    len
}

/// Where the last [`gex_get_file`] left its contents.
#[no_mangle]
pub extern "C" fn gex_file_data() -> u32 {
    EXTRACTED.0.borrow().as_ptr() as u32
}

/// The identity the guest is given; the embedder may override before loading.
/// A page has no user, so 1000; Node passes its own to match QEMU.
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

/// The module's dispatch metadata, staged until the guest is loaded: from the
/// module's `gex.meta` section, so runtime translation continues allocating
/// table slots where the ahead-of-time build left off.
static METADATA: SingleThreaded<Vec<u8>> = SingleThreaded(const { RefCell::new(Vec::new()) });

#[no_mangle]
pub extern "C" fn gex_set_metadata(ptr: u32, len: u32) {
    phase(7);
    *METADATA.0.borrow_mut() = shim_slice(ptr, len).to_vec();
}

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
fn current_image_program(p: &Px<'_>) -> Option<String> {
    p.mm
        .lock()
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
    with_px(|px| with_jit_only(|_| {
        // Under the JIT lock: these three maps are written at runtime by
        // `gex_add_closure_module` on other Workers' fill paths.
        let claims = IMAGE_PAGES.0.borrow();
        let idxs = claims.get(&page).cloned().unwrap_or_default();
        let tables = IMAGE_TABLE.0.borrow();
        let progs = LIB_PROGRAM.0.borrow();
        let have = current_image_program(px);
        let mut s = format!(
            "[gex] page {page}: running {:?}, {} candidate(s):",
            have.as_deref().unwrap_or("<none>"),
            idxs.len()
        );
        for i in &idxs {
            s.push_str(&format!(
                " [{i} {:?} ours={} table={:?}]",
                progs.get(*i).map(|p| p.as_str()).unwrap_or("<unnamed>"),
                image_is_ours(px, *i),
                tables.get(&(*i, page)),
            ));
        }
        s.push('\n');
        warn_line(&s);
    }));
}

/// Whether image library `idx` is the one this process is running.
fn image_is_ours(p: &Px<'_>, idx: usize) -> bool {
    let progs = LIB_PROGRAM.0.borrow();
    let Some(want) = progs.get(idx).filter(|p| !p.is_empty()) else {
        return false;
    };
    let have = current_image_program(p);
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

/// Take a module the closure build made ahead of time for one of this
/// program's libraries. It is `Linkage::Shared` and fills the tables itself, so
/// only its metadata is needed; the build's bases are safe unchecked because
/// one `gexc` `PageTable` allocated the whole closure. Returns the slot count
/// the table must reach, or zero if the metadata could not be read (then the
/// caller must not install it).
#[no_mangle]
pub extern "C" fn gex_add_closure_module(ptr: u32, len: u32, prog: u32, prog_len: u32) -> u32 {
    let Some(meta) = gex_xlate::Metadata::decode(shim_slice(ptr, len)) else {
        return 0;
    };
    // From registration on, this process holds a module that can fill pages,
    // so the miss path may spend a trap asking. Armed here, not at instantiate:
    // a closure library is not instantiated until a fill asks for it, so
    // arming on instantiate would deadlock (nothing armed -> no miss asks ->
    // nothing instantiates). `fillSlot` instantiates it on the way.
    FILL_POSSIBLE.store(true, std::sync::atomic::Ordering::Relaxed);
    // Under the JIT lock: closure libraries stream in AFTER the process is up
    // and the guest (with its threads) is running (see `installClosureModule`),
    // so these `SingleThreaded<RefCell>` maps are written here while other
    // Workers read them on the dispatch-miss path (`gex_closure_owner`,
    // `gex_page_table`, `gex_lib_lazy`). Unlocked, that read-vs-write races the
    // BTreeMaps' structure -- the true-parallelism stack smash. Those readers
    // lock the same way. The maps are translation state, so the lock is the
    // JIT's, not the kernel's (docs/kernel-locking.md §3.6, L16); it nests.
    with_jit_only(|_| {
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
        // page back in it: only the table survives per-process numbering
        // (`table.init` takes it as an immediate), the offset within it is this
        // process's to choose. Demand-filled libraries only -- a small module
        // writes absolute slots at instantiation and keeps the build's numbers.
        if claim_pages() {
            let mut tables = CLOSURE_TABLE.0.borrow_mut();
            let mut image_tables = IMAGE_TABLE.0.borrow_mut();
            for (page, base) in &meta.dir_entries {
                let t = gex_abi::dispatch_entry_table(*base) as u32;
                if page * gex_abi::DISPATCH_PAGE >= gex_abi::INTERP_BASE {
                    tables.insert(*page, t);
                } else {
                    // Keyed by module too: every closure root is an image at
                    // `PIE_BASE`, so several name the same page and only one is
                    // this process's (`image_is_ours`).
                    image_tables.insert((idx, *page), t);
                }
            }
            LIB_LAZY.0.borrow_mut().insert(idx as u32);
        }
    });
    // Registered, not published. See `gex_publish_closure_module`.
    0
}

/// The program's guest path, when the embedder knows it. See `gex_load`.
static EXE_PATH: SingleThreaded<String> = SingleThreaded(const { RefCell::new(String::new()) });

#[no_mangle]
pub extern "C" fn gex_set_exe(ptr: u32, len: u32) {
    if let Ok(s) = std::str::from_utf8(shim_slice(ptr, len)) {
        *EXE_PATH.0.borrow_mut() = s.to_string();
    }
}

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

/// Whether this process runs the program the image module was built from, and
/// may be given its pages. Every image sits at `PIE_BASE`, so image pages are
/// the one part of a closure not true whatever the process goes on to run;
/// handing apt-get bash's translated blocks runs someone else's code and dies.
///
/// Asked of the image's code identity, not a flag, so it stays true across a
/// fork that has not exec'd. Refused only on a *positive* mismatch: an unknown
/// identity is not evidence (refusing on it stranded a forked shell).
fn image_pages_are_ours(h: &Host) -> bool {
    let want = IMAGE_PROGRAM.0.borrow();
    if want.is_empty() {
        return true;
    }
    match h
        .kernel
        .mem
        .lock()
        .vmas()
        .iter()
        .find(|v| v.start <= gex_abi::PIE_BASE && gex_abi::PIE_BASE < v.end)
        .and_then(|v| v.code.as_ref())
    {
        Some((p, _)) => p.as_str() == want.as_str(),
        None => true,
    }
}

/// Put a registered closure library's pages into the dispatch directory. Split
/// from registration because it costs: a written entry is a table that must be
/// grown to hold its slot. Unpublished is not unreachable -- the entry stays
/// zero (the miss stub), so a branch there asks for a fill that publishes,
/// grows, instantiates and fills before resuming.
#[no_mangle]
pub extern "C" fn gex_publish_closure_module(ptr: u32, len: u32, idx: u32) -> u32 {
    let Some(meta) = gex_xlate::Metadata::decode(shim_slice(ptr, len)) else {
        return 0;
    };
    // Kept because `execve` republishes: the modules are still in this
    // instance's tables, so the new program gets every library the old one had.
    with_px(|px| with_jit_only(|j| {
        // Not the image's pages unless this process runs *this* image's
        // program (every closure root is an image at `PIE_BASE`, so the
        // question is which, not whether).
        let ours = image_is_ours(px, idx as usize);
        let mut gm = GuestMem::new(guest_bytes());
        for (page, base) in &meta.dir_entries {
            if page * gex_abi::DISPATCH_PAGE < gex_abi::INTERP_BASE && !ours {
                continue;
            }
            // Under per-process numbering, entries are not written here: `gexc`
            // numbers slots in one closure-wide sequence, so writing a library's
            // entries would grow the table to its highest slot (2.7M for `cat
            // /dev/null`). The entry stays zero (the miss stub) until a branch
            // lands, and `gex_claim_page` then gives it a base from this
            // process's numbering. Images too -- a demand-filled image writes
            // nothing until a fill asks, so its pages claim like any library's.
            if claim_pages() {
                continue;
            }
            CLOSURE_DIRS.0.borrow_mut().push((*page, *base));
            j.page_table.assign(*page, *base);
            // And the guest's own directory, which a branch reads: a `Shared`
            // module has no memory to publish its own from, so the host writes.
            dir_note(*page, *base, "pin-meta");
            let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, *base);
        }
    }));
    1
}

/// The first slot in each table that no image can write. An image's element
/// segment is active and absolute, writing its blocks at `gexc`'s slots on
/// instantiation, so a claim starting at `table.length` could be overwritten by
/// the next `exec`. Every image is registered before any guest runs, so claims
/// start above the highest slot any of them reaches.
static IMAGE_HIGH: SingleThreaded<[u32; gex_abi::DISPATCH_TABLES]> =
    SingleThreaded(std::cell::RefCell::new([0; gex_abi::DISPATCH_TABLES]));

/// Where this process may start handing out slots in table `i`.
#[no_mangle]
pub extern "C" fn gex_claim_floor(i: u32) -> u32 {
    // Under the JIT lock: `IMAGE_HIGH` is written at runtime by streaming
    // closures.
    with_jit_only(|_| IMAGE_HIGH.0.borrow().get(i as usize).copied().unwrap_or(0))
}

/// Whether pages are numbered by this process or by the build.
///
/// Off by default. See `gex_set_claim_pages`.
static CLAIM_PAGES: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// Turn per-process page numbering on. The build numbers slots in one
/// closure-wide sequence, so a process branching into a late-numbered library
/// grows its table to that length whatever it uses (2.7M slots for `cat
/// /dev/null`); claiming a page at a time takes that to 52,224. Not finished: a
/// module reused from an earlier process carries that process's slot numbers,
/// unreconciled.
#[no_mangle]
pub extern "C" fn gex_set_claim_pages(on: i32) {
    CLAIM_PAGES.store(on != 0, std::sync::atomic::Ordering::Relaxed);
}

fn claim_pages() -> bool {
    CLAIM_PAGES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Libraries whose slots are claimed a page at a time: the big ones `gexc`
/// gives passive element segments and a fill helper. A small one writes its
/// slots at instantiation and keeps its build numbering.
static LIB_LAZY: SingleThreaded<std::collections::BTreeSet<u32>> =
    SingleThreaded(std::cell::RefCell::new(std::collections::BTreeSet::new()));

/// Whether library `idx` is claimed page by page, for a host deciding how
/// long its table must be before instantiating it.
#[no_mangle]
pub extern "C" fn gex_lib_lazy(idx: u32) -> i32 {
    // Under the JIT lock: `LIB_LAZY` is written at runtime by streaming closures.
    with_jit_only(|_| LIB_LAZY.0.borrow().contains(&idx) as i32)
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
    // Whole body under the JIT lock: `CLOSURE_TABLE`/`IMAGE_TABLE` are written
    // at runtime by streaming closures (`gex_add_closure_module`) while this
    // runs on every Worker's fill path.
    with_px(|px| with_jit_only(|_| {
        // An image page belongs to whichever root of the closure this process is
        // actually running, so it is looked up by module and not by page alone.
        if page * gex_abi::DISPATCH_PAGE < gex_abi::INTERP_BASE {
            let owner = closure_owner_held(px, page);
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
    }))
}

/// Which dispatch table `gexc` built each image page for, per image. Separate
/// from `CLOSURE_TABLE` because image pages collide: every closure root sits at
/// `PIE_BASE` and names the same pages.
static IMAGE_TABLE: SingleThreaded<std::collections::BTreeMap<(usize, u64), u32>> =
    SingleThreaded(std::cell::RefCell::new(std::collections::BTreeMap::new()));

/// Per-table base allocator, shared by all of a process's Worker threads (a
/// shim static lives in the shared linear memory; a `fork` copies it, so it is
/// per process). Numbering MUST be process-wide, not per-Worker: the dispatch
/// directory it fills is process-wide, and a per-Worker counter (the old
/// JS-side `#claimed`) let two threads claiming the same fresh page pick
/// *divergent* bases -- the loser of the directory write then dispatched that
/// page through a slot its own table had filled with a different page's block,
/// running wrong code against the live guest stack. That is the intermittent
/// multithreaded-GTK stack smash, which only reproduced under true parallelism
/// and vanished under any slowdown (the claim then serialised).
static CLAIM_NEXT: [std::sync::atomic::AtomicU32; gex_abi::DISPATCH_TABLES] =
    [const { std::sync::atomic::AtomicU32::new(0) }; gex_abi::DISPATCH_TABLES];

/// The highest slot END any directory entry names, per table, across every
/// Worker of this process. Every base written into the shared dispatch directory
/// (by any base-assignment path -- claim, JIT miss, lazy/pin/restore) is folded
/// in here via `dir_note`, so a Worker can grow its own funcref table to cover a
/// page a *sibling* numbered. Without it the directory names a slot this Worker's
/// table has not grown to and `return_call_indirect` traps "table index out of
/// bounds" -- the concurrent-JIT / stale-dispatch race. CLAIM_NEXT alone is not
/// enough: the JIT miss path numbers pages from the system-wide `shared_slot` map,
/// not `gex_claim_page`, so its slots never bump CLAIM_NEXT.
static DIR_HIGH: [std::sync::atomic::AtomicU32; gex_abi::DISPATCH_TABLES] =
    [const { std::sync::atomic::AtomicU32::new(0) }; gex_abi::DISPATCH_TABLES];

/// Fold a base written to the directory into `DIR_HIGH`. The table is the entry's
/// table field; the slot is its offset within that table, so the size the table
/// must reach is that offset plus a page of slots.
fn note_dir_high(base: u32) {
    let t = gex_abi::dispatch_entry_table(base);
    let end = (base & gex_abi::DISPATCH_SLOT_MASK) + gex_abi::SLOTS_PER_PAGE as u32;
    if let Some(c) = DIR_HIGH.get(t) {
        c.fetch_max(end, std::sync::atomic::Ordering::AcqRel);
    }
}

/// The size table `i` must reach to dispatch any page the directory names --
/// max of the claim allocator's high-water and every directory entry's. The run
/// loop in gex.mjs grows this Worker's funcref table to this and retries a
/// "table index out of bounds" dispatch. See `DIR_HIGH`.
#[no_mangle]
pub extern "C" fn gex_claimed_slots(i: u32) -> u32 {
    let claimed = CLAIM_NEXT
        .get(i as usize)
        .map(|c| c.load(std::sync::atomic::Ordering::Acquire))
        .unwrap_or(0);
    let named = DIR_HIGH
        .get(i as usize)
        .map(|c| c.load(std::sync::atomic::Ordering::Acquire))
        .unwrap_or(0);
    claimed.max(named)
}

/// Number a page in this process's directory and return its base slot, doing it
/// atomically so a concurrent claim of the *same* page cannot hand two threads
/// two different bases. If another thread numbered it first its base is adopted
/// and the range this thread reserved is left unused (harmless: nothing maps to
/// it). The caller grows its own (per-instance) table to cover the base.
#[no_mangle]
pub extern "C" fn gex_claim_page(page: u64) -> u32 {
    use std::sync::atomic::Ordering::{AcqRel, Acquire};
    let table = match gex_page_table(page) {
        t if t >= 0 => t as usize,
        _ => return 0,
    };
    // The directory word as an atomic in the shared linear memory. The guest
    // address space starts at linear address 0 (see `guest_bytes`), so the
    // guest address is the native address; `with_exposed_provenance` keeps LLVM
    // from folding it to a null-derived pointer.
    let addr = (gex_abi::DISPATCH_DIR + page * 4) as usize;
    let dir = unsafe {
        &*(std::ptr::with_exposed_provenance::<u32>(addr)
            as *const std::sync::atomic::AtomicU32)
    };
    let existing = dir.load(Acquire);
    if existing != 0 {
        return existing & gex_abi::DISPATCH_SLOT_MASK;
    }
    // Keep the counter at or above this table's floor: image slots and the
    // reserved miss span sit below it, and base 0 is the "no translation"
    // sentinel the branchless lookup relies on.
    let floor = gex_claim_floor(table as u32).max(gex_abi::MISS_SLOTS as u32);
    CLAIM_NEXT[table].fetch_max(floor, AcqRel);
    let base = CLAIM_NEXT[table].fetch_add(gex_abi::SLOTS_PER_PAGE as u32, AcqRel);
    let entry = gex_abi::dispatch_entry(table, base);
    match dir.compare_exchange(0, entry, AcqRel, Acquire) {
        Ok(_) => {
            dir_note(page, entry, "claim-page");
            // Under the JIT lock: `page_table` and `CLOSURE_DIRS` are process
            // state two Workers must not push to at once (the second race the
            // audit found).
            with_jit_only(|j| {
                j.page_table.assign(page, entry);
                CLOSURE_DIRS.0.borrow_mut().push((page, entry));
            });
            base
        }
        // Another thread numbered it first between our load and CAS: adopt its
        // base, drop our reserved range.
        Err(won) => won & gex_abi::DISPATCH_SLOT_MASK,
    }
}

/// The dispatch page whose directory writes should be narrated, or -1.
static DIR_WATCH: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);

#[no_mangle]
pub extern "C" fn gex_watch_dir(page: i64) {
    DIR_WATCH.store(page, std::sync::atomic::Ordering::Relaxed);
}

/// Say who is about to change a watched page's directory entry, to catch a page
/// dispatched through a base other than the one its slots were written at.
fn dir_note(page: u64, entry: u32, who: &str) {
    // Every directory write passes through here, so it is where the process-wide
    // per-table high-water is kept: fold this base in before the watch gate so a
    // sibling Worker can grow its table to cover this page. See `DIR_HIGH`.
    note_dir_high(entry);
    dense::note_entry(entry);
    let want = DIR_WATCH.load(std::sync::atomic::Ordering::Relaxed);
    if want < 0 || want as u64 != page {
        return;
    }
    let gm = GuestMem::new(guest_bytes());
    let had = gm.read_u32(gex_abi::DISPATCH_DIR + page * 4).unwrap_or(0);
    if had != entry {
        // With the pid, to tell one process renumbering from several numbering
        // independently. `my_pid` reads a static: this runs inside `with_exec_lock`
        // closures and must not borrow.
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

/// The slot a page's dispatch entry points at, in this process -- what
/// `gexfillat` is told. The directory is per process, so a fill writes where it
/// says rather than where the build guessed.
#[no_mangle]
pub extern "C" fn gex_page_base(page: u32) -> u32 {
    let gm = GuestMem::new(guest_bytes());
    gm.read_u32(gex_abi::DISPATCH_DIR + page as u64 * 4)
        .unwrap_or(0)
        & gex_abi::DISPATCH_SLOT_MASK
}

/// How long dispatch table `i` must be for what this process has published. Per
/// table, not one total: closures own tables 0..3 with baked-in slot numbers
/// and the run translates into 4..7, so a single total would grow 0..3 hugely.
#[no_mangle]
pub extern "C" fn gex_table_need(i: u32) -> u32 {
    // Without the lock (docs/kernel-locking.md, step 4): the per-table marks
    // are atomics the page table shares with every clone, published once per
    // table (`regview`). JS asks this once per table per install.
    match regview::table_need(i as usize) {
        Some(n) => n as u32,
        None => with_jit_only(|j| {
            regview::publish_marks(&j.page_table);
            j.page_table.table_need(i as usize) as u32
        }),
    }
}

/// Register a module a forked child inherited, without instantiating it. A
/// separate list from `gex_add_closure_module` because a runtime-translated
/// unit covers whatever the old program mapped, so `execve` must drop it (a
/// closure library it keeps). Directory entries are published now (the child's
/// inherited directory already names these pages); the instance waits for a
/// branch, since the child touches almost none of its inherited history.
#[no_mangle]
pub extern "C" fn gex_add_lazy_module(ptr: u32, len: u32) -> u32 {
    let Some(meta) = gex_xlate::Metadata::decode(shim_slice(ptr, len)) else {
        return 0;
    };
    // Registered now, instantiated on the branch that wants it, so the miss
    // path has somewhere to ask from here on.
    FILL_POSSIBLE.store(true, std::sync::atomic::Ordering::Relaxed);
    let idx = LAZY_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // The JIT lock alone: the page table and `LAZY_OWNER` are its.
    with_jit_only(|j| {
        let mut gm = GuestMem::new(guest_bytes());
        let mut owner = LAZY_OWNER.0.borrow_mut();
        for (page, base) in &meta.dir_entries {
            j.page_table.assign(*page, *base);
            dir_note(*page, *base, "register-module");
            let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, *base);
            owner.insert(*page, idx);
        }
        j.page_table.advance_to(meta.next_slot);
        j.page_table.slots() as u32
    })
}

/// The inherited module covering this page, or -1.
#[no_mangle]
pub extern "C" fn gex_lazy_owner(page: u64) -> i32 {
    // Under the JIT lock: `gex_add_lazy_module` writes `LAZY_OWNER` (a
    // BTreeMap) at runtime as lazy modules install, and this read is on the
    // dispatch-miss path of every Worker -- an unlocked `borrow()` racing that
    // `borrow_mut()` reads the tree mid-rebalance and follows a stale node
    // pointer.
    with_jit_only(|_| {
        LAZY_OWNER
            .0
            .borrow()
            .get(&page)
            .map(|v| *v as i32)
            .unwrap_or(-1)
    })
}

/// Throw away a forked child's inherited dispatch directory and put back only
/// the entries it can honour: a child deferring its parent's modules must not
/// claim slots it has not filled, or a branch bets on whatever the slot holds.
///
/// Zeroing alone is not enough -- the guest module publishes its own entries
/// from a passive segment guarded by an atomic the copied memory already set,
/// so it will never re-publish them, and an erased child cannot run its first
/// instruction. So erase, then write back what the metadata says this image
/// covers.
#[no_mangle]
pub extern "C" fn gex_reset_directory() -> u32 {
    phase(6);
    let meta = gex_xlate::Metadata::decode(&METADATA.0.borrow()).unwrap_or_default();
    let data = guest_bytes();
    let from = gex_abi::DISPATCH_DIR as usize;
    let to = LOW_END as usize;
    gex_kernel::zero_dirty_bytes(&mut data[from..to]);
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

/// A dispatch slot the host is being asked to write, or -1. A module's element
/// segments are passive, so its blocks reach the table only via `gexfillat`;
/// when a branch misses on a page this process already has a base for, the code
/// exists but is unwritten, so ask for the slot rather than re-lift the block.
// Per Worker: set on the way into a trap or a check and read back by the
// same Worker (see `CTX_ADDR` for what a process-wide one did).
thread_local! {
    static FILL_WANTED: std::cell::Cell<i64> = const { std::cell::Cell::new(-1) };
}

/// The fill's memory of what it asked, under its own lock: misses on every
/// Worker of the process ask and answer at once (a lock-free `RefCell` here
/// raced the trees' structure and smashed a live stack).
struct FillAsked {
    /// Spans already asked about, so one nobody can fill is asked once and
    /// then takes the ordinary path rather than asking forever.
    asked: std::collections::BTreeSet<u64>,
    /// Pages asked about that nobody could fill. Not permanent: the covering
    /// module may just not be installed in this process yet (the ordinary
    /// case for a forked child), so `gex_fill_possible` forgives these when
    /// a module arrives.
    failed: std::collections::BTreeSet<u64>,
}

static FILL_ASKED: gex_kernel::sync::Mutex<FillAsked> = gex_kernel::sync::Mutex::new(FillAsked {
    asked: std::collections::BTreeSet::new(),
    failed: std::collections::BTreeSet::new(),
});

/// Whether the host is holding any module unwritten. Without it the miss path
/// traps into JS for a fill on the first miss of every page whose base it has,
/// even when nothing is deferred (114s vs 82s on `apt-get update`). Set when a
/// module is first deferred.
static FILL_POSSIBLE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);


/// Pages already served whole from the shared store for a fault, so a second
/// fault on one is not served again. A served batch covers whatever the process
/// that built it reached; when that does NOT include the faulting block -- a
/// forked child's own post-`fork` block, which no ancestor ever ran -- serving
/// resumes the guest onto the same empty slot, it faults again, and the miss
/// path serves the same batch forever. A nested `$(...)` wedged its whole
/// process tree here. Serving a page once and then falling through to translate
/// bounds it: the wasted serve is one turn, and the translate produces the
/// block that was missing.
/// Each page's base under which this process has installed blocks: lifted
/// here, or taken from the store. A stored module numbering such a page
/// otherwise is not served (see the serve filter in `gex_dispatch_miss`).
static LIVE_BASE: SingleThreaded<std::collections::BTreeMap<u64, u32>> =
    SingleThreaded(const { RefCell::new(std::collections::BTreeMap::new()) });

fn note_live(dirs: &[(u64, u32)]) {
    let mut m = LIVE_BASE.0.borrow_mut();
    for (p, b) in dirs {
        m.insert(*p, *b);
    }
}

static SERVED_PAGE: SingleThreaded<std::collections::BTreeSet<u64>> =
    SingleThreaded(const { RefCell::new(std::collections::BTreeSet::new()) });

/// The page the host is being asked about, so the answer can be attributed.
// Per Worker: set on the way into a trap or a check and read back by the
// same Worker (see `CTX_ADDR` for what a process-wide one did).
thread_local! {
    static FILL_PENDING: std::cell::Cell<i64> = const { std::cell::Cell::new(-1) };
}

/// The host is holding a module whose slots are not written yet.
///
/// Turning this on also forgives every page that could not be filled before,
/// because the module just installed may be the one that covers them.
#[no_mangle]
pub extern "C" fn gex_fill_possible(on: i32) {
    FILL_POSSIBLE.store(on != 0, std::sync::atomic::Ordering::Relaxed);
    if on != 0 {
        let mut f = FILL_ASKED.lock();
        let failed = std::mem::take(&mut f.failed);
        let spans = gex_abi::SLOTS_PER_PAGE as u64
            / FILL_WIDTH.load(std::sync::atomic::Ordering::Relaxed) as u64;
        for page in failed {
            // Every span of it: the module that has just arrived may cover
            // parts of the page this one could not.
            for i in 0..spans {
                f.asked.remove(&(page * spans + i));
            }
        }
    }
}

/// How far into the page the fill should start, in slots.
///
/// A fill writes `FILL_SPAN` slots from here rather than the whole page. See
/// `gex_abi::FILL_SPAN` for why.
// Per Worker: set on the way into a trap or a check and read back by the
// same Worker (see `CTX_ADDR` for what a process-wide one did).
thread_local! {
    static FILL_FROM: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The address that missed, for the fill the host is about to run. `FILL_FROM`
/// is it rounded to a span, which the module needs; this exact pc is how to see
/// whether the one slot the guest dispatches through came back written.
// Per Worker: set on the way into a trap or a check and read back by the
// same Worker (see `CTX_ADDR` for what a process-wide one did).
thread_local! {
    static FILL_PC: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[no_mangle]
pub extern "C" fn gex_fill_pc() -> u64 {
    FILL_PC.with(|c| c.get())
}

/// Slots a fill writes: `FILL_SPAN` normally, a whole page for a closure built
/// before `gexfillat` took a `from` (it ignores the argument and writes all
/// 1,024). Set from the export's arity, so a new shim stays usable against an
/// un-rebuilt closure.
static FILL_WIDTH: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(gex_abi::FILL_SPAN);

#[no_mangle]
pub extern "C" fn gex_set_fill_width(n: u32) {
    let n = if n == 0 { gex_abi::FILL_SPAN } else { n };
    FILL_WIDTH.store(n.min(gex_abi::SLOTS_PER_PAGE as u32), std::sync::atomic::Ordering::Relaxed);
}

#[no_mangle]
pub extern "C" fn gex_fill_from() -> u32 {
    FILL_FROM.with(|c| c.get())
}

/// The slot the host should write, or -1 if it is not being asked.
#[no_mangle]
pub extern "C" fn gex_fill_wanted() -> i64 {
    FILL_WANTED.with(|c| c.get())
}

/// The host has answered, and whether it found anybody. A page nobody could
/// fill is remembered so the next miss takes the ordinary path, until the next
/// deferred module arrives.
#[no_mangle]
pub extern "C" fn gex_fill_done(ok: i32) {
    let page = FILL_PENDING.with(|c| c.replace(-1));
    if ok == 0 && page >= 0 {
        FILL_ASKED.lock().failed.insert(page as u64);
    }
    FILL_WANTED.with(|c| c.set(-1));
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

/// The one stdin queue, reachable both from the descriptor the kernel holds and
/// from the exports the page calls.
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
    STDIN.set_eof();
}

/// Whether the guest stopped waiting for input rather than because it finished.
/// Both give up the wasm stack the same way, so a page with a terminal must ask
/// to tell "wait for a keystroke" from "the program is over".
#[no_mangle]
pub extern "C" fn gex_paused_for_input() -> i32 {
    (paused() & 1 != 0) as i32
}

/// Whether the guest stopped because it is waiting on the network. The page
/// has nothing to supply -- it only has to let its event loop run, so the
/// data it is waiting for can arrive -- and then call [`gex_wake`].
#[no_mangle]
pub extern "C" fn gex_paused_for_net() -> i32 {
    (paused() & 2 != 0) as i32
}

/// Whether ending this wait needs the page's own event loop to turn: a guest
/// waiting on another Worker can sleep on the shared wake word, but one waiting
/// on a network callback that runs on *this* Worker's event loop cannot.
#[no_mangle]
pub extern "C" fn gex_paused_for_host() -> i32 {
    (paused() & 4 != 0) as i32
}

thread_local! {
    /// Persistence guard for the all-blocked deadlock verdict: `(wake seq at the
    /// last all-blocked observation, how many in a row with that seq unchanged)`.
    /// A real deadlock bumps the wake counter never again -- every thread is
    /// parked and nothing runs a syscall -- so the seq stays put and the count
    /// climbs. A transient all-blocked instant during startup (eight threads
    /// hitting one `pthread_once`, a wake in flight but not yet marked) keeps
    /// bumping the seq, which resets the count. Only a run of `DEADLOCK_CONFIRM`
    /// observations at one frozen seq is a real hang, so a storm is never
    /// mistaken for one and the process is not killed out from under it.
    static DEADLOCK_WATCH: std::cell::Cell<(i32, u32)> = const { std::cell::Cell::new((0, 0)) };
}

/// How many consecutive all-blocked schedules at an unchanged wake seq before the
/// deadlock is called real. Each is a park cycle apart, so this is a fraction of
/// a second of genuinely no wake anywhere -- long past any startup storm, short
/// enough that a real deadlock is still reported promptly.
const DEADLOCK_CONFIRM: u32 = 32;

/// Whether a pending signal makes a parked thread runnable. See `wait_ready`.
/// A constant so bisecting can flip it in one build; measured off, the wedge is
/// not this.
const SIGNAL_ENDS_WAIT: bool = true;

/// Whether a safepoint delivers signals. See `gex_safepoint`. A constant to be
/// flipped while bisecting the hottest path; asked once, the answer was no.
const SAFEPOINT_SIGNALS: bool = true;


/// Whether the page, rather than another Worker, is what ends this wait.
///
/// The distinction the whole of this turns on. A guest waiting for another
/// guest process is waiting for a different Worker, which bumps the wake
/// counter when it gets there. A guest waiting for a keystroke, or for bytes
/// off a WebSocket, is waiting for a callback on *this* Worker's event loop,
/// and a Worker that has not returned from a syscall is not running it.
fn wait_needs_page(r: &Run) -> bool {
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
            // (`HAVE_NET` is what the kernel's `net` is made from.)
            Ok(FileType::Socket) => !have_net(),
            // And whatever else says its waits are ended by another process:
            // a pseudoterminal, whose two ends live in the global kernel with a
            // terminal emulator in another Worker holding one of them. It was
            // called the page's before, which is true of the page's OWN
            // terminal (`STDIN`, just above) and of nothing else.
            _ => h.peer_wakes() && (unsafe { k_present() }) != 0,
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
    //
    // Asked of the handle itself: every global-kernel socket a process holds
    // is in that set (`Kernel::note_remote_unix`), so the handle can say so
    // without the kernel.
    let is_remote = |h: &Arc<dyn gex_kernel::vfs::FileHandle>| h.is_global_socket();
    match &r.waiting {
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
        // ended by another Worker, whose notify reaches a parked one.
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
        Waiting::Net(None) => have_net(),
        _ => false,
    }
}

/// Wait, here, for the syscall that just parked.
///
/// The syscall path already finishes a parked call without leaving wasm:
/// `gex_syscall` calls `schedule`, and `schedule` picks the thread up,
/// finishes the operation and says where to carry on. This only waits, on
/// this Worker's wait slot, which the turn that found nothing to run
/// registered on the queue of everything the thread waits for
/// (docs/kernel-locking.md §5.2 step 2): until one of them is notified or
/// the thread's own deadline. A wait only the page ends is not waited for
/// here: the stack goes back.
fn park_until_ready() {
    if unsafe { k_present() } == 0 || waitq::wants_page() != Some(false) {
        return;
    }
    waitq::park();
}

/// Sleep on this Worker's wait slot until a notify on something its thread
/// waits for, or the thread's own deadline (`waitq::park`). The argument is
/// what the page's run loop used to bound a park by, and is not used: a wait
/// is ended by its notify or its deadline, not by a bound.
///
/// Returns the wait instruction's answer: 0 woken, 1 the slot had already
/// moved (or there is no registered wait: schedule again), 2 timed out. -1
/// means no global kernel, so poll the event loop.
#[no_mangle]
pub extern "C" fn gex_park(_max_ms: u32) -> i32 {
    if unsafe { k_present() } == 0 {
        return -1;
    }
    if !waitq::waiting_here() {
        return 1;
    }
    waitq::park()
}

/// How long, at most `max_ms`, this Worker's waiting thread may sleep before
/// its own deadline (a timed `poll`, a `sleep`) is due; at least 1 ms. For a
/// wait the page's event loop must keep turning through (`gex.mjs`
/// `#netWait`), which cannot use [`gex_park`]. Takes no lock: the deadline is
/// the one its wait registered with.
#[no_mangle]
pub extern "C" fn gex_wait_bound_ms(max_ms: u32) -> u32 {
    let ms = waitq::park_ns(turn_now()).map_or(max_ms as u64, |ns| (ns / 1_000_000).min(max_ms as u64));
    ms.max(1) as u32
}

#[no_mangle]
pub extern "C" fn gex_wake() -> i32 {
    // Counted, because a park/wake/park cycle making no progress costs no CPU
    // and resets every other census, so it looks like a run that stopped.
    // Reported on powers of two from 4,096 (~4s at a millisecond a turn).
    static WAKES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = WAKES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if n.is_power_of_two() && n >= 4096 {
        // Without the kernel lock: the thread table and each run state lock
        // themselves (L23).
        let census = sigs::home().map(|home| {
            let mut out = String::new();
            // One process per Worker, so this Worker's threads are all there is
            // to report; other processes census their own.
            for (pid, p) in std::iter::once((home.pid() as usize, home.table())) {
                for (i, t) in p.iter() {
                    let r = t.run();
                    out.push_str(&format!(
                        " {pid}.{i}={}@{:#x}",
                        waiting_name(&r.waiting),
                        r.pc
                    ));
                    // For a futex park, say enough to tell a LOST wake from a
                    // legitimate wait: the address, the shared generation at park
                    // time vs now (equal => no wake was relayed to it), and the
                    // guest word vs the value it slept on (differ => the guest
                    // thinks it was woken, so a relay that never bumped the gen is
                    // a lost wake). See `wait_ready`'s Futex arm.
                    if let Waiting::Futex { parked, val, wake_gen, .. } = &r.waiting {
                        let a = parked.addr();
                        let gen_now = if unsafe { k_present() } != 0 {
                            unsafe { k_futex_gen(a) }
                        } else { 0 };
                        let word = GuestMem::new(guest_bytes()).read_u32(a).unwrap_or(0);
                        out.push_str(&format!(
                            "[ftx a={a:#x} gen={wake_gen}/{gen_now} word={word:#x}/slept={val:#x} woken={}]",
                            parked.woken()
                        ));
                    }
                }
            }
            out
        }).unwrap_or_default();
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
    // Handing over the whole of stdin at once means no more will be typed, and
    // a guest reading to the end must see the end.
    STDIN.set_eof();
}

/// The terminal's current `c_lflag` (bit 1 ICANON, bit 3 ECHO). For a page with
/// no global kernel, so no shared terminal, that must do its own line editing
/// against whatever this process last set.
#[no_mangle]
pub extern "C" fn gex_tty_lflag() -> u32 {
    sigs::home().map_or(0, |h| u32::from_le_bytes(h.tty().termios()[12..16].try_into().unwrap()))
}

/// Tell the guest how big the terminal is, and `SIGWINCH` the foreground group
/// (a full-screen program has no other way to learn the screen changed). Sent
/// only when the size actually changed.
#[no_mangle]
pub extern "C" fn gex_set_winsize(rows: u32, cols: u32) {
    // The terminal and the process table lock themselves.
    let Some(h) = sigs::home() else { return };
    let tty = h.tty();
    let (_, _, x, y) = tty.winsize();
    if tty.set_winsize((rows as u16, cols as u16, x, y)) {
        let fg = tty.fg();
        let g = if fg > 0 { fg } else { h.pid };
        let _ = h.procs().kill_group(h.pid, g, gex_kernel::signal::sig::SIGWINCH);
    }
}

/// Say that stdin is a terminal (what makes a shell print a prompt), and claim
/// it if nobody has: a session leader opening an unowned terminal becomes its
/// controlling process, giving `^C` a foreground group to reach before the
/// shell calls `tcsetpgrp`.
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

/// Whether to narrate every syscall, the browser's `--strace`. In the native
/// host's shape so the two traces diff directly -- the way browser-only bugs
/// get found. (`gex-kernel`'s own `eprintln!` tracing goes nowhere on wasm.)
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
pub(crate) fn warn_line(s: &str) {
    unsafe { warn(s.as_ptr() as u32, s.len() as u32) };
}

/// Write a line to the host's debug console, not the guest's stderr.
fn trace_line(s: &str) {
    unsafe { debug(s.as_ptr() as u32, s.len() as u32) };
}

/// The last few syscalls, in order, for when a run stops making them: the
/// histogram says what a run did, but when it stops the question is what it did
/// *last* (a hung `writev` means something different from an empty `poll`).
const SYSLOG_N: usize = 24;
static mut SYSLOG: [(u64, u64, u64, u64); SYSLOG_N] = [(0, 0, 0, 0); SYSLOG_N];
static mut SYSLOG_AT: usize = 0;

/// The ring, oldest first, as `name(arg0, arg1) = ret` lines. Without
/// `with_exec_lock`: read from inside the guest's own write, which already holds it.
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

/// The running program's argv, space-separated and capped, for the
/// per-process line (which program a cost belongs to).
#[no_mangle]
pub extern "C" fn gex_cmdline(out: u32, max: u32) -> u32 {
    let text = (|| {
        let Some(home) = sigs::home() else { return Default::default() };
        let mut t = home.argv().join(" ");
        if t.is_empty() {
            t = home.exe().clone();
        }
        t
    })();
    let mut n = text.len().min(max as usize);
    while n > 0 && !text.is_char_boundary(n) {
        n -= 1;
    }
    shim_slice_mut(out, n as u32).copy_from_slice(&text.as_bytes()[..n]);
    n as u32
}

/// Nanoseconds spent inside syscalls, and how many there were: kept while
/// [`sys_timed`] (a heartbeat or a trace), so both read 0 in a plain run.
static SYS_TOTAL_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SYS_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether syscalls are timed and counted (`SYS_TOTAL_NS`, `SYS_CALLS`).
fn sys_timed() -> bool {
    (unsafe { HEARTBEAT } != 0) || tracing()
}

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

/// `nr` is the call's canonical number (`gex_syscall`'s `this_nr`: an
/// i386-ABI process's own numbers mapped too), `ctx` the calling thread's
/// context: no kernel lock to find either.
fn count_syscall(nr: u64, ctx: u64) {
    if unsafe { HEARTBEAT } == 0 {
        return;
    }
    // Indexed by number. aarch64 Linux stops well short of this, and anything
    // past it is a guest asking for something that does not exist.
    static mut COUNTS: [u32; 512] = [0; 512];
    static mut TOTAL: u32 = 0;
    let (a0, a1) = {
        let gm = GuestMem::new(guest_bytes());
        (
            gm.read_u64(ctx + ctx::SYSCALL_ARGS[0] as u64).unwrap_or(0),
            gm.read_u64(ctx + ctx::SYSCALL_ARGS[1] as u64).unwrap_or(0),
        )
    };
    let total = unsafe {
        if (nr as usize) < COUNTS.len() {
            COUNTS[nr as usize] = COUNTS[nr as usize].saturating_add(1);
        }
        SYSLOG[SYSLOG_AT] = (nr, a0, a1, 0);
        SYSLOG_AT = (SYSLOG_AT + 1) % SYSLOG_N;
        TOTAL = TOTAL.wrapping_add(1);
        TOTAL
    };
    // Every `HEARTBEAT` calls: a fixed threshold cannot speak for short-lived
    // processes (a warm `ls` makes ~130 calls), so the interval is the
    // caller's.
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
    // The same list ordered by time, not count: a slow run's cost is a single
    // call taking milliseconds, invisible in a count census.
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

/// `svc #0`. Returns normally when the guest carries straight on (almost every
/// syscall); a redirect or exit traps out to JS, the way the native host
/// unwinds out of wasm, since there is no other way to abandon the stack.
/// On the way back this Worker's function table is grown if another Worker
/// allocated past it meanwhile (`cgen::ftab_resume`, X's B16), and the
/// compositor pumped if the call wrote to (or closed) its socket: a client
/// that keeps running reaches no scheduler turn, and its requests waited
/// for one (X's B2: Wine's destroyed context menu stayed on top, catching
/// the clicks meant for the window under it, while explorer ran).
#[no_mangle]
pub extern "C" fn gex_syscall(ctx: i32, pc: i64) {
    syscall_entry(ctx, pc);
    cgen::ftab_resume();
    if gex_kernel::unix::take_host_sent() {
        pump_compositor();
    }
}

fn syscall_entry(ctx: i32, pc: i64) {
    // A recording of which unit functions ran (`?covrec=1`): reported now and then.
    // A Windows process's every syscall is an NT call, made before the kernel
    // is taken: the NT layer takes it itself, briefly, and blocks without it.
    if let Some(p) = nt::process() {
        return nt::syscall(p, ctx, pc);
    }
    // One clock reading for the whole call (`turn_now`), taken when first
    // asked: most calls never ask.
    let _now = NowScope::open();
    // A stateful provider's syscall (`PROVIDER_SYSCALL` set by the page's glue,
    // cgen/libprov.rs): its wasm frames cannot be left, so it is served as a
    // wasm32 program's is -- a blocking call waits here, a thread or a child
    // is staged for the host -- and the call returns.
    let pc = {
        let provider = (pc as u64) & PROVIDER_SYSCALL != 0;
        IN_PROVIDER.with(|c| c.set(provider));
        ((pc as u64) & !PROVIDER_SYSCALL) as i64
    };
    // A Unix call from guest code under Wine's own Unix side (W3): `int 0x80`
    // with EAX = UNIX_CALL from the trampoline page. ntdll.so's handler takes
    // it, nested, and sets EAX; the guest goes on after the syscall.
    // And native Wine's NATIVE_LEFT (gexrt), served as for an NT process.
    if IN_PROVIDER.with(|c| c.get()) {
        nt::native_left(ctx, pc);
    } else {
        let hook = cgen::guest_unix_call_hook();
        if hook != 0 {
            let gm = GuestMem::new(guest_bytes());
            let eax = gm.read_u64(ctx as u32 as u64 + ctx::x(0) as u64).unwrap_or(0) as u32;
            if eax == gex_kernel::nt::gex_nr::UNIX_CALL {
                // SAFETY: a host import taking scalars; no lock is held.
                unsafe { cgen::ftab_call_vi(hook as i32, ctx) };
                return;
            }
        }
    }
    // The clock calls, which never take the kernel lock (`clock_syscall`).
    if !IN_PROVIDER.with(|c| c.get()) && clock_syscall(ctx) {
        // A wasm32 program has no safepoints of its own: its loops reach
        // the kernel through a call like these (gexcc's `__gex_poll` makes a
        // `getpid`), so a signal is delivered here, as at any call's return
        // (a translated guest's safepoints do it for it). The handler's
        // frame goes into the context, which the program's stub runs; a
        // default that ends the process ends it. gexcc's alarmloop spun for
        // ever without this.
        if WASM32.load(std::sync::atomic::Ordering::Acquire) && sigs::ready() {
            let at = ctx as u32 as u64;
            let mut gm = GuestMem::new(guest_bytes());
            if let gex_kernel::signal::Delivery::Terminate { signal, .. } = sigs::deliver(&mut gm, at) {
                let pc_at = gm.read_u64(at + ctx::PC as u64).unwrap_or(0);
                if exit_here(-(signal as i32), Some(pc_at)) != 0 {
                    trap();
                }
            }
        }
        return;
    }
    // This Worker's reader slot in the descriptor table's epoch array
    // (`gex_kernel::file::FdTable`): its wait slot, which no other live
    // Worker has. Without one, a lookup takes the table's own mutex.
    #[cfg(feature = "shared-memory")]
    if gex_kernel::sync::me() == 0 && unsafe { k_present() } != 0 {
        gex_kernel::sync::set_thread_id(waitq::slot() as u32 + 1);
    }
    fjit::watch_check(pc as u64, "syscall");
    // A guest thread in a syscall counts as stopped for a provider's
    // collection (cgen/libprov.rs); it parks on its way back if one runs.
    let stop_tracked = !IN_PROVIDER.with(|c| c.get()) && cgen::libprov::any_registered();
    let tracked_tid = if stop_tracked { my_tid() as u32 } else { 0 };
    if stop_tracked {
        cgen::libprov::syscall_enter(tracked_tid);
    }
    // Which syscall is running, so the allocator can name who was asking (wasm
    // has no backtrace). See `HEAP_BY_CALL`. Kept here too: `SYSCALL_NR` is
    // shared by every Worker of the process.
    let this_nr = {
        let gm = GuestMem::new(guest_bytes());
        // `ctx` is this thread's context (the locked section below checks it
        // against the thread's own and says so if not): no lock to find it.
        let base = ctx as u32 as u64;
        let n = gm.read_u64(base + ctx::SYSCALL_NR as u64).unwrap_or(u64::MAX);
        // An i386-ABI process (W3's Wine): its number, in `x(0)`, is i386's.
        let compat = CLOCK_ABI.load(std::sync::atomic::Ordering::Relaxed) == 1;
        let n = if compat && !gex_abi::PPC {
            let raw = gm.read_u64(base + ctx::x(0) as u64).unwrap_or(u64::MAX);
            gex_kernel::i386::canonical_number(raw)
        } else if gex_abi::X86 {
            gex_kernel::x86::canonical_number(n)
        } else if gex_abi::PPC {
            gex_kernel::ppc::canonical_number(n)
        } else {
            n
        };
        SYSCALL_NR.store(n, std::sync::atomic::Ordering::Relaxed);
        // And a select's arguments, to read the answer back from the guest's
        // own sets afterwards.
        if n == gex_kernel::syscalls::nr::CLOCK_GETTIME {
            // Where the answer lands, so the trace shows the time the guest was
            // given: apt's stall is a pselect6/clock_gettime loop recomputing a
            // deadline that never arrives if the clock does not move.
            let g = |i: u8| gm.read_u64(base + ctx::x(i) as u64).unwrap_or(0);
            CLOCK_ARG.with(|c| c.set(g(1)));
        } else {
            CLOCK_ARG.with(|c| c.set(0));
        }
        if n == gex_kernel::syscalls::nr::PSELECT6 {
            let g = |i: u8| gm.read_u64(base + ctx::x(i) as u64).unwrap_or(0);
            SELECT_ARGS[0].store(g(0), std::sync::atomic::Ordering::Relaxed);
            SELECT_ARGS[1].store(g(1), std::sync::atomic::Ordering::Relaxed);
            SELECT_ARGS[2].store(g(2), std::sync::atomic::Ordering::Relaxed);
        } else {
            SELECT_ARGS[0].store(0, std::sync::atomic::Ordering::Relaxed);
        }
        n
    };
    // posix_spawn's child running in its parent's Worker: not the calls
    // that would change the memory map it shares (vfork.rs).
    if vfork::active() {
        vfork::check(this_nr);
    }
    if tracing() {
        let gm = GuestMem::new(guest_bytes());
        let base = my_ctx();
        // The number and arguments where this architecture's ABI puts them;
        // the number canonicalised so the names below match on x86-64 too.
        let g = |i: usize| gm.read_u64(base + ctx::SYSCALL_ARGS[i] as u64).unwrap_or(0);
        let n = gm.read_u64(base + ctx::SYSCALL_NR as u64).unwrap_or(0);
        let n = if gex_abi::X86 {
            gex_kernel::x86::canonical_number(n)
        } else if gex_abi::PPC {
            gex_kernel::ppc::canonical_number(n)
        } else {
            n
        };
        let who = cur();
        // The path, for calls whose meaning is which file: `execve(0x2805ef70)`
        // says a program ran but not which. `openat`'s path is arg 1, the rest
        // arg 0.
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
        // For calls whose first argument is a descriptor, what it *is*: an fd
        // number cannot say whether two processes' pipes are the same, since
        // each numbers its own.
        let obj = match n {
            nr::READ | nr::WRITE | nr::READV | nr::WRITEV | nr::CLOSE | nr::DUP3
            | nr::LSEEK => fds_free()
                .and_then(|fds| fds.get(g(0) as i32).ok())
                .map(|f| format!(" fd0={}", f.path)),
            _ => None,
        }
        .unwrap_or_default();
        // The pid, not only the thread: every process traces to the same
        // stderr, so without it a fork's clones, execves and wait4 read as one
        // sequence.
        let whose_pid = my_pid();
        // The caller (x30) symbolized: which library function invoked this
        // syscall. Pins a busy-wait to the code doing it -- gtk3-demo's
        // native-wayland stall spins the main thread on time()/gettimeofday and
        // only the caller says which lock/condition it is polling.
        // On x86-64 the return address is the word at rsp, not a register
        // (`g` indexes the six argument slots; 30 would be out of bounds).
        let ret = if gex_abi::X86 {
            gm.read_u64(gm.read_u64(base + ctx::SP as u64).unwrap_or(0))
                .unwrap_or(0)
        } else if gex_abi::PPC {
            gm.read_u32(base + ctx::PPC_LR as u64).unwrap_or(0) as u64
        } else {
            gm.read_u64(base + ctx::x(30) as u64).unwrap_or(0)
        };
        let caller = object_off_at(ret);
        // An unknown call shows its raw number: "unknown" alone cannot be looked up.
        let raw_nr = gm.read_u64(base + ctx::SYSCALL_NR as u64).unwrap_or(u64::MAX);
        let name = match gex_kernel::syscalls::syscall_name(n) {
            "unknown" => format!("unknown#{raw_nr}"),
            s => s.to_string(),
        };
        warn_line(&format!(
            "[gex:{whose_pid}] t{who} pc=0x{pc:x} {}({:#x}, {:#x}, {:#x}){}{obj} <-{}  ...\n",
            name,
            g(0),
            g(1),
            g(2),
            named.map_or(String::new(), |p| format!(" \"{p}\"")),
            if caller.is_empty() { format!("{ret:#x}") } else { caller },
        ));
        // For a gather write, what the kernel sees at each iovec: a mismatch
        // with the guest means they disagree about the memory they share.
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
    count_syscall(this_nr, ctx as u32 as u64);
    watch_mem("syscall");
    // Recorded before the kernel is taken, and readable without taking it: a
    // crash inside a syscall leaves the kernel borrowed, so this one number is
    // what survives to narrow a crash to a single handler.
    LAST_SYSCALL.store(
        GuestMem::new(guest_bytes())
            .read_u64(ctx as u32 as u64 + ctx::SYSCALL_NR as u64)
            .unwrap_or(!0) as u32,
        std::sync::atomic::Ordering::Relaxed,
    );
    IN_SYSCALL.store(true, std::sync::atomic::Ordering::Relaxed);
    let _in_syscall = InSyscall;
    ctx_addr_set(ctx as u32 as u64);
    // The process tree, always. Stood down under `GEX_TRACE`, which narrates
    // these along with everything else.
    if !tracing() {
        note_process_syscall(ctx as u32 as u64, pc);
    }
    // The host's integrity check, the process's pid and the pin report are
    // done inside the locked section below: each took the host lock on its
    // own, and with a W3 game's ~25 Wine Workers contending for it every take
    // costs (Undertale's game thread spent ~14 ms a frame on this path).
    // Every syscall timed, with a heartbeat or a trace on (`sys_timed`): a
    // blocking call parks the Worker here, so this covers waiting as well as
    // work. Off by default: two clock reads (JS calls) and two read-modify-
    // writes on counters every Worker shares cost more than a lock-free call.
    let timed = sys_timed();
    let began = if timed { now_ns() } else { 0 };
    if timed {
        SYS_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    // A GL submit is taken under the lock and done after it (gl.rs).
    gl::DEFER.with(|d| d.set(true));
    let mut mapped = Vec::new();
    // Another thread's `execve` took the process from this one: not even a
    // lock-free call runs, since the memory it would touch is about to be the
    // new program's (`exec_waits`).
    let gone_now = thread_gone();
    if gone_now {
        stop_gone();
    }
    let stop = match if gone_now { Some(3) } else { free_syscall(ctx, pc, &mut mapped) } {
    Some(stop) => stop,
    None => with_exec_lock(|h| {
        let stop = (|h: &mut Host| -> i32 {
        // Another thread's `execve` took the process from this one: the
        // thread table is the new program's, and this thread is not in it.
        if thread_gone() {
            stop_gone();
            return 3;
        }
        // Whether the kernel this call is about to run against is still
        // intact; whose process this is, for the calls that name it (set
        // here so it cannot be missed); and whether a pinned library landed
        // where the closure built it (otherwise invisible in a shell demo,
        // where process 1 never exits to call `gex_pin_report`).
        check_host_magic_in(h, "a syscall");
        set_my_pid(h.kernel.pid);
        report_new_pins_in(h);
        let mut gm = GuestMem::new(guest_bytes());
        let kstate = h.threads[cur()].kstate();
        if ctx as u32 as u64 != kstate.ctx_addr {
            warn_line(&format!(
                "[gex] syscall context mismatch at {:#x}: translated ctx={:#x}, worker thread {} ctx={:#x}",
                pc as u64,
                ctx as u32,
                kstate.tid,
                kstate.ctx_addr,
            ));
        }
        h.kernel.enter_thread(&kstate);
        // Whether this is a wasm32 program: `WASM32` says (cgen/wasmprog.rs
        // sets it at the program's install, without the lock).
        h.kernel.wasm32 = WASM32.load(std::sync::atomic::Ordering::Acquire);
        // Rebuilding the proc snapshot costs more than most syscalls, and only
        // a handful can observe it, so the rest skip it.
        let nr = gm
            .read_u64(kstate.ctx_addr + ctx::SYSCALL_NR as u64)
            .unwrap_or(!0);
        // The raw number: `SNAP_SYSCALL` is compared raw; the proc-snapshot
        // question wants the canonical one.
        // An i386-ABI process (W3's Wine) numbers its calls the i386 way, with
        // the number in `x(0)` in every build.
        let canonical = if h.kernel.compat && !gex_abi::PPC {
            let raw = gm.read_u64(kstate.ctx_addr + ctx::x(0) as u64).unwrap_or(!0);
            gex_kernel::i386::canonical_number(raw)
        } else if gex_abi::X86 {
            gex_kernel::x86::canonical_number(nr)
        } else if gex_abi::PPC {
            gex_kernel::ppc::canonical_number(nr)
        } else {
            nr
        };
        // The dynamic linker jumped to gex's stub entry: keep what it built and
        // carry on at the program's real entry, registers as the linker left
        // them (nothing writes a return value for this one).
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
                    unsafe { k_snap_store(key, blob.as_ptr() as gex_kernel::PAddr, n) };
                }
                link_state(3);
                // Under tracing only: the per-process line already says
                // `link kept`.
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
            h.threads[cur()].run().pc = entry;
            set_resume(entry);
            set_resume_ctx(kstate.ctx_addr);
            return 1;
        }
        // A syscall from a process that has already exited: `exit_process`
        // tears the descriptor table down, so later guest calls fail like bugs
        // elsewhere. This says whether the guest truly ran on.
        // A syscall from a thread whose process has exited (another Worker
        // ran `exit_group` while this one was parked, in `pause()` say): on
        // Linux the thread would already be gone, so it goes now, without
        // the call. `exit_process` has torn the descriptor table down; the
        // call would only fail strangely.
        if exited() {
            if tracing() {
                trace_line(&format!(
                    "[gex] pid {} thread {} ends at its {} after the process exited\n",
                    h.kernel.pid,
                    kstate.tid,
                    gex_kernel::syscalls::syscall_name(nr),
                ));
            }
            set_resume_none();
            h.threads[cur()].run().waiting = Waiting::Exited;
            return 3;
        }
        let a0 = gm.read_u64(kstate.ctx_addr + ctx::x(0) as u64).unwrap_or(0);
        let a1 = gm.read_u64(kstate.ctx_addr + ctx::x(1) as u64).unwrap_or(0);
        let a2 = gm.read_u64(kstate.ctx_addr + ctx::x(2) as u64).unwrap_or(0);
        let slot = sys_note(nr, a0, a1);
        let outcome = h.kernel.syscall(&mut gm, pc as u64);
        // The page finishes its own waits (`finish_wait`, which asks the op
        // for its restart class): the kernel's note for `finish_blocking`,
        // left, would follow the thread through every switch.
        h.kernel.block_restart = gex_kernel::signal::Restart::Never;
        h.kernel.block_pc = 0;
        // The clocks never come here (`clock_syscall`); one that does is a
        // regression, said once, as an error every page test fails on.
        if is_clock_nr(h.kernel.syscall_nr) && !IN_PROVIDER.with(|c| c.get()) && !exited() {
            static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
                warn_line(&format!("[gex] pid {}: clock call {} took the kernel lock\n", h.kernel.pid, h.kernel.syscall_nr));
            }
        }
        let ret = gm.read_u64(kstate.ctx_addr + ctx::x(0) as u64).unwrap_or(0);
        sys_ret(slot, ret);
        // A signal sent: the process's waits look again, and an abort is
        // named (`kill_sent`).
        {
            use gex_kernel::syscalls::nr as knr;
            // The kernel's own (canonical) number: `nr` is the raw one, which
            // for an x86 or i386 process is in that ABI's numbering.
            let canon = h.kernel.syscall_nr;
            if matches!(canon, knr::KILL | knr::TKILL | knr::TGKILL) {
                let sig = if canon == knr::TGKILL { a2 } else { a1 };
                kill_sent(&gm, kstate.ctx_addr, h.kernel.pid, sig, &Px::of(h));
            }
        }
        // Relay every futex this syscall woke to the shared per-address counter,
        // so a waiter parked in another Worker sees the wake exactly rather than
        // by re-reading a word that may have changed back. Covers a FUTEX_WAKE, a
        // thread exit clearing its tid, a robust-list wake -- all go through the
        // kernel's `FutexTable::wake`, which recorded them.
        if unsafe { k_present() } != 0 {
            for addr in h.kernel.futexes.take_woken() {
                unsafe { k_futex_wake(addr) };
            }
        }
        if is_mem_call(nr) {
            mem_note(nr, a0, a1, ret);
        }
        // What the guest was trying to say when it could not: glibc writes a
        // fatal diagnostic to fd 2 then aborts, so a stderr write that does not
        // go through turns a named failure into "killed by signal 6".
        // Only when nothing went through (an error, or 0 of a non-empty
        // write). A short positive count is not a loss: a full pipe on a
        // blocking descriptor takes what fits and the caller writes the rest
        // (`Pipe::write`, `BlockOp::WriteStream`). Warning on that once sent
        // 4.5 MB of xwd's image, written to stdout through a pipe, to the
        // console as "could not reach stderr".
        if (a0 == 1 || a0 == 2) && (nr == 64 || nr == 66) {
            stderr_capture(&gm, a0, nr == 66, a1, a2, ret);
        }
        match outcome {
            SyscallOutcome::Continue => 0,
            SyscallOutcome::Resume(target) => {
                // A syscall boundary that resumes somewhere else is a signal
                // handler frame: the process took a signal here. Capped, and
                // with where the signal came from (`k_last_kill`), because a
                // signal that should never have been sent looks like a program
                // misbehaving -- a terminal tearing down a live session.
                {
                    use std::sync::atomic::{AtomicU32, Ordering};
                    static SAID: AtomicU32 = AtomicU32::new(0);
                    if SAID.fetch_add(1, Ordering::Relaxed) < 32 {
                        let last = if unsafe { k_present() } != 0 {
                            let v = unsafe { k_last_kill() };
                            format!(
                                " (signal #{}: {} from pid {} to {})",
                                unsafe { k_last_kill_seq() },
                                v & 0xff,
                                (v >> 40) as i32,
                                ((v >> 8) & 0xffff_ffff) as i32
                            )
                        } else {
                            String::new()
                        };
                        // Informational (a handler running is normal), so the
                        // debug console: a console error fails a page test.
                        trace_line(&format!(
                            "[sig] pid {} runs a handler at {target:#x}{last}\n",
                            h.kernel.pid
                        ));
                    }
                }
                set_resume(target);
                1
            }
            SyscallOutcome::Exit(code) => exit_process(code),
            // A signal with no handler kills the *process*, not the run.
            // Setting the run's exit here is what ended `apt update`: apt
            // stops its http method with `kill(pid, SIGINT)` -- ordinary
            // shutdown, and the method installs no handler -- and the whole
            // guest went down with it, after the download had succeeded.
            // Negative for "died of a signal"; see `exit_process`.
            SyscallOutcome::Killed { signal, .. } => {
                let at = gm.read_u64(kstate.ctx_addr + ctx::PC as u64).unwrap_or(0);
                killed(signal as i32, at)
            }
            SyscallOutcome::SpawnThread { ctx_addr, tid } => thread_made(tid, ctx_addr, pc as u64),
            SyscallOutcome::ThreadExit => {
                h.threads[cur()].run().waiting = Waiting::Exited;
                end_if_all_exited();
                // Nowhere to resume this thread. A resume left from before
                // (a call-out's return, `cgen::returned`, which a call-out
                // that was not the one returned to leaves set) would be
                // entered by the run loop: a Wine thread's pthread_exit under
                // its start routine's call-out went back to the call-out's
                // return pad and the process died of its breakpoint (133).
                set_resume_none();
                set_resume_ctx_none();
                3
            }
            SyscallOutcome::Blocking(op) => {
                let waiting = match &op {
                    gex_kernel::BlockOp::FutexWait {
                        table,
                        uaddr,
                        deadline,
                        parked,
                        val,
                    } => Waiting::Futex {
                        val: *val,
                        // The shared generation of this address, sampled now --
                        // BEFORE parking but AFTER the kernel checked the word,
                        // which is fine: a wake landing between the check and here
                        // bumps the counter, so the first re-check sees the change.
                        // (A wake between the word-check and the *kernel's*
                        // snapshot is caught by `parked`, taken before the check.)
                        wake_gen: if unsafe { k_present() } != 0 {
                            unsafe { k_futex_gen(*uaddr) }
                        } else {
                            0
                        },
                        // The place the kernel snapshotted BEFORE it checked the
                        // word -- using it (not a fresh `park` here, after the
                        // check) is what stops a wake between the check and the
                        // park from being lost. Falls back to a fresh park only
                        // if the kernel gave none, which it always does for a
                        // real FUTEX_WAIT.
                        parked: parked.clone().unwrap_or_else(|| table.park(*uaddr)),
                        deadline: deadline.map(|d| turn_now() + d.as_nanos() as u64),
                    },
                    gex_kernel::BlockOp::Sleep(d) => Waiting::Sleep(turn_now() + d.as_nanos() as u64),
                    // A priority-inheritance lock: tried when the thread runs
                    // again (`finish_parked`), and the syscall made again until
                    // it is taken (`PI_RETRY`); a short sleep between tries.
                    gex_kernel::BlockOp::FutexLockPi { uaddr, deadline, .. } => Waiting::Pi {
                        addr: *uaddr,
                        gen: if unsafe { k_present() } != 0 { unsafe { k_futex_gen(*uaddr) } } else { 0 },
                        deadline: deadline.map(|d| turn_now() + d.as_nanos() as u64),
                    },
                    gex_kernel::BlockOp::ReadStream { handle, .. } => {
                        // What the thing being waited on says about itself,
                        // at the moment of parking. A reader on a pipe whose
                        // writers are already gone should have had
                        // end-of-file instead, and that is a different bug
                        // from a reader whose writer is simply busy -- the
                        // ends tell them apart and nothing else does. gpgv
                        // parks here on a pipe whose only writer has exited.
                        // See task 140.
                        if tracing() {
                            trace_line(&format!(
                                "[gex:{}] park read {}\n",
                                h.kernel.pid,
                                handle.debug_state()
                            ));
                        }
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
                        Waiting::Net(deadline.map(|d| turn_now() + d.as_nanos() as u64))
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
                    // `sigsuspend` waits for a signal, and only a signal: one
                    // from another process notifies this process's wait queue
                    // (the process table does), one from a thread of its own
                    // notifies it too (`waitq::wake_process`), and a timer's is
                    // the wait's deadline (`waitq::TIMER_AT`). Deliver at once
                    // if one is already deliverable.
                    gex_kernel::BlockOp::SigSuspend { .. } => {
                        if h.kernel.signals.would_deliver() || unsafe { k_present() } == 0 {
                            Waiting::Runnable
                        } else {
                            Waiting::Signal
                        }
                    }
                    // `rt_sigtimedwait`, woken as `sigsuspend` is, or by its
                    // deadline; made again when it ends with one of its set
                    // pending (`finish_parked`), which takes it.
                    gex_kernel::BlockOp::SigWait { timeout, .. } => {
                        Waiting::SigWait(timeout.map(|d| turn_now().saturating_add(d.as_nanos() as u64)))
                    }
                };
                // A new wait: whatever this Worker's thread was registered
                // for is over.
                waitq::leave();
                // What else the call owes, out of the kernel and into the
                // thread's own run state, so that finishing it needs no
                // kernel (`finish_wait`). A call the kernel would redo once
                // its wait is over is made again instead.
                let k = &mut h.kernel;
                // The kernel's own finish for a wait it parked; here the call
                // is made again instead (`finish_parked`).
                k.retry_sigwait = None;
                let owed = Owed {
                    restore_sigmask: k.restore_sigmask.take(),
                    write_blocked: std::mem::take(&mut k.write_blocked),
                    reissue: k.retry_accept.take().is_some()
                        | k.retry_recvmsg.take().is_some()
                        | k.retry_transfer.take().is_some(),
                };
                {
                    let mut r = h.threads[cur()].run();
                    r.pc = pc as u64 + gex_abi::SYSCALL_INST_BYTES;
                    r.waiting = waiting;
                    r.blocked_on = Some(op);
                    r.owed = owed;
                }
                // The scheduler's first turn is `schedule`'s, after this
                // take (`pick_mine`): the wait is the thread's own now.
                3
            }
            SyscallOutcome::SpawnProcess {
                pid,
                child_stack,
                tls,
                vfork,
            } => {
                // posix_spawn's CLONE_VM | CLONE_VFORK: its child can run in
                // this Worker (vfork.rs).
                vfork::note_clone(vfork);
                if frames_stay() {
                    // A wasm32 process clones at its thread entry (the
                    // context's PC, as the musl fork's __clone leaves it);
                    // the call returns, and `#wasmprogSyscall` hands the
                    // child to the host.
                    let start = gm.read_u64(kstate.ctx_addr + ctx::PC as u64).unwrap_or(0);
                    do_fork(h, pid, pc as u64, child_stack, tls, Some(start));
                    WASM32_STAGED.with(|c| c.set(true));
                    return 0;
                }
                do_fork(h, pid, pc as u64, child_stack, tls, None);
                // Give the stack back though the parent carries on: the child
                // is a copy of this memory that must be taken now, when the
                // stack comes back. Letting the parent run on first copied it
                // several syscalls later, with a context already saved over.
                set_resume(pc as u64 + gex_abi::SYSCALL_INST_BYTES);
                set_resume_ctx(h.threads[cur()].ctx_addr());
                1
            }
            SyscallOutcome::Exec { path, argv, envp } if vfork::active() => {
                // posix_spawn's child, in its parent's memory: the new
                // program gets a Worker of its own (vfork.rs).
                vfork::stage_exec(h, &path, &argv, &envp);
                1
            }
            // Not for a wasm32 program, whose call cannot be made again from
            // its instruction: its threads have no translated code to run
            // over the new image, and stop at their next call (`do_exec`).
            SyscallOutcome::Exec { path, argv, envp } if !frames_stay() && exec_waits(h) => {
                // The other threads are told to go and this one waits a
                // moment and makes the call again, until they have.
                let _ = (path, argv, envp);
                let mut r = h.threads[cur()].run();
                r.pc = pc as u64 + gex_abi::SYSCALL_INST_BYTES;
                r.waiting = Waiting::Sleep(turn_now() + EXEC_WAIT_STEP_NS);
                r.blocked_on = Some(gex_kernel::BlockOp::Sleep(std::time::Duration::from_nanos(EXEC_WAIT_STEP_NS)));
                r.owed = Owed { reissue: true, ..Owed::default() };
                drop(r);
                waitq::leave();
                3
            }
            SyscallOutcome::Exec { path, argv, envp } => {
                h.exec_waited_since = None;
                let entry = do_exec(h, &path, &argv, &envp);
                if entry == 0 {
                    // The kernel checked the path before saying `Exec`, so a
                    // failure here is the loader's: not an ELF, or one this
                    // cannot place. `ENOEXEC` is what the guest expects.
                    let mut gm = GuestMem::new(guest_bytes());
                    let ctx_addr = h.threads[cur()].ctx_addr();
                    let _ = gex_kernel::syscalls::set_result(
                        &mut gm,
                        ctx_addr,
                        Errno::ENOEXEC.as_return(),
                    );
                    0
                } else {
                    set_resume(entry);
                    set_resume_ctx(h.threads[cur()].ctx_addr());
                    1
                }
            }
            _ => {
                set_exit(128 + 31);
                2
            }
        }
        })(h);
        // Objects the call unmapped are retired, and those it mapped taken
        // for recompiling, under this same lock: each was a lock take of its
        // own after every syscall.
        retire_unmapped_code_in(h);
        mapped = std::mem::take(&mut h.kernel.mem.lock().log.new_code);
        waitq::publish_timer();
        stop
    }),
    };
    gl::DEFER.with(|d| d.set(false));
    gl::submit_pending(ctx as u32 as u64);
    if timed {
        let took = now_ns().saturating_sub(began);
        SYS_TOTAL_NS.fetch_add(took, std::sync::atomic::Ordering::Relaxed);
        if unsafe { HEARTBEAT } != 0 {
            // Canonical, as the census counts it (an i386-ABI process's
            // own numbers name other calls in the canonical table).
            let nr = this_nr as usize;
            if nr < 512 {
                unsafe { SYS_NS[nr] = SYS_NS[nr].saturating_add(took) };
            }
        }
    }
    // Whatever that call changed notified the queues of what it changed
    // (docs/kernel-locking.md §5.2 step 2): nobody is woken here.
    let _ = this_nr;
    // Objects the guest just mapped (`mapped`, taken in the locked section),
    // recompiled in bulk before it can branch in: queued below, after the
    // lock. Nothing is instantiated and the guest is not interrupted, because
    // a page's directory entry is not published until its module installs,
    // so every branch still finds the miss stub; JS picks the queue up at the
    // next miss.
    //
    // Libraries some of whose exports a provider serves (glibc's mem*,
    // docs/elf-providers.md): JS binds them before the guest goes on, as the
    // native host does after the same syscall.
    let stop = if crate::cgen::elfprov::maps(&mapped) && stop == 0 {
        set_resume(pc as u64 + gex_abi::SYSCALL_INST_BYTES);
        set_resume_ctx(my_ctx());
        1
    } else {
        stop
    };
    recompile_objects(mapped);
    // 3 means the running thread gave up the wasm stack: pick another, waiting
    // first if it is waiting on another Worker (see `park_until_ready`).
    let stop = if stop == 3 && frames_stay() {
        wasm32_wait(pc as u64)
    } else if stop == 3 {
        // Ask first, wait second: most parked calls are already ready by now
        // (the pipe has room, the child has exited), and a readiness scan first
        // -- a call into the global kernel per shared handle -- cost more than
        // the unwinding it avoided.
        let mut picked = schedule();
        if picked == 2 {
            park_until_ready();
            picked = schedule();
        }
        // When the thread that carried on is the one that made this call, and
        // resumes at the instruction after its `svc` (the bottom of this
        // block), returning is the same as re-entering there minus the trap and
        // a round of the event loop. Only then: a signal or a different thread
        // is a real re-entry that must go out through JS.
        let here = my_ctx();
        if picked == 1
            && resume() == Some(pc as u64 + gex_abi::SYSCALL_INST_BYTES)
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
    // The answer, into the slot the entry took: the ring is written before the
    // call (a call that never returns is the one worth seeing); this fills in
    // the answer for the ones that do.
    if unsafe { HEARTBEAT } != 0 {
        let x0 = GuestMem::new(guest_bytes()).read_u64(my_ctx() + ctx::x(0) as u64).unwrap_or(0);
        unsafe {
            let last = (SYSLOG_AT + SYSLOG_N - 1) % SYSLOG_N;
            SYSLOG[last].3 = x0;
        }
    }
    if tracing() {
        let gm = GuestMem::new(guest_bytes());
        let (i, ctx_addr) = (cur(), my_ctx());
        let x0 = gm.read_u64(ctx_addr + ctx::x(0) as u64).unwrap_or(0);
        // For a `select`, split the count: the total hides whether the ready
        // descriptors are readable or writable, which decides where a
        // never-reading spin's fault is.
        let split = {
            let nfds = SELECT_ARGS[0].load(std::sync::atomic::Ordering::Relaxed);
            if nfds == 0 || nfds > 1024 {
                String::new()
            } else {
                let words = (nfds as usize).div_ceil(64);
                // The fd numbers, not a count: which descriptors are ready is
                // the question when a guest is handed some and reads none.
                let list = |set: u64| {
                    let mut v: Vec<u32> = Vec::new();
                    if set == 0 {
                        return v;
                    }
                    for w in 0..words {
                        let word = gm.read_u64(set + (w as u64) * 8).unwrap_or(0);
                        for b in 0..64u32 {
                            if word & (1u64 << b) != 0 {
                                let fd = (w as u32) * 64 + b;
                                if fd < nfds as u32 {
                                    v.push(fd);
                                }
                            }
                        }
                    }
                    v
                };
                let rv = list(SELECT_ARGS[1].load(std::sync::atomic::Ordering::Relaxed));
                let wv = list(SELECT_ARGS[2].load(std::sync::atomic::Ordering::Relaxed));
                let join = |v: &[u32]| {
                    v.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(",")
                };
                // What each ready-for-read descriptor is: told readable and not
                // reading is either a lying handle or a guest that never gets
                // there, and the open file says which.
                let what = {
                    let mut s = String::new();
                    for fd in &rv {
                        match fds_free().ok_or(gex_kernel::Errno::EBADF).and_then(|t| t.get(*fd as i32)) {
                            Ok(f) => s.push_str(&format!(
                                " {fd}:{}{} av={:?} pr={} {}",
                                if f.readable { "r" } else { "-" },
                                if f.writable { "w" } else { "-" },
                                f.handle.available(),
                                f.handle.poll_readable(),
                                f.path,
                            )),
                            Err(_) => s.push_str(&format!(" {fd}:bad")),
                        }
                    }
                    s
                };
                format!(
                    " nfds={nfds} r=[{}] w=[{}]{what}",
                    join(&rv),
                    join(&wv)
                )
            }
        };
        let clk = {
            let at = CLOCK_ARG.with(|c| c.get());
            if at == 0 {
                String::new()
            } else {
                let secs = gm.read_u64(at).unwrap_or(0);
                let nanos = gm.read_u64(at + 8).unwrap_or(0);
                format!(" clock={secs}.{nanos:09}")
            }
        };
        trace_line(&format!("[gex]   t{i} = {}{split}{clk}\n", x0 as i64));
    }
    if stop == 0 && stop_tracked {
        cgen::libprov::guest_again(tracked_tid);
    }
    // A call to make again (`finish_wait`: a priority-inheritance lock still
    // held, a stream another thread got to first, an `accept` or `recvmsg`
    // whose wait is over) from a provider or a wasm32 program: it has no
    // instruction to go back to, so the call is made again here, outside
    // the host lock.
    let provider = IN_PROVIDER.with(|c| c.get());
    if stop != 0
        && (provider || frames_stay())
        && resume() == Some(pc as u64)
        && resume_ctx().unwrap_or(my_ctx()) == my_ctx()
    {
        set_resume_none();
        set_resume_ctx_none();
        let pc = if provider { pc as u64 | PROVIDER_SYSCALL } else { pc as u64 };
        return gex_syscall(ctx, pc as i64);
    }
    if stop != 0 {
        trap();
    }
}


/// Everything below the guest's image: the register contexts, the exclusive
/// monitor scratch, and the dispatch directory. None of it is in the memory
/// map, and all of it is per-process.
const LOW_END: u64 = gex_abi::DISPATCH_DIR + gex_abi::DISPATCH_MAX_PAGES * 4;

// The snapshot switcher was here (`Region`, `snapshot_memory`,
// `restore_memory`, `clear_mapped`, `switch_process`, `install_process`). It
// existed because every process shared one address space -- forced because
// shared state (process table, pipes, sockets) was made of pointers, and a
// pointer means a memory -- so two processes' libraries collided and one had to
// be written out and the other read in (205 MiB over a single `apt update`).
//
// That state now lives in the global kernel module every Worker instantiates
// against, where a pipe is an id rather than an `Arc`, so there is nothing to
// save or restore: `fork` asks the host for a Worker with a copy of the
// parent's regions, `wait4`/`kill`/`exit` route through the kernel's table, and
// each process is a Worker the engine schedules.

/// The clock calls' process constants (`clock_syscall`): the ABI (0 before
/// the process is loaded, 1 i386, 2 native) and when the process started
/// (`Kernel::started`, ns), which the processor-time clocks count from. Set
/// at load, exec and a child's or thread's adoption (`clock_constants`),
/// before the process's first syscall, and not changed by any syscall after.
static CLOCK_ABI: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
static CLOCK_STARTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Clock calls answered by `clock_syscall`.
static CLOCK_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The clock-family numbers (canonical), which `clock_syscall` answers.
fn is_clock_nr(n: u64) -> bool {
    use gex_kernel::syscalls::nr;
    matches!(n, nr::CLOCK_GETTIME | nr::CLOCK_GETRES | nr::GETTIMEOFDAY | nr::GETRUSAGE | nr::SCHED_YIELD)
}

/// `Kernel::compat` of this process, without the kernel: published with it
/// at every change (`clock_constants`).
fn compat_abi() -> bool {
    CLOCK_ABI.load(std::sync::atomic::Ordering::Acquire) == 1
}

/// When this process started (`CLOCK_STARTED`), for a sleep until a time on
/// a processor-time clock without the kernel.
pub(crate) fn clock_started() -> u128 {
    CLOCK_STARTED.load(std::sync::atomic::Ordering::Acquire) as u128
}

fn clock_constants(compat: bool, started: u128) {
    use std::sync::atomic::Ordering::Release;
    CLOCK_STARTED.store(started as u64, Release);
    CLOCK_ABI.store(if compat { 1 } else { 2 }, Release);
}


/// The clocks, with no kernel lock (docs/kernel-locking.md §5.2 step 1):
/// `clock_gettime`, `clock_getres`, `gettimeofday`, `getrusage` and
/// `sched_yield` are answered here, always, from the clock and the process's
/// constants (`gex_kernel::syscalls::clock_call`, `EFAULT` included). They
/// read nothing the kernel guards: the time base is fixed at boot
/// (`k_boot_secs`), the ABI and start at load. Undertale's game process makes
/// about 7,000 of them a second (Wine's `NtDelayExecution` is getrusage,
/// sched_yield, getrusage, and its timers read the clock).
///
/// Not done here, as the kernel would at a syscall boundary: delivering a
/// pending signal, which the next safepoint does (`gex_safepoint`'s own
/// check); and a provider collection's stop (cgen/libprov.rs), which parks
/// the thread on its way back to guest code, as after any syscall.
///
/// The getpid family is answered here too (`ident_syscall`, step 6a).
fn clock_syscall(ctx: i32) -> bool {
    use std::sync::atomic::Ordering::{Acquire, Relaxed};
    let abi = CLOCK_ABI.load(Acquire);
    // Before the process is loaded there is no guest to make a call; once it
    // has exited, the full path ends the thread.
    if abi == 0 || exited() {
        return false;
    }
    let mut gm = GuestMem::new(guest_bytes());
    let started = CLOCK_STARTED.load(Acquire) as u128;
    let at = ctx as u32 as u64;
    if !gex_kernel::syscalls::clock_call(&mut gm, at, abi == 1, browser_clock, started)
        && !ident_syscall(&mut gm, at, abi == 1)
    {
        return false;
    }
    SYS_CALLS.fetch_add(1, Relaxed);
    if tracing() {
        let nr = gm.read_u64(at + if abi == 1 { ctx::x(0) } else { ctx::SYSCALL_NR } as u64).unwrap_or(0);
        let ret = gm.read_u64(at + ctx::x(0) as u64).unwrap_or(0);
        trace_line(&format!("[sys] {nr} (no kernel lock) = {ret:#x}\n"));
    }
    // A provider's collection running: park here, as the full path does on
    // its way back to guest code.
    if cgen::libprov::stop_active() {
        let tid = my_tid();
        if tid != 0 {
            cgen::libprov::park_if_stopping(tid as u32, false);
        }
    }
    // Narration, not a warning: a page test fails on any console error.
    if CLOCK_CALLS.fetch_add(1, Relaxed) == 0 {
        trace_line(&format!(
            "[gex] pid {}: clock calls served without the kernel lock\n",
            MY_PID.load(Relaxed)
        ));
    }
    true
}

/// After a `kill`, `tkill` or `tgkill` of signal `sig` from the thread whose
/// context is at `ctx_addr`, with the kernel lock or without it (L22d).
///
/// A signal for a thread of this process is raised on that thread's own
/// pending word, which its safepoints read (`sigs`); the thread may be
/// parked in another Worker, so the process's waits look again.
///
/// A process raising SIGABRT (`abort()`, a fortify/stack-check trap, a
/// caught fatal signal) prints nothing of its own if the message path is
/// gone, so name who did it: the stack from the raiser holds `abort`'s
/// saved return address (its caller), and x30 the nearest frame -- enough
/// to say which library and function a silent abort came from.
fn kill_sent(gm: &GuestMem<'_>, ctx_addr: u64, pid: i32, sig: u64, p: &Px<'_>) {
    waitq::wake_process();
    if sig != 6 {
        return;
    }
    let sp = gm.read_u64(ctx_addr + ctx::SP as u64).unwrap_or(0);
    let x30 = gm.read_u64(ctx_addr + ctx::x(30) as u64).unwrap_or(0);
    let mut bt = String::new();
    // The frame-pointer chain: at each aarch64 frame `[x29]` is the
    // caller's x29 and `[x29+8]` its return address -- a clean call
    // stack, unlike raw stack words which are half stale data.
    let mut fp = gm.read_u64(ctx_addr + ctx::x(29) as u64).unwrap_or(0);
    for _ in 0..40 {
        if fp == 0 || fp & 7 != 0 {
            break;
        }
        let ret = gm.read_u64(fp + 8).unwrap_or(0);
        let o = object_off_at(ret);
        if !o.is_empty() {
            bt.push_str(&format!("\n    #fp {ret:#x} {o}"));
        }
        let next = gm.read_u64(fp).unwrap_or(0);
        if next <= fp {
            break;
        }
        fp = next;
    }
    for i in 0..80u64 {
        let w = gm.read_u64(sp + i * 8).unwrap_or(0);
        let o = object_off_at(w);
        if o.contains(".so") || o.starts_with('/') {
            bt.push_str(&format!("\n    [sp+{:#x}] {w:#x} {o}", i * 8));
        }
    }
    warn_line(&format!(
        "gex: SIGABRT raised by pid {} x30={x30:#x} {}, stack refs:{bt}\n",
        pid,
        object_name_at(p, x30),
    ));
}

/// The getpid family for this Worker's thread (`ident_call`): its pid as
/// its process's load or fork set it (`MY_PID`), its tid as its thread's
/// start bound it (`bind_mine`), and the process's ids as they are now
/// (`Creds`). No parent: asking for it is a call into the process table,
/// and a reparent must be seen; `getppid` is a quiet call. A vfork child
/// runs on its parent's thread, whose tid is not its own: its calls take
/// the full path.
fn ident_syscall(gm: &mut GuestMem<'_>, at: u64, compat: bool) -> bool {
    if vfork::pid_here().is_some() {
        return false;
    }
    let Some(home) = sigs::home() else { return false };
    let ids = gex_kernel::syscalls::Idents {
        pid: MY_PID.load(std::sync::atomic::Ordering::Relaxed) as u32,
        ppid: 0,
        tid: my_tid() as u32,
    };
    ids.pid != 0 && gex_kernel::syscalls::ident_call(gm, at, compat, &ids, home.creds())
}

/// This process's descriptor table, which needs no kernel lock
/// (docs/kernel-locking.md §3.4): `h.kernel.fds` of the installed host
/// (`set_host`). The table stays where it is for the life of
/// the process -- it is closed (`close_all`), never replaced -- so the
/// pointer holds until the next `set_host`, which runs before any thread of
/// the new process makes a call.
static FDS: std::sync::atomic::AtomicPtr<gex_kernel::file::FdTable> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
/// This process's descriptor table, without the kernel (`FDS`): `None`
/// before a process is installed.
pub(crate) fn fds_free() -> Option<&'static gex_kernel::file::FdTable> {
    // A vfork child's own table, while it runs in this Worker (vfork.rs).
    if let Some(t) = vfork::fds_here() {
        return Some(t);
    }
    let p = FDS.load(std::sync::atomic::Ordering::Acquire);
    // SAFETY: see `FDS`. Every use of the table goes through `&self`, its
    // state behind its own atomics and mutex.
    (!p.is_null()).then(|| unsafe { &*p })
}

/// The mark the page's glue puts on a stateful provider's syscalls (as gexrt's
/// `NATIVE_SYSCALL` on native Wine's, `nt.rs`).
const PROVIDER_SYSCALL: u64 = 1 << 61;

thread_local! {
    /// The syscall being served is a stateful provider's (see `gex_syscall`).
    static IN_PROVIDER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the syscall being served cannot leave its wasm frames: a wasm32
/// program's, a stateful provider's, or guest code a provider called (its
/// frames are below the call-out).
fn frames_stay() -> bool {
    WASM32.load(std::sync::atomic::Ordering::Acquire)
        || IN_PROVIDER.with(|c| c.get())
        || cgen::provider_callout_open()
}

/// `Kernel::wasm32` of this process, for what asks it without the kernel
/// (`frames_stay`). Set where the kernel's is, and by `set_host`.
pub(crate) static WASM32: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A thread or a fork a wasm32 program's syscall staged without trapping
/// (`gex_wasmprog_staged`).
///
/// Per Worker: the Worker whose syscall staged it is the one that must start
/// it (a fork's child is a copy of memory as the call left it; see
/// `gex_fork_pending`). Shared, a sibling thread's syscall took the flag and
/// the forking Worker never drained its child.
thread_local! {
    static WASM32_STAGED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a wasm32 program's last syscall staged a thread or a child for
/// the host (and forget it).
#[no_mangle]
pub extern "C" fn gex_wasmprog_staged() -> i32 {
    WASM32_STAGED.with(|c| c.replace(false)) as i32
}

thread_local! {
    /// `wasm32_wait` has said this Worker's program waits on the page.
    static PAGE_WAIT_SAID: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A wasm32 program's blocking call (cgen/wasmprog.rs): its frames cannot be
/// given back to JS, so the Worker waits here until the call can finish, and
/// the call returns. Anything else that ends the wait (the process gone)
/// traps as usual.
fn wasm32_wait(pc: u64) -> i32 {
    // Narration, once per Worker: a wasm32 program's every blocking call
    // comes through here (all of W3's Wine), and a line per call was about
    // 90 console.debug lines a second on Undertale; its wined3d thread spent
    // 1.4 ms a frame in them under the profiler. The check also took the
    // kernel's lock on every pass of the loop; it is made on the first pass.
    //
    // A pass of this loop asks `schedule`, whose turn for a wait already
    // registered takes no lock (`waitq::idle_turn`), and parks on this
    // Worker's wait slot.
    let mut said = PAGE_WAIT_SAID.with(|c| c.get());
    loop {
        // A pass reads the clock once (`turn_now`); the park below lets
        // time pass, and the next pass reads it again.
        let _now = NowScope::open();
        let picked = schedule();
        if picked == 1 {
            let here = my_ctx();
            if resume() == Some(pc + gex_abi::SYSCALL_INST_BYTES) && resume_ctx().unwrap_or(here) == here {
                set_resume_none();
                set_resume_ctx_none();
                return 0;
            }
            return picked;
        }
        // An idle turn saw this Worker's thread still in a registered wait;
        // any other way out of `schedule` must be asked.
        let idle = waitq::waiting_here();
        if exited() || SCHED_OVER.with(|c| c.replace(false)) {
            return picked;
        }
        // Bit 4 of `paused()`: the idle turn found a wait only the page ends.
        if !said && idle && paused() & 4 != 0 {
            // A wait on the page itself (its keyboard, the Worker's own
            // network) needs this Worker's event loop, which a wasm32
            // program's frames hold: it is polled here instead.
            trace_line("gex: a wasm32 program waits on the page (stdin or the network): polled\n");
            PAGE_WAIT_SAID.with(|c| c.set(true));
        }
        said = true;
        if unsafe { k_present() } == 0 {
            continue;
        }
        if waitq::wants_page() == Some(true) {
            // The page's own input, which this Worker's frames keep it from
            // being handed: looked at again in 20 ms, as before the wait
            // queues (a keystroke into the kernel's queue notifies the slot
            // sooner).
            waitq::park_bounded(20_000_000);
        } else {
            gex_park(0);
        }
    }
}

fn do_fork(h: &mut Host, pid: i32, pc: u64, child_stack: u64, tls: Option<u64>, start: Option<u64>) {
    let ctx_addr = h.threads[cur()].ctx_addr();
    // Not with a sibling mid-translation: the child is a copy of this memory,
    // and a translator outside the lock is changing the heap.
    jit_in_held(&mut h.jit);
    // The parent carries straight on, keeping its memory, threads and tables:
    // the child is a separate Worker and will not use them.
    {
        let mut r = h.threads[cur()].run();
        r.pc = pc + gex_abi::SYSCALL_INST_BYTES;
        r.waiting = Waiting::Runnable;
        r.blocked_on = None;
    }

    // The child is a *Worker*: its own memory, its own shim, its own tables.
    // What it needs from here is the parent's mapped regions copied into that
    // memory, and a context to start at -- `pc + 4`, with `x0` of zero, on
    // `child_stack` if `clone` named one. The `FORK` slots below are what the
    // host reads (via `gex_fork_pending`) to build the Worker, which then calls
    // `gex_adopt_child` to become the process.
    stage_fork_fds(h, pid);
    // The working directory is per-process state the child must inherit: its
    // fresh kernel starts at `/`, so hand it the parent's cwd to apply in
    // `gex_adopt_child`, or `sh -c 'cd d; make'` builds in the wrong place.
    // The calling thread's (its fs_struct's, which a fork copies).
    *FORK_CWD.0.borrow_mut() = h.kernel.caller_cwd();
    // And the program, which a fork child is too: `/proc/self/{exe,cmdline}`
    // (tests/gexcc/longargs re-execs `/proc/self/exe` from a child).
    // Dispositions (an ignored SIGPIPE, SIGINT), mask, umask and limits,
    // which a child inherits and keeps across its `execve`.
    *FORK_STATE.0.borrow_mut() = state_bytes(&h.kernel, false);
    *FORK_EXE.0.borrow_mut() = std::iter::once(h.kernel.exe_path.as_str())
        .chain(h.kernel.argv.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join("\0");
    FORK[1].store(start.unwrap_or(pc + gex_abi::SYSCALL_INST_BYTES), std::sync::atomic::Ordering::Relaxed);
    FORK[2].store(ctx_addr, std::sync::atomic::Ordering::Relaxed);
    FORK[3].store(child_stack, std::sync::atomic::Ordering::Relaxed);
    FORK[4].store(tls.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
    FORK[5].store(h.kernel.mem.lock().brk_current(), std::sync::atomic::Ordering::Relaxed);
    // Only the forking thread's Worker may claim it (`gex_fork_pending`).
    FORK_BY.store(cur() as u64 + 1, std::sync::atomic::Ordering::Relaxed);
    // Last, because it is what the host tests: the rest must be readable by
    // the time a non-zero pid appears.
    FORK[0].store(pid as u64, std::sync::atomic::Ordering::Release);
}

/// The child's descriptor table, as text, staged for the host to carry.
///
/// An `Arc<OpenFile>` is a pointer into a heap the child's Worker never saw, so
/// the child is sent a *description* of each descriptor and rebuilds it. Only
/// what can be rebuilt travels (a shared-root file or a global-kernel pipe, an
/// id or path meaning the same in any process); a device node or host socket is
/// left closed rather than silently replaced.
///
/// One line per descriptor:
///
///     <fd> <cloexec> <flags> <offset> <path>
///
/// Space-separated, path last, since a guest path may hold anything but newline.
static FORK_FDS: SingleThreaded<String> = SingleThreaded(const { RefCell::new(String::new()) });
/// The parent's working directory at `fork`, staged for the child to inherit.
static FORK_CWD: SingleThreaded<String> = SingleThreaded(const { RefCell::new(String::new()) });
/// The parent's exe path then its argv, NUL-separated, staged at `fork`.
static FORK_EXE: SingleThreaded<String> = SingleThreaded(const { RefCell::new(String::new()) });
/// The parent's signal state, umask and limits at `fork` (`state_bytes`),
/// staged for the child to inherit.
static FORK_STATE: SingleThreaded<Vec<u8>> = SingleThreaded(const { RefCell::new(Vec::new()) });

#[no_mangle]
pub extern "C" fn gex_fork_state_ptr() -> u32 {
    FORK_STATE.0.borrow().as_ptr() as u32
}

#[no_mangle]
pub extern "C" fn gex_fork_state_len() -> u32 {
    FORK_STATE.0.borrow().len() as u32
}

/// The child's side of `gex_fork_state_*`, before `gex_adopt_child`.
static CHILD_STATE: SingleThreaded<Vec<u8>> = SingleThreaded(const { RefCell::new(Vec::new()) });

#[no_mangle]
pub extern "C" fn gex_set_child_state(ptr: u32, len: u32) {
    *CHILD_STATE.0.borrow_mut() = shim_slice(ptr, len).to_vec();
}

/// The signal state, umask and resource limits a process passes on: to a
/// fork's child (`pending` false), or with its pending signals across an
/// exec that starts in another Worker.
fn state_bytes(k: &Kernel, pending: bool) -> Vec<u8> {
    let sig = k.signals.to_bytes(pending);
    let limits = k.rlimits.get();
    let mut v = Vec::with_capacity(8 + sig.len() + limits.len() * 16);
    v.extend_from_slice(&(sig.len() as u32).to_le_bytes());
    v.extend_from_slice(&sig);
    v.extend_from_slice(&k.proc_fs.umask().to_le_bytes());
    for [soft, hard] in limits {
        v.extend_from_slice(&soft.to_le_bytes());
        v.extend_from_slice(&hard.to_le_bytes());
    }
    v
}

/// [`state_bytes`] into a kernel being built. Bytes that are not one
/// change nothing.
fn apply_state(k: &mut Kernel, b: &[u8]) {
    let Some(n) = b.get(..4).map(|w| u32::from_le_bytes(w.try_into().unwrap()) as usize) else { return };
    let Some(sig) = b.get(4..4 + n).and_then(gex_kernel::signal::Signals::from_bytes) else { return };
    let rest = &b[4 + n..];
    let mut limits = k.rlimits.get();
    if rest.len() != 4 + limits.len() * 16 {
        return;
    }
    k.signals = sig;
    k.thread_signals.clear();
    k.sig_tid = 0;
    k.proc_fs.set_umask(u32::from_le_bytes(rest[..4].try_into().unwrap()));
    for (i, lim) in limits.iter_mut().enumerate() {
        let at = 4 + i * 16;
        lim[0] = u64::from_le_bytes(rest[at..at + 8].try_into().unwrap());
        lim[1] = u64::from_le_bytes(rest[at + 8..at + 16].try_into().unwrap());
    }
    k.rlimits.set(limits);
}

#[no_mangle]
pub extern "C" fn gex_fork_exe_ptr() -> u32 {
    FORK_EXE.0.borrow().as_ptr() as u32
}

#[no_mangle]
pub extern "C" fn gex_fork_exe_len() -> u32 {
    FORK_EXE.0.borrow().len() as u32
}

/// The child's side of `gex_fork_exe_*`, before `gex_adopt_child`.
static CHILD_EXE: SingleThreaded<String> = SingleThreaded(const { RefCell::new(String::new()) });

#[no_mangle]
pub extern "C" fn gex_set_child_exe(ptr: u32, len: u32) {
    if let Ok(s) = std::str::from_utf8(shim_slice(ptr, len)) {
        *CHILD_EXE.0.borrow_mut() = s.to_string();
    }
}

/// Describe the calling process's descriptors for the child. `child` is who the
/// reopened pipe ends belong to: opened under the parent's pid instead, the
/// kernel credits the parent, and a crashed child releases no ends, so the
/// reader waits forever on an end-of-file the dead child's end holds shut.
fn stage_fork_fds(h: &Host, child: i32) {
    let mut out = String::new();
    for (fd, file, cloexec) in h.kernel.fds.entries() {
        let Some(name) = carry_name(&file, child) else {
            continue;
        };
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

/// The name a descriptor travels to another process by -- a fork's child, or
/// the far end of `SCM_RIGHTS` -- taking whatever reference keeps it open on
/// the way. `holder` is who a pipe or pty end is opened for: the child, or no
/// one yet (the global kernel moves it to the receiver).
///
/// A pipe end is re-opened in the global kernel, or the sender closing its
/// copy looks like the last reference and the reader sees a false EOF. A file
/// the global kernel holds goes as a duplicated id, which survives the unlink
/// of a file the receiver would otherwise fail to reopen by name; anything
/// else with a path, by path. An epoll set goes as its registrations (the
/// same descriptor numbers name the same files in a fork's child): a copy, so
/// an `epoll_ctl` on one side after the fork is not seen by the other, where
/// Linux shares the one set. `None`: nothing with a name -- a local socket.
fn carry_name(file: &gex_kernel::file::OpenFile, holder: i32) -> Option<String> {
    if let Some(id) = pipe_id(&file.path) {
        unsafe { k_pipe_open_end(id, file.writable as i32, holder) };
        return Some(file.path.clone());
    }
    if let Some((id, master)) = pty_ref(&file.path) {
        unsafe { k_pty_open_end(id, master as i32, holder) };
        pty_note("hands on", id, master as i32, holder);
        return Some(file.path.clone());
    }
    if let Some(id) = file.handle.remote_id() {
        let dup = unsafe { k_fs_dup(id) };
        if dup < 0 {
            warn_line(&format!("gex: could not hand {} to another process\n", file.path));
            return None;
        }
        // With its path after a colon, for what reads it back (`/proc`,
        // W3's image hook finding the module beside a DLL).
        return Some(format!("#{dup}:{}", file.path));
    }
    if let Some(sock) = gex_kernel::syscalls::Kernel::remote_unix_of_file(file) {
        // A socket in the global kernel: another id on the same end.
        let dup = sock.dup_id()?;
        return Some(format!("sock:{dup}:{}", sock.listening as i32));
    }
    if let Some(set) = Arc::clone(&file.handle).as_epoll() {
        let key = Arc::as_ptr(&file.handle) as *const () as usize;
        return Some(format!("epoll:{key}:{}", set.describe()));
    }
    // A connected TCP socket: the same connection, held once more (its Wisp
    // stream, or its end in the global kernel). Wine's wineserver makes every
    // socket and connects it, then hands the client the descriptor.
    if let Some(sock) = Arc::clone(&file.handle).as_inet_socket() {
        if let Some(name) = sock.carry() {
            return Some(name);
        }
    }
    file.path.starts_with('/').then(|| file.path.clone())
}

thread_local! {
    /// While `gex_adopt_fds` runs: the epoll sets rebuilt so far, by the
    /// parent's key, so two descriptors for one set stay one set.
    static ADOPTED_EPOLLS: RefCell<Option<std::collections::HashMap<String, Arc<gex_kernel::file::OpenFile>>>> =
        const { RefCell::new(None) };
}

/// The console descriptors, rebuilt in a child from the name alone. A fork
/// stages each descriptor as a path for the child to reopen, which works for a
/// file but not the terminal -- a Worker's own `Console`, named `/dev/stdin`
/// etc. but existing nowhere to open. Without this a dup of the terminal (dash
/// dups it to fd 10 for job control) is dropped, `tcsetpgrp` returns `EBADF`,
/// and the child dies before its `execve`.
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
/// The global kernel's id of the pipe end descriptor `fd` is, or -1: the
/// wineserver's door kick pipe, which a door call in another process
/// writes (gex.mjs `#doorPublish`, docs/wineserver-kernel.md).
#[no_mangle]
pub extern "C" fn gex_fd_pipe_id(fd: i32) -> i32 {
    fds_free().and_then(|fds| fds.get(fd).ok()).and_then(|f| pipe_id(&f.path)).unwrap_or(-1)
}

fn pipe_id(path: &str) -> Option<i32> {
    path.strip_prefix("pipe:[")?.strip_suffix(']')?.parse().ok()
}

/// The `(id, master)` in a pty path like `pty:[3:m]` / `pty:[3:s]`.
fn pty_ref(path: &str) -> Option<(i32, bool)> {
    let inner = path.strip_prefix("pty:[")?.strip_suffix(']')?;
    let (id, side) = inner.split_once(':')?;
    Some((id.parse().ok()?, side == "m"))
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

/// The parent's working directory, staged at `fork` for the child to adopt.
#[no_mangle]
pub extern "C" fn gex_fork_cwd_ptr() -> u32 {
    FORK_CWD.0.borrow().as_ptr() as u32
}

#[no_mangle]
pub extern "C" fn gex_fork_cwd_len() -> u32 {
    FORK_CWD.0.borrow().len() as u32
}

/// Rebuild the descriptors the parent had, in the child.
///
/// Called once by the child's Worker after `gex_adopt_child`. stdin/out/err are
/// already installed, so a line naming one replaces it -- what a shell
/// redirecting a child's output needs.
#[no_mangle]
pub extern "C" fn gex_adopt_fds(ptr: u32, len: u32) -> i32 {
    phase(5);
    let Ok(text) = std::str::from_utf8(shim_slice(ptr, len)) else {
        return -1;
    };
    adopt_fds_text(&text.to_string())
}

/// [`gex_adopt_fds`]'s work, on `stage_fork_fds`'s text.
fn adopt_fds_text(text: &str) -> i32 {
    ADOPTED_EPOLLS.with(|m| *m.borrow_mut() = Some(Default::default()));
    // No kernel lock (L25c): the names resolve in the process's published
    // namespace, and the descriptor table locks itself.
    let home = sigs::proc_home();
    let ns = home.ns();
    let fds = fds_free().expect("gex: descriptors adopted before their process");
    let n = {
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
            let file = match rebuild_carried(&ns, path, flags, offset) {
                Ok(f) => f,
                Err(e) => {
                    // Never quietly: a missing descriptor is an `EBADF` at
                    // the child's first use, far from here.
                    warn_line(&format!("gex: fork left fd {fd} ({path}) behind: {e}\n"));
                    continue;
                }
            };
            if fds.install(fd, file, cloexec != 0).is_ok() {
                n += 1;
            }
        }
        n
    };
    ADOPTED_EPOLLS.with(|m| *m.borrow_mut() = None);
    n
}

/// The other half of [`carry_name`]: the open file a name stands for, in
/// this process. The reference the sender took is this one now; nothing more
/// is opened for a pipe, pty, file id or socket id.
fn rebuild_carried(
    ns: &gex_kernel::vfs::Namespace,
    path: &str,
    flags: u64,
    offset: u64,
) -> Result<Arc<gex_kernel::file::OpenFile>, String> {
    use gex_kernel::file::OpenFile;
    use gex_kernel::fs::oflags;
    let acc = flags & oflags::ACCMODE;
    let file = if let Some(id) = pipe_id(path) {
        let r = global_pipes().ok_or("no global kernel")?;
        let write = acc != oflags::O_RDONLY;
        OpenFile::new(
            Arc::new(gex_kernel::pipe::RemotePipeEnd { id, write, remote: r }),
            path.to_string(),
            flags,
            !write,
            write,
        )
    } else if let Some((id, master)) = pty_ref(path) {
        let r = global_ptys().ok_or("no global kernel")?;
        OpenFile::new(
            Arc::new(gex_kernel::pty::RemotePty { id, master, remote: r }),
            path.to_string(),
            flags,
            true,
            true,
        )
    } else if let Some((id, name)) = path.strip_prefix('#').and_then(|v| {
        let (id, name) = v.split_once(':').unwrap_or((v, ""));
        Some((id.parse::<i32>().ok()?, name))
    }) {
        let root = global_root().ok_or("no global kernel")?;
        let f = OpenFile::new(
            gex_kernel::vfs::remote::adopt(root.remote(), id),
            name.to_string(),
            flags,
            acc != oflags::O_WRONLY,
            acc != oflags::O_RDONLY,
        );
        f.set_offset(offset);
        f
    } else if let Some(rest) = path.strip_prefix("epoll:") {
        let (key, watches) = rest.split_once(':').ok_or("bad epoll name")?;
        let watches = gex_kernel::unix::EpollSet::parse(watches).ok_or("bad epoll registrations")?;
        if let Some(f) = ADOPTED_EPOLLS.with(|m| m.borrow().as_ref().and_then(|m| m.get(key).cloned())) {
            return Ok(f);
        }
        let f = gex_kernel::syscalls::Kernel::adopt_epoll(watches, flags);
        ADOPTED_EPOLLS.with(|m| {
            if let Some(m) = m.borrow_mut().as_mut() {
                m.insert(key.to_string(), Arc::clone(&f));
            }
        });
        f
    } else if let Some(rest) = path.strip_prefix("sock:") {
        let r = global_sockets().ok_or("no global kernel")?;
        let (id, listening) = rest.split_once(':').ok_or("bad socket name")?;
        let id = id.parse::<i32>().map_err(|_| "bad socket id")?;
        let sock = Arc::new(gex_kernel::unix::RemoteUnix::new(id, listening == "1", None, r));
        sock.set_nonblocking(flags & oflags::O_NONBLOCK != 0);
        let handle: Arc<dyn FileHandle> = sock;
        OpenFile::new(handle, "socket".to_string(), flags, true, true)
    } else if path.starts_with("inet:") {
        // `Socket::carry`'s name: the connection the sender holds, already
        // counted as this holder's.
        let t = net::Socket::carried_transport(path).ok_or("bad inet name")?;
        let transport: Arc<dyn net::Transport> = if let Some(id) = t.strip_prefix("wisp:") {
            let id = id.parse::<i32>().map_err(|_| "bad stream id")?;
            Arc::new(WispStream { id: std::sync::atomic::AtomicI32::new(id) })
        } else if let Some(id) = t.strip_prefix("loop:") {
            let id = id.parse::<i32>().map_err(|_| "bad socket id")?;
            Arc::new(net::RemoteEnd::adopt(id, global_sockets().ok_or("no global kernel")?))
        } else {
            return Err(format!("unknown connection '{t}'"));
        };
        let sock = net::Socket::adopt(
            path,
            transport,
            Arc::new(WispNet),
            Arc::new(net::Resolver::new()),
            Arc::new(net::Ports::new()),
            global_sockets(),
        )
        .ok_or("bad inet name")?;
        sock.set_nonblocking(flags & oflags::O_NONBLOCK != 0);
        let handle: Arc<dyn FileHandle> = sock;
        OpenFile::new(handle, "socket".to_string(), flags, true, true)
    } else if let Some(rest) = path.strip_prefix("local:") {
        let (pid, n) = rest.split_once(':').ok_or("bad local name")?;
        if pid.parse::<i32>().ok() != Some(my_pid()) {
            return Err(format!("parked in pid {pid}, not here"));
        }
        let n = n.parse::<u32>().map_err(|_| "bad local number")?;
        let mut t = LOCAL_FDS.lock().unwrap_or_else(|e| e.into_inner());
        let i = t.1.iter().position(|e| e.0 == n).ok_or("already taken")?;
        return Ok(t.1.swap_remove(i).1);
    } else if let Some((handle, readable, writable)) = console_handle(path) {
        // The terminal, which is this Worker's and not a file.
        OpenFile::new(handle, path.to_string(), flags, readable, writable)
    } else {
        // Through this process's own mounts, not the bare root: `/dev` and
        // `/proc` are per-process, so `/dev/null` is not in the shared
        // filesystem by name.
        use gex_kernel::vfs::FileSystem;
        let handle = ns
            .open(path, flags, 0)
            .or_else(|e| match global_root() {
                Some(root) => root.open(path.trim_start_matches('/'), flags, 0),
                None => Err(e),
            })
            .map_err(|e| format!("errno {}", e.0))?;
        let f = OpenFile::new(handle, path.to_string(), flags, acc != oflags::O_WRONLY, acc != oflags::O_RDONLY);
        f.set_offset(offset);
        f
    };
    Ok(file)
}

/// W64 (an x86-64 guest, the native PE core): the window for the core's
/// data, the top of the guest's space under KUSER_SHARED_DATA, kept from
/// every other mapping. A PE32+ export's RVA is added to its DLL's base in
/// 64 bits, so a data export below a skeleton's base would come out past
/// 4 GiB (ws2_32's `__wine_unix_call_dispatcher` lookup did). Natively the
/// data goes above 2 GiB; the page's guest ends below that.
pub(crate) const W64_PE_DATA: (u64, u64) = (0x7fde_0000, 0x7ffe_0000);

/// Reserve [`W64_PE_DATA`] (no access until the core's data is placed in
/// it) and keep mmap below it.
pub(crate) fn w64_reserve_pe_data(mem: &mut gex_kernel::mem::MemoryMap) {
    use gex_kernel::mem::{flags, prot};
    let (lo, hi) = W64_PE_DATA;
    if let Err(e) = mem.mmap(lo, hi - lo, prot::NONE, flags::PRIVATE | flags::ANONYMOUS | flags::FIXED_NOREPLACE, "[w64 PE data window]") {
        warn_line(&format!("gex: W64: reserving the PE data window {lo:#x}..{hi:#x}: errno {}\n", e.0));
    }
    mem.set_mmap_top(lo);
}

/// The kernel's half of a W3 process's address space: no mapping above
/// KUSER_SHARED_DATA unless asked for there, and the i386 code kept off
/// whatever is above 2 GiB. With the NT layer's split (`lo` its line), what
/// the host left below 2 GiB is the process's own arena; with W3's compact
/// space (`nt::w3_windows_space`) the arena grows on demand, shared with
/// the shim's heap, as for any process.
pub(crate) fn w3_address_space(mem: &mut gex_kernel::mem::MemoryMap, lo: u64) {
    mem.set_mmap_top(0x7ffe_0000);
    if !nt::w3_compact() {
        mem.adopt_arena(gex_kernel::mem::page_align_up(lo), GUEST_LIMIT);
    }
    // And the low space a wasm32 program leaves free, between the dispatch
    // directory and its stack (its image and break are above, in the
    // placement window): where a Windows executable's 0x400000 is.
    mem.adopt_arena(gex_abi::DISPATCH_DIR_END, gex_abi::STACK_TOP - gex_abi::STACK_SIZE);
    // An access there faults, as in the NT layer.
    gex_xlate::set_guard_limit(0x8000_0000);
    // The exe slot and pinned DLL band: once a fixed mapping has made them
    // arena, a bottom-up anonymous mmap (Wine's 64 MiB heaps) took their
    // holes and the next pinned DLL found its base taken and relocated
    // (steam_api64, uxtheme, actxprxy: every one a unit miss).
    let (lo, hi) = nt::W3_ISLAND;
    mem.keep_fixed_only(lo, hi);
}

/// `gex.kuser_address()` (W3's Wine, at start): where this gex has i386
/// code read KUSER_SHARED_DATA. `gex_abi::W3_KUSER` when the loader's
/// header asked for the fold (`FLAG_KUSER_W3`), else Windows' 0x7ffe0000.
/// Wine stops at once when it maps the page elsewhere, rather than run with
/// every constant read of it landing on nothing.
#[no_mangle]
pub extern "C" fn gex_kuser_address() -> u32 {
    if gex_abi::kuser_fold() { gex_abi::W3_KUSER as u32 } else { gex_abi::KUSER_WINDOWS as u32 }
}

/// Where this process's Windows address space starts (W3), or 0: what a fork
/// hands its child to split at the same line.
#[no_mangle]
pub extern "C" fn gex_windows_space() -> u32 {
    nt::windows_space() as u32
}

/// A fresh W3 process (the host saw the program's i386 flag before loading
/// it): the split, first thing after instantiation, so the line sits at the
/// bottom of the arena rather than above the startup heap. Where the Windows
/// space starts, or 0 on failure.
#[no_mangle]
pub extern "C" fn gex_split_windows_space() -> u32 {
    // W3's compact space (see `nt::w3_windows_space`), or the split when the
    // site asked (`gex_set_w3_split`); the name is the JS side's.
    match nt::w3_space() {
        Ok(lo) => lo as u32,
        Err(e) => {
            warn_line(&format!("gex: W3: {e}\n"));
            0
        }
    }
}

/// `?w3split=1`: this process's W3 address space is the NT layer's split, not
/// the compact space (`nt::w3_space`). Set before the space is taken.
#[no_mangle]
pub extern "C" fn gex_set_w3_split(on: u32) {
    nt::set_w3_split(on != 0);
}

/// A forked child of a W3 process: the split, first thing after
/// instantiation, before the heap takes a span. Zero, or -1 on failure.
#[no_mangle]
pub extern "C" fn gex_adopt_windows_space(lo: u32) -> i32 {
    match nt::adopt_windows_space(lo as u64) {
        Ok(()) => 0,
        Err(e) => {
            warn_line(&format!("gex: W3 child: {e}\n"));
            -1
        }
    }
}

/// A child `fork` has made that the host has not built a Worker for: pid, entry
/// pc, context address, child stack, TLS. Plain words, not a queue: the host
/// drains this before the parent runs again, so there is only ever one.
static FORK: [std::sync::atomic::AtomicU64; 6] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 6];

/// The thread (index + 1) whose `fork` is in `FORK`.
static FORK_BY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The pid of a child needing a Worker, or zero -- and claiming it, so a `fork`
/// several Workers can see is not started twice (see `gex_thread_pending`).
///
/// Only on the Worker running the thread that forked: the child is a copy of
/// memory as `fork` left it, and only that Worker copies it before the parent
/// runs on (synchronously after a wasm32 program's syscall, before the run
/// loop resumes tier-1's). A sibling thread's Worker that drained it instead
/// copied memory while the parent ran on -- for a wasm32 program, after
/// `__gex_fork` had unmapped the record the child starts on, so the child
/// read its thread pointer from a zero page and trapped (the aarch64 browser
/// flake in tests/wasm32 fork: "vfork child exit 137" with a parked thread
/// in the parent, 6 in 750 vforks; tests/wasm32/vforkthreads).
#[no_mangle]
pub extern "C" fn gex_fork_pending() -> i32 {
    if FORK[0].load(std::sync::atomic::Ordering::Acquire) == 0
        || FORK_BY.load(std::sync::atomic::Ordering::Relaxed) != cur() as u64 + 1
    {
        return 0;
    }
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

/// Where the parent's heap had reached. A child inherits the break with the
/// memory behind it; a break of zero is not a smaller heap but one growing from
/// address zero over the register contexts.
#[no_mangle]
pub extern "C" fn gex_fork_brk() -> i64 {
    claimed_fork(4)
}

/// Where this Worker re-enters the guest, and why it stopped. Per Worker: on
/// the shared `Host` one thread's resume address overwrote another's, and a
/// Worker resumed at zero with a good pc in its own context.
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

thread_local! {
    /// This Worker's last resumes set (the shim's line that set it, pc), for
    /// the wild-branch report: who sent the guest where it went.
    static RESUME_RING: std::cell::RefCell<Vec<(u32, u64)>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[track_caller]
fn set_resume(v: u64) {
    RESUME.with(|c| c.set(Some(v)));
    let line = std::panic::Location::caller().line();
    RESUME_RING.with(|r| {
        let mut r = r.borrow_mut();
        if r.len() == 16 {
            r.remove(0);
        }
        r.push((line, v));
    });
    // GEX_NT_RESUME_LOG: each (call site, pc) a resume was set from, once:
    // where the guest leaves wasm and comes back (an AOT-only run needs a
    // unit or region entry at every such pc).
    if RESUME_LOG.load(std::sync::atomic::Ordering::Relaxed) {
        let at = std::panic::Location::caller();
        static SEEN: std::sync::Mutex<Option<std::collections::HashSet<(u32, u64)>>> = std::sync::Mutex::new(None);
        let fresh = SEEN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert((at.line(), v));
        if fresh {
            warn_line(&format!("gex-nt: resume at {v:#x} from {}:{}\n", at.file(), at.line()));
        }
    }
}

pub(crate) static RESUME_LOG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn set_resume_none() {
    RESUME.with(|c| c.set(None));
}

fn resume_ctx() -> Option<u64> {
    RESUME_CTX.with(|c| c.get())
}

fn set_resume_ctx(v: u64) {
    ctx_addr_set(v);
    RESUME_CTX.with(|c| c.set(Some(v)));
}

/// Where the running thread's register context is, without asking the kernel.
///
/// Every question the crash reporter asks used to go through `with_exec_lock`, and
/// a crash inside a `with_exec_lock` closure leaves the kernel borrowed -- so each
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
///
/// Per Worker: it was one static for the process, so with threads it named
/// whichever Worker last made a syscall -- and the run loop's recovery from a
/// table that lagged a sibling's numbering resumed the trapping thread at
/// *another thread's* last branch target (`gex_last_target`). In Noita a
/// worker thread calling a functor's `_Do_call` came back at the neighbouring
/// slot of the same vtable, and returned with its stack 4 bytes off.
#[cfg(feature = "shared-memory")]
thread_local! {
    static CTX_ADDR: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
#[cfg(feature = "shared-memory")]
fn ctx_addr_get() -> u64 {
    CTX_ADDR.with(|c| c.get())
}
#[cfg(feature = "shared-memory")]
fn ctx_addr_set(v: u64) {
    CTX_ADDR.with(|c| c.set(v));
}
#[cfg(not(feature = "shared-memory"))]
static CTX_ADDR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(not(feature = "shared-memory"))]
fn ctx_addr_get() -> u64 {
    CTX_ADDR.load(std::sync::atomic::Ordering::Relaxed)
}
#[cfg(not(feature = "shared-memory"))]
fn ctx_addr_set(v: u64) {
    CTX_ADDR.store(v, std::sync::atomic::Ordering::Relaxed);
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

/// Run thread `i` from here on, its signal state this Worker's (`sigs`).
/// Under the kernel lock, which `h` keeps every caller honest about.
fn set_current(h: &mut Host, i: usize) {
    CURRENT.with(|c| c.set(i));
    bind_mine(h, i);
}

/// Make thread `i`'s own state this Worker's: what it asks about its thread
/// without the kernel (its context, its tid, its signals).
pub(crate) fn bind_mine(h: &mut Host, i: usize) {
    if let Some(t) = h.threads.get(i) {
        let (tid, ctx_addr) = (t.tid(), t.ctx_addr());
        MY_CTX.with(|c| c.set(ctx_addr));
        MY_TID.with(|c| c.set(tid));
        MY_RUN.with(|c| *c.borrow_mut() = Some(t.state_arc()));
        sigs::bind(&mut h.kernel, tid);
    }
}

thread_local! {
    /// The first turn of the `schedule` a class call's park goes on to,
    /// made in the call (`park_free`).
    static FIRST_PICK: RefCell<Option<Pick>> = const { RefCell::new(None) };
    /// The last `schedule` found this Worker's thread exited, or its whole
    /// process: what `wasm32_wait` asks instead of the thread table.
    static SCHED_OVER: Cell<bool> = const { Cell::new(false) };
    /// This Worker's thread's register context (`set_current`): its own,
    /// for what asks without the kernel.
    static MY_CTX: Cell<u64> = const { Cell::new(0) };
    /// And its tid.
    static MY_TID: Cell<i32> = const { Cell::new(0) };
    /// And its run state ([`Run`]), which this Worker changes without the
    /// kernel lock.
    /// It is the thread's slot state, so its `gone` and `halted` with it.
    static MY_RUN: RefCell<Option<Arc<ThreadCell>>> = const { RefCell::new(None) };
}

/// Whether this Worker's thread went with its old program when another
/// thread of the process called `execve`. Its index into the thread table
/// (`cur()`) means nothing any more: the table is the new program's.
pub(crate) fn thread_gone() -> bool {
    MY_RUN.with(|c| c.borrow().as_ref().is_some_and(|t| t.gone.load(std::sync::atomic::Ordering::SeqCst)))
}

/// This Worker's thread, found [`thread_gone`], stops: it is exited, runs no
/// more guest code, and says so to the `execve` waiting for it
/// ([`ThreadCell::halted`]). Nowhere to resume.
pub(crate) fn stop_gone() {
    if let Some(run) = MY_RUN.with(|c| c.borrow().clone()) {
        run.lock().unwrap_or_else(|e| e.into_inner()).waiting = Waiting::Exited;
        run.halted.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    set_resume_none();
    set_resume_ctx_none();
}

/// [`thread_gone`], for the host: a process Worker whose thread is gone must
/// not end the process, which runs on in another Worker.
#[no_mangle]
pub extern "C" fn gex_thread_gone() -> i32 {
    thread_gone() as i32
}

/// [`MY_CTX`].
pub(crate) fn my_ctx() -> u64 {
    MY_CTX.with(|c| c.get())
}

/// [`MY_TID`].
pub(crate) fn my_tid() -> i32 {
    MY_TID.with(|c| c.get())
}

/// A thread `clone` has made that the host has not made a Worker for:
/// tid, context address, entry pc.
///
/// The same shape as `FORK` and for the same reason: `clone` returns to the
/// parent immediately and the host drains this before the parent runs again,
/// so there is only ever one.
static THREAD: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// `THREAD[0]` while a stage writes the other two words (`stage_thread`).
const STAGING: u64 = u64::MAX;

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

/// A thread `clone` has made, by either path (L25b): into the thread table,
/// staged for the host to give a Worker of its own, and the creator on
/// after its `clone` (whose answer, the tid, is in its context). No kernel
/// lock: the table claims a slot with an atomic add, the registry backfill
/// is the JIT state's, and the stage is claimed by a swap. The stop value.
fn thread_made(tid: i32, ctx_addr: u64, syscall_pc: u64) -> i32 {
    // The kernel has already written the child's context; it starts at the
    // pc that context carries.
    let start = GuestMem::new(guest_bytes()).read_u64(ctx_addr + ctx::PC as u64).unwrap_or(0);
    // A Worker of its own. Always: a thread is a Worker here.
    //
    // The table entry is still this process's -- `join`, the futexes and
    // `live_threads` are all counted here -- but the thread runs on another
    // stack, and `Elsewhere` is the state that says "not me". Without it two
    // Workers would run the same context.
    sigs::proc_home().table().push(tid, ctx_addr, ThreadCell::new(Run::new(start, Waiting::Elsewhere)));
    // From here the process has a second Worker, so translated modules must
    // be kept for it to catch up. Until now they were not (see
    // `gex_jit_installed`) -- so on this first edge, take back into the
    // registry everything already installed, or a joining thread traps in
    // the low startup pages the main Worker lifted before this clone (see
    // `backfill_registry`).
    with_jit_only(|j| {
        if !j.ever_threaded {
            backfill_registry(j);
            j.ever_threaded = true;
            regview::set_threaded(true);
        }
    });
    stage_thread(tid, ctx_addr, start);
    if frames_stay() {
        // A wasm32 program's frames cannot be left: the call returns, and
        // `#wasmprogSyscall` starts the thread.
        WASM32_STAGED.with(|c| c.set(true));
        return 0;
    }
    // The stack goes back so the host can see the record and start the
    // Worker, exactly as a `fork` does. The creator resumes where it was:
    // `clone` returns the tid to it.
    set_resume(syscall_pc + gex_abi::SYSCALL_INST_BYTES);
    set_resume_ctx(my_ctx());
    1
}

fn stage_thread(tid: i32, ctx_addr: u64, pc: u64) {
    // One slot, shared by every Worker. A second clone must not overwrite a
    // triple the claiming Worker has not finished reading: `gex_thread_pending`
    // takes the tid and *then* the two context words, so an overwrite in that
    // window hands a thread the wrong `ctx`/`pc` -- a Worker running a stranger's
    // registers, which is exactly the wild-branch crash GTK's startup (several
    // threads spawned at once) hit. So wait for the slot to be claimed (tid back
    // to 0) before staging the next; the claim is quick (the clone syscall
    // returns straight to the JS that reads it), so this spins rarely and
    // briefly. Bounded only to turn a never-claimed slot into a loud message
    // rather than a silent hang under `ExecLock`.
    // Without the kernel lock two Workers may stage at once: the slot is
    // taken for writing by a swap from 0 to `STAGING`, which the claim
    // (`gex_thread_pending`) passes over.
    let mut spins: u64 = 0;
    while THREAD[0]
        .compare_exchange_weak(0, STAGING, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
        spins += 1;
        if spins == 1 << 30 {
            warn_line("gex: stage_thread: prior thread slot never claimed\n");
            spins = 0;
        }
    }
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
    loop {
        let tid = THREAD[0].load(std::sync::atomic::Ordering::Acquire);
        if tid == 0 {
            return 0;
        }
        // Being staged: a moment.
        if tid == STAGING {
            core::hint::spin_loop();
            continue;
        }
        // Read the context words BEFORE claiming, then claim with a
        // compare-and-swap that also proves the tid did not change while they
        // were read. `stage_thread` will not overwrite a slot whose tid is still
        // set, so a successful swap means these two words are this tid's; a
        // failed one means another Worker claimed it (or a stale re-read) --
        // retry. This is the claim that keeps a Worker off a stranger's
        // registers; the swap alone (tid first, words after) let a racing stage
        // slip its own words in between.
        let ctx = THREAD[1].load(std::sync::atomic::Ordering::Relaxed);
        let pc = THREAD[2].load(std::sync::atomic::Ordering::Relaxed);
        if THREAD[0]
            .compare_exchange(
                tid,
                0,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
        {
            CLAIMED_THREAD.with(|c| c.set((ctx, pc)));
            return tid as i32;
        }
    }
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

/// Become the thread `tid`, in a Worker attached to this process's memory.
/// Nothing is built or copied -- a thread shares everything with its process,
/// already in the attached memory; this only sets which thread-table entry this
/// Worker runs. Returns the pc to enter at, or zero if there is no such thread.
#[no_mangle]
pub extern "C" fn gex_adopt_thread(tid: i32) -> i64 {
    phase(9);
    // No kernel lock (L25b): the thread table and the signal states lock
    // themselves, and the binding is this Worker's own.
    let Some(home) = sigs::home() else { return 0 };
    let table = home.table();
    let Some(i) = table.position(tid) else {
        return 0;
    };
    // Gone before it ever ran: another thread's `execve` is replacing the
    // program it was made for.
    if table[i].gone().load(std::sync::atomic::Ordering::SeqCst) {
        table[i].halted().store(true, std::sync::atomic::Ordering::SeqCst);
        return 0;
    }
    table[i].run().waiting = Waiting::Runnable;
    CURRENT.with(|c| c.set(i));
    bind_free(i);
    let pc = table[i].run().pc;
    pc as i64
}

/// [`bind_mine`] with no kernel lock (L25b, L25c): thread `i` of this
/// Worker's process, from its thread table and its signal states.
pub(crate) fn bind_free(i: usize) {
    let Some(home) = sigs::home() else { return };
    if let Some(t) = home.table().get(i) {
        let tid = t.tid();
        MY_CTX.with(|c| c.set(t.ctx_addr()));
        MY_TID.with(|c| c.set(tid));
        MY_RUN.with(|c| *c.borrow_mut() = Some(t.state_arc()));
        sigs::bind_free(home, tid);
    }
}

/// Kept so a host built against the older shape still links; the claim in
/// `gex_fork_pending` has already cleared the record.
#[no_mangle]
pub extern "C" fn gex_fork_taken() {}

/// Become the forked child, in a Worker holding a copy of the parent's memory.
/// The host copied every mapped region in, so image, stack and heap are here;
/// what is missing is a kernel saying this is `pid` and the few context words
/// that differ from the parent. Written here, not by the parent, which is still
/// using that context. Returns the pc to enter at.
#[no_mangle]
pub extern "C" fn gex_adopt_child(
    pid: i32,
    ctx_addr: i64,
    pc: i64,
    sp: i64,
    tls: i64,
    brk: i64,
) -> i64 {
    phase(1);
    // The state this Worker started from is a *copy* of the parent's, and the
    // descriptors in it are the parent's: pipe and pty ends it still holds,
    // sockets it is still using. Every one of those handles closes its end in
    // the global kernel when it drops (that is how a process leaving by a trap
    // still releases them), so letting the copy drop here closes descriptors
    // that belong to another process -- and it is charged to whichever pid this
    // Worker answers as. That is what killed a terminal's session: the shell's
    // slave end went with the copy, the master read end of file, and the whole
    // foreground group took a SIGHUP one command in. The child's own ends are
    // opened for it by `stage_fork_fds`, so the copy is leaked rather than
    // dropped -- into memory that is this process's own image anyway.
    if gex_has_host() != 0 {
        forget_copied_host();
    }
    // The slot maps the copied low region names are the parent's.
    dense::child();
    // So is the probe cache, which names the parent's table indices.
    gex_kernel::ftab::fcache_reset(guest_bytes());
    cgen::ftab_registry_reset();
    // Built from the module's `gex.meta`, as `gex_load` does: an empty
    // table would report "nothing to translate" almost at once, since `fork`
    // returns into libc. A fresh one, not the parent's copied in the image.
    install_jit(fresh_jit(&gex_xlate::Metadata::decode(&METADATA.0.borrow()).unwrap_or_default()));
    set_host(child_host(pid, ctx_addr, pc, sp, tls, brk));
    pc
}

/// A process's first JIT state, from the module's `gex.meta`, so runtime
/// translation carries on where the build stopped.
fn fresh_jit(meta: &gex_xlate::Metadata) -> Jit {
    Jit {
        described: meta.next_slot != 0,
        page_table: shared_page_table(&meta.dir_entries, meta.next_slot, meta.n_blocks),
        translated: Default::default(),
        provider: Default::default(),
        coverage_snap: Vec::new(),
        registry: Vec::new(),
        registry_ext: Vec::new(),
        store_grace: std::collections::VecDeque::new(),
        registry_sums: Vec::new(),
        registry_dense: Vec::new(),
        ever_threaded: false,
        shared: Default::default(),
        installed_mods: Default::default(),
        obj_cursor: Default::default(),
        dense_base: Default::default(),
        retired_pages: Default::default(),
        remiss: Default::default(),
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
        obj_wasm: Default::default(),
        compiles: 0,
        nanos: 0,
    }
}

/// The process state a forked child starts with: in a Worker of its own
/// holding a copy of the parent's image (`gex_adopt_child`), or for
/// posix_spawn's child in its parent's (`vfork`). Its working directory,
/// program and signal state are what was staged (`gex_set_cwd`,
/// `gex_set_child_exe`, `gex_set_child_state`).
/// The context slots up to and including the one at `ctx`: where a process
/// running on `ctx` numbers its next clone's.
fn ctx_slots_below(ctx: u64) -> u32 {
    let stride = (ctx::SIZE as u64 + 63) & !63;
    (ctx.saturating_sub(gex_abi::ctx_addr(0)) / stride) as u32 + 1
}

fn child_host(pid: i32, ctx_addr: i64, pc: i64, sp: i64, tls: i64, brk: i64) -> Host {
    let ctx_addr = ctx_addr as u64;
    {
        let mut gm = GuestMem::new(guest_bytes());
        // What `fork` returns in the child.
        let _ = gex_kernel::syscalls::set_result(&mut gm, ctx_addr, 0);
        if sp != 0 {
            // `posix_spawn` clones onto a stack it allocated; a plain `fork`
            // passes zero and keeps the parent's.
            let _ = gm.write_u64(ctx_addr + ctx::SP as u64, sp as u64);
        }
        if tls != 0 {
            // PowerPC's thread pointer is r2.
            let slot = if gex_abi::PPC { ctx::x(2) } else { ctx::TPIDR };
            let _ = gm.write_u64(ctx_addr + slot as u64, tls as u64);
        }
        // A monitor is per-thread and the child's starts clear.
        let _ = gm.write_u64(ctx_addr + ctx::MON_SIZE as u64, 0);
    }
    // A kernel of its own: the child's *memory* arrived as a copy, but its
    // kernel state is Rust data in the parent's Worker that copying guest bytes
    // does not bring over. Rebuilt here: identity, break, the memory map (via
    // `gex_adopt_vma`, once per region after this), and the inherited
    // descriptors (via `gex_adopt_fds`).
    //
    // With the parent's break, not zero: the heap's bytes came over in the
    // image, and a kernel that thinks it empty hands the next `malloc` an
    // address in the low region where the register contexts are.
    let mut kernel = Kernel::new(ctx_addr, brk as u64, String::new());
    // Who this process is: `Kernel::new` defaults to uid 1000, so a child that
    // did not copy `IDENTITY` here is not root and every forked `dpkg` refuses.
    kernel.creds.set_uid(unsafe { IDENTITY.0 });
    kernel.creds.set_gid(unsafe { IDENTITY.1 });
    kernel.mem.lock().grows_on_demand(arena_grow, arena_back);
    if nt::windows_space() != 0 {
        w3_address_space(&mut kernel.mem.lock(), nt::windows_space());
    }
    // The shared process table, so the parent's `wait4` finds a child it can no
    // longer see: the child publishes its exit into a table both Workers call
    // into, and the parent parked on exactly that.
    if let Some(procs) = global_procs() {
        kernel.procs = procs;
        // And the layout they share. Only with a global kernel: without one
        // this process is the whole system and has nobody to agree with.
        kernel.place = Some(std::sync::Arc::new(SharedPlaces));
    }
    kernel.pipes = global_pipes();
    kernel.unix_remote = global_sockets();
    kernel.ptys = global_ptys();
    kernel.tty = global_tty();
    install_console(&mut kernel);
    kernel.pid = pid;
    kernel.tid = pid;
    // Contexts above the main thread's: its context is the one the parent
    // forked from, slot N when the forking thread was not the first, and clones
    // numbered from 1 reached N and drove two threads through one context (the
    // Win7 shell's COM factory child of explorer: its fifth thread ran on the
    // main thread's registers).
    kernel.next_ctx_slot.fetch_max(ctx_slots_below(ctx_addr), std::sync::atomic::Ordering::AcqRel);
    // The process's start, unless this is a vfork child's kernel: the
    // memory's other Workers are its parent's threads (vfork.rs).
    if vfork::pid_here().is_none() {
        PROCESS_STARTED.store(now_ns(), std::sync::atomic::Ordering::Relaxed);
    }
    // Before anything opens a descriptor: the child rebuilds its fd table
    // before its first syscall, and if `MY_PID` were still unset every
    // inherited pipe end would be recorded against pid 0.
    set_my_pid(pid);
    // The shared filesystem: the parent's `TmpFs` is Rust data this Worker
    // cannot see, so without this the child's root is empty and `execve` cannot
    // find its program.
    if let Some(r) = global_root() {
        kernel.vfs.mount("/", r);
    }
    // And the per-process mounts that are not files: without them a child has
    // no `/dev/null` to redirect into and no `/proc/self/maps` for libc.
    let wayland = mount_devices(&mut kernel);
    let proc_state = mount_procfs(&mut kernel);
    // The parent's working directory, now that the root is mounted so `set_cwd`
    // can confirm the target exists; else the child resolves relative paths
    // against `/`.
    {
        let cwd = std::mem::take(&mut *CWD.0.borrow_mut());
        if !cwd.is_empty() {
            let _ = kernel.set_cwd(&cwd);
        }
    }
    // The parent's program and arguments (`gex_set_child_exe`).
    {
        let exe = std::mem::take(&mut *CHILD_EXE.0.borrow_mut());
        if !exe.is_empty() {
            let mut parts = exe.split('\0').map(str::to_string);
            kernel.exe_path = parts.next().unwrap_or_default();
            kernel.argv = parts.collect();
        }
    }
    kernel.clock = browser_clock;
    // And the clock a write stamps a file with.
    gex_kernel::vfs::tmpfs::set_clock(browser_clock);
    kernel.net = page_net().cloned();
    // The dispositions, mask, umask and limits the parent had (`do_fork`'s
    // `FORK_STATE`, staged here by the host with `gex_set_child_state`).
    {
        let state = std::mem::take(&mut *CHILD_STATE.0.borrow_mut());
        if !state.is_empty() {
            apply_state(&mut kernel, &state);
        }
    }
    Host {
        magic_head: HOST_MAGIC,
        kernel,
        wayland,
        root: Arc::new(gex_kernel::vfs::TmpFs::new()),
        proc_state,
        // The child is one thread: `fork` does not carry the parent's others
        // across, and in this design they were never in this Worker anyway.
        threads: threads_of(ThreadState::new(ctx_addr, pid), pc as u64, Waiting::Runnable),
        jit: JitHere,
        exec_waited_since: None,
        magic_tail: HOST_MAGIC,
    }
}

/// Record one of the parent's mappings in the adopted child's memory map.
/// Called once per region after `gex_adopt_child`: the bytes are copied but the
/// map is Rust state that did not travel, and an empty map `mmap`s over the
/// child's own image.
#[no_mangle]
pub extern "C" fn gex_adopt_vma(start: i64, end: i64, prot: i32) {
    // The map locks itself (no kernel lock, L25c).
    sigs::proc_home().mm().lock().insert(start as u64, end as u64, prot as u32, "[forked]");
}

/// End the running process. Always returns 2: this Worker ran one process and
/// there is nothing to hand the stack to.
///
/// `code` is an exit status, or the negated signal number for a signal death --
/// `wait4` reports the two differently (`code << 8` vs the bare signal), and
/// carrying the sign keeps them apart without widening the status path.
fn exit_process(code: i32) -> i32 {
    // One thread ends the process; any other that gets here meanwhile (a
    // second `exit_group`, the last thread's `exit`, a fault) only stops.
    // No kernel lock: what the end touches locks itself.
    if let Some(home) = sigs::home() {
        if home.exit().claim() {
            end_process_once(home, code);
        }
    } else {
        set_exited(true);
    }
    // And nowhere to resume, said explicitly: the host re-enters the guest when
    // `gex_resume_pc` is set, so a resume left over from an earlier parked call
    // would send a just-exited process back into its own code with an emptied
    // descriptor table.
    set_resume_none();
    set_resume_ctx_none();
    2
}

/// The end [`exit_process`] claimed for its caller, run once per process.
fn end_process_once(home: &sigs::Home, code: i32) {
    // Every thread marked `Exited`, because the scheduler ends a Worker only
    // when all threads are: a thread left `Runnable` gets picked again and
    // re-entered with its kernel state already released, so the guest keeps
    // making syscalls after it is gone.
    for (_, t) in home.table().iter() {
        let mut r = t.run();
        r.waiting = Waiting::Exited;
        r.blocked_on = None;
    }
    // See `gex_exited`: tells a program that finished from one that abandoned
    // the wasm stack with no status set. From here every other thread's next
    // call, and the end of its park, is its end (`free_syscall`, `finish_mine`).
    set_exited(true);
    waitq::wake_process();
    let pid = home.pid();
    // Descriptors go first and explicitly: a trap out of wasm does not unwind,
    // so nothing is dropped, and a pipe end between two Workers lives in the
    // global kernel and only learns of a close from `Drop` -- otherwise the
    // reader waits forever for an EOF nobody sends. A thread still inside a
    // call on one keeps its description until the call ends.
    if let Some(fds) = fds_free() {
        fds.close_all();
    }
    // And anything the table did not account for: a guest leaving by a trap
    // rather than `exit_group` never reaches the line above (see
    // `k_pipe_forget`).
    // SAFETY: takes a scalar and no borrow of anything here.
    unsafe {
        if k_present() != 0 {
            k_pipe_forget(pid);
            // A pty end held open (a shell exiting without closing its slave)
            // otherwise strands the master: the reader never sees EOF. And a
            // master gone this way must hang the slave up.
            let left = k_pty_forget(pid);
            if left > 0 {
                pty_note("exit-sweeps", 0, 0, pid);
            }
        }
    }
    // The status last, so a parent's `wait4` finds the descriptors closed.
    publish_exit(pid, code);
    // Nothing to hand the stack to: other processes are other Workers already
    // running, and the parent's `wait4` finds the status in the shared table.
    if tracing() {
        trace_line(&format!("[proc] exit {pid} with {code}\n"));
    }
    // A shell reports a signal death as 128 + the signal, and so does this.
    set_exit(if code < 0 { 128 - code } else { code });
}

/// `execve`: replace this process's image with another program's. Everything
/// not the image survives -- pid, parent, non-cloexec descriptors, cwd -- which
/// is what makes fork-then-exec work. Returns the entry point, or zero if the
/// program could not be loaded (the caller already has an errno).
thread_local! {
    /// The exec about to run is on a memory no program has run in yet (a
    /// posix_spawn child's, `vfork::gex_adopted_exec`): the image's and the
    /// stack's spans read as zero already, so `do_exec` does not scan them.
    /// Taken by the next `do_exec`.
    pub(crate) static EXEC_FRESH: Cell<bool> = const { Cell::new(false) };
}

/// One of the few lines gex says by default (shims/browser/js/log.mjs): a
/// process starting, with the file it runs and its arguments.
fn note_exec(path: &str, argv: &[String]) {
    let mut args: String = argv.iter().skip(1).map(|a| format!(" {a}")).collect();
    if args.len() > 200 {
        let mut cut = 200;
        while !args.is_char_boundary(cut) {
            cut -= 1;
        }
        args.truncate(cut);
        args.push_str("...");
    }
    let pid = MY_PID.load(std::sync::atomic::Ordering::Relaxed);
    warn_line(&format!("[gex-info] pid {pid}: exec {path}{args}\n"));
}

fn do_exec(h: &mut Host, path: &str, argv: &[String], envp: &[String]) -> u64 {
    let fresh = EXEC_FRESH.with(|c| c.replace(false));
    {
        let size = warch::memory_size(0) as u64 * 65536;
        w3_note_reach(size, "an execve, which keeps the memory", 0, size);
    }
    // A sibling's translation in flight is of the program this replaces.
    jit_in_held(&mut h.jit);
    jitout::EXEC_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Release);
    // Bumped so the host can drop what this exec makes worthless: a forked
    // child holds its parent's inherited modules (thousands in an `apt` run),
    // and the moment it execs every one describes a program that is gone.
    EXEC_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Has this program been linked already, in this run, in this
    // environment? Then there is nothing to load: the answer is an address
    // space, and it goes back byte for byte.
    let key = snap_key(path, envp);
    let linked = snap_lookup(key);
    // Headers only: each segment is read from its file straight into place
    // below, once the old image is gone. Reading the whole executable and its
    // interpreter into the heap, parsing their symbols and dynamic tables and
    // then copying every segment again was most of an exec's own cost.
    let (program, ranges) = if linked.is_some() {
        (None, Vec::new())
    } else {
        let read_at = |p: &str, off: u64, out: &mut [u8]| h.kernel.read_file_at(p, off, out);
        match gex_ld::plan_program(path, gex_abi::PIE_BASE, INTERP_BASE, &read_at) {
            Ok((p, r)) => (Some(p), r),
            Err(_) => return 0,
        }
    };
    note_exec(path, argv);
    // Record, when this is the first process to run the program and there is
    // a linker to catch.
    let hook = if program.as_ref().and_then(|p| p.interp.as_ref()).is_some() && snapshots_on() {
        SNAP_STUB
    } else {
        0
    };
    h.kernel.close_on_exec();
    // Handlers are addresses in the image being replaced; see Signals::exec.
    let leader = h.kernel.pid;
    h.kernel.exec_signals(leader);
    // The old image goes, and not only from the memory map. `execve` reuses
    // the process's existing memory rather than getting a fresh address space,
    // so anything not deliberately cleared stays, and the next program maps its
    // libraries at the same addresses -- finding the old program's *relocated*
    // copy, whose `.got.plt` sends a call through the PLT into unmapped memory.
    let old: Vec<(u64, u64)> = h.kernel.mem.lock().vmas().iter().map(|v| (v.start, v.end)).collect();
    let old_code: Vec<(u64, u64)> = h
        .kernel
        .mem
        .lock()
        .vmas()
        .iter()
        .filter(|v| v.prot & gex_kernel::mem::prot::EXEC != 0)
        .map(|v| (v.start, v.end))
        .collect();
    // The dead arena the new program may reuse: the old map's own spans, and
    // in a forked child everything it inherited below `ADOPTED_TOP` (its shim
    // never allocates there; see `arena_take`) -- less anything the runtime
    // owns, by the memory map's own record.
    let mut dead: Vec<(u64, u64)> = h.kernel.mem.lock().arena_spans().to_vec();
    let mut inherited = ADOPTED_TOP.load(std::sync::atomic::Ordering::Acquire);
    // W3's compact space: the shim's heap does allocate there, between the
    // inherited regions, and above its cursor is what it will take next --
    // not dead, whatever was inherited above it (KUSER_SHARED_DATA at 2 GiB).
    // Below the cursor everything is claimed, so `guest_only` sorts it.
    if nt::w3_compact() {
        inherited = inherited.min(ARENA_NEXT.load(std::sync::atomic::Ordering::Acquire));
    }
    if inherited > gex_abi::ARENA_BOTTOM {
        dead.push((gex_abi::ARENA_BOTTOM, inherited));
    }
    let dead: Option<Vec<(u64, u64)>> = dead
        .iter()
        .map(|&(lo, hi)| memmap::guest_only(lo.max(gex_abi::ARENA_BOTTOM), hi.min(gex_abi::MMAP_TOP)))
        .collect::<Option<Vec<_>>>()
        .map(|v| v.into_iter().flatten().collect());
    h.kernel.mem.lock().renew(program.as_ref().map(|p| p.exe.brk).unwrap_or(0));
    // A fresh map for a fresh image, and it grows the same way.
    h.kernel.mem.lock().grows_on_demand(arena_grow, arena_back);
    // Old code the new program may put something else at: what lies in the
    // donated arena. Code elsewhere (ld.so at its fixed base) can only come
    // back as the same object at the same place, and keeps its translations
    // as it did before exec reused anything -- retiring it made the next
    // fresh process lift ld.so block by block (33k blocks, nofill 31k).
    let reused: Vec<(u64, u64)> = match &dead {
        Some(dead) => old_code
            .iter()
            .flat_map(|&(lo, hi)| {
                dead.iter().filter_map(move |&(a, b)| {
                    let (s, e) = (lo.max(a), hi.min(b));
                    (s < e).then_some((s, e))
                })
            })
            .collect(),
        None => Vec::new(),
    };
    if let Some(dead) = dead {
        h.kernel.mem.lock().donate_arena(&dead);
    }
    // Everything keyed by a code address there forgets it.
    cgen::on_exec(&reused);
    fjit::on_retire(&reused);
    {
        // The live memory size, not `guest_bytes().len()` (which is the whole
        // 4 GiB nameable range): the fill must stay inside what actually exists.
        let live = warch::memory_size(0) as usize * 65536;
        let data = guest_bytes();
        for (start, end) in old {
            // Only the fixed low layout is reused at the same address by the next
            // program -- its image at `PIE_BASE`, `ld.so` at `INTERP_BASE`, the
            // stack, and the placement window -- so only that must be cleared.
            // The mmap arena is never re-handed-out at an old address: a fresh
            // process's `arena_take` starts above the memory's high-water mark,
            // so nothing new lands on the old arena and its stale bytes are
            // unreachable. Skipping it is also what makes exec from a large
            // parent viable: Xvfb's `xkbcomp` helper inherits ~130 MiB of arena
            // (a 107 MiB libLLVM span among it), and zeroing those never-reused
            // pages stalled the exec indefinitely -- a bulk fill over this shared
            // memory commits every page and crawls, where the live low layout
            // clears in a blink. This was the long-standing glxgears/virgl "hang".
            if start >= gex_abi::ARENA_BOTTOM {
                continue;
            }
            let (a, b) = (start as usize, (end as usize).min(live).min(data.len()));
            if a < b {
                // Only the pages the old program wrote: a mapping it reserved
                // and never touched reads as zero already, and filling it
                // would commit all of it (the mspaint demo's Wine processes
                // carried ~80 MiB each of exactly that).
                gex_kernel::zero_dirty_bytes(&mut data[a..b]);
            }
        }
    }
    // The probe cache names the old program's table indices.
    gex_kernel::ftab::fcache_reset(guest_bytes());
    cgen::ftab_registry_reset();
    h.kernel.exe_path = path.to_string();
    h.kernel.argv = argv.to_vec();
    h.kernel.envp = envp.to_vec();
    // The program the compositor's pump asks about (`wl::ProcCx`), and the
    // image `/proc` shows.
    wl::publish_process(&h.kernel);
    h.kernel.publish_proc(&h.proc_state.0);
    if let Some(p) = program.as_ref() {
        h.kernel.compat = p.compat();
        gex_xlate::set_compat(p.compat());
    }
    // An exec keeps the process's start (its processor time goes on).
    if h.kernel.started == 0 {
        h.kernel.started = now_ns() as u128;
    }
    clock_constants(h.kernel.compat, h.kernel.started);
    // Set again when a wasm32 program is published (cgen/wasmprog.rs).
    h.kernel.wasm32 = false;
    WASM32.store(false, std::sync::atomic::Ordering::Release);
    cgen::wasmprog::stage(program.as_ref());
    h.kernel.live_threads.store(1, std::sync::atomic::Ordering::Release);
    // Clones above the context this program goes on, which is the exec'ing
    // thread's: not slot 0 after an exec from a second thread.
    let here = if ctx_addr_get() != 0 { ctx_addr_get() } else { h.kernel.ctx_addr };
    h.kernel.next_ctx_slot.store(ctx_slots_below(here).max(1), std::sync::atomic::Ordering::Release);

    let data = guest_bytes();
    // The new image goes where its own headers say, which for a static
    // executable is the same place the old one was.
    for ((seg, from), &(off, len)) in program.iter().flat_map(|p| p.placements()).zip(&ranges) {
        let (a, b) = (seg.vaddr as usize, (seg.vaddr + seg.mem_size) as usize);
        let len = len as usize;
        if b > data.len() || a + len > data.len() {
            return 0;
        }
        if !fresh {
            gex_kernel::zero_dirty_bytes(&mut data[a..b]);
        }
        // From the file into place: the one copy.
        if h.kernel.read_file_at(from.unwrap_or(path), off, &mut data[a..a + len]) != Some(len) {
            return 0;
        }
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
        // No origin: an image segment is *built* by the loader, not copied from
        // a file (`mem_size` runs past the file bytes, tail zeroed), so it
        // cannot be re-read and a snapshot carries its bytes. It does get a code
        // identity -- this program's segment at this address -- which lets two
        // processes share a translation instead of each re-lifting its own text.
        h.kernel.mem.lock().insert_loaded(
            seg.vaddr,
            seg.vaddr + seg.mem_size,
            prot,
            "[image]",
            Some((from.unwrap_or(path).to_string(), seg.vaddr)),
        );
    }

    // A wasm32 program is published before it runs: its magic, read where
    // it now is.
    cgen::wasmprog::stage_entry(program.as_ref().map(|p| p.exe.entry).filter(|&entry| {
        let at = entry.wrapping_sub(0x40) as usize;
        data.get(at..at + 8) == Some(&cgen::wasmprog::MAGIC[..])
            && program.iter().flat_map(|p| p.exe.segments.iter()).any(|s| {
                (at as u64) >= s.vaddr && (at as u64) + 8 <= s.vaddr + s.mem_size
            })
    }));
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
    let mut facts = match (&program, &restored) {
        (Some(p), _) => gex_ld::elf::headers_only(
            p.exe.phdr_vaddr,
            p.exe.phentsize,
            p.exe.phnum,
            p.exe.entry,
        ),
        (None, Some(l)) => gex_ld::elf::headers_only(l.phdr, l.phent, l.phnum, l.entry),
        (None, None) => return 0,
    };
    // A 32-bit program's stack is laid out with 4-byte words; the recorded
    // (closure) images are 64-bit ones.
    if let Some(p) = &program {
        facts.class32 = p.exe.class32;
    }
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
        // `STACK_TOP`, not the top of the addressable range: a stack at the
        // top makes the memory allocate up to it, four gigabytes per process.
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
    // Zero first, then lay the new stack on top: `execve` gives a fresh stack,
    // which on Linux reads as zero. Writing only the layout left everything
    // below it as the previous image had, so a program reading an uninitialised
    // local read the last program's data instead of zero.
    let stack_lo = (gex_abi::STACK_TOP - gex_abi::STACK_SIZE) as usize;
    // (A fresh memory's is zero: scanning its 8 MiB to find that out was
    // half a millisecond of every posix_spawn child's exec.)
    if !fresh {
        gex_kernel::zero_dirty_bytes(&mut data[stack_lo..gex_abi::STACK_TOP as usize]);
    }
    let start = layout.low as usize;
    data[start..start + layout.bytes.len()].copy_from_slice(&layout.bytes);
    h.kernel.mem.lock().insert(
        gex_abi::STACK_TOP - gex_abi::STACK_SIZE,
        gex_abi::STACK_TOP,
        gex_kernel::mem::prot::READ | gex_kernel::mem::prot::WRITE,
        "[stack]",
    );

    // The new program has nothing translated, and the old directory points at
    // other code at the same addresses. Clearing it sends every branch back
    // through the miss stub for a fresh translation.
    if DIR_WATCH.load(std::sync::atomic::Ordering::Relaxed) >= 0 {
        warn_line(&format!("[gex] pid {} dir cleared by exec\n", my_pid()));
    }
    gex_kernel::zero_dirty_bytes(&mut data[gex_abi::DISPATCH_DIR as usize..LOW_END as usize]);
    // And forget which spans were asked for: `FILL_ASKED` avoids re-asking a
    // span, which is only safe while the fill's answer is reachable -- and
    // clearing the directory above made it unreachable, so a stale mark would
    // strand every block on the page on the translator for the process's life.
    {
        let mut f = FILL_ASKED.lock();
        f.asked.clear();
        f.failed.clear();
    }
    SERVED_PAGE.0.borrow_mut().clear();
    // And the bases blocks were installed under: the page table starts over
    // below, and a page of the new program has nothing installed yet.
    LIVE_BASE.0.borrow_mut().clear();
    // And the modules inherited from the parent: they cover the PARENT's pages.
    // The new program's image sits at the same PIE base, so a page number the
    // parent's image had is now another program's code; a fill answered from
    // the inherited module would run the old program's block there (x86 qt6ct
    // exec'd from dash ran dash's `_init` in qt6ct's address space).
    if gex_abi::X86 {
        LAZY_OWNER.0.borrow_mut().clear();
    }
    // Slots already handed out are never reused, so allocation carries on from
    // the high-water mark. No entries, so `usize::MAX` ("nothing with active
    // segments to place") is true whatever the new program turns out to be.
    h.jit.page_table = shared_page_table(&[], h.jit.page_table.slots(), usize::MAX);
    regview::publish_marks(&h.jit.page_table);
    h.jit.translated.clear();
    // And whatever was queued for the host to install: a fork copies the
    // parent's queue, and a module of the OLD program installed after this
    // point publishes its pages -- the old program's bases for the new
    // program's addresses (`install-done` wrote dash's base for qt6ct's
    // `_init` page; the block there was dash's).
    if gex_abi::X86 {
        h.jit.pending.clear();
        publish_pending(&h.jit);
    }
    // Except the closure's: those modules are still in the dispatch tables
    // (`execve` rebuilds the image, not the wasm instance), so the new program
    // gets every library the build covered for nothing. See `CLOSURE_DIRS`.
    // Trace what this process holds, to diagnose an exec'd program re-lifting.
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
    // Only re-point the directory at closure slots this Worker actually holds.
    // `installed_mods` is the shared modules in *this* Worker's tables, and it
    // is what "the closure's modules are still in the dispatch tables after
    // execve" (above) really means. A forked child gets fresh, empty tables --
    // fork does not carry the parent's installed shared modules -- so once it
    // execs, every CLOSURE_DIRS entry names a table slot it never installed;
    // writing them anyway sends the next indirect branch through an absent slot
    // and traps ("table index is out of bounds", 0 modules installed -- broke
    // debian-deb/lzma/twice/io, all dpkg-deb/xz exec'd from a forked shell).
    // With nothing installed the whole closure directory is bogus for this
    // process, so leave those pages at the miss stub to translate on demand.
    let have_mods = !h.jit.installed_mods.is_empty();
    let closure = CLOSURE_DIRS.0.borrow();
    if !closure.is_empty() && have_mods {
        let mut kept = 0usize;
        for (page, base) in closure.iter() {
            // Not the image's pages: a closure library is pinned to its own
            // address and covers the same code, but the image sits at
            // `PIE_BASE` and this call replaces it. Since `--shared-image` the
            // image arrives like any library, so its entries would otherwise
            // survive and point the new program at the old program's blocks.
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
    // And the store modules this Worker installed before the exec: they are
    // still in its tables, but nothing points at them now -- the page table
    // and directory start over above -- so counting them installed made the
    // serve path refuse every one of them, and the new program lifted what
    // it could have been served. A program exec'd from a fork of node (the
    // child had taken libc and libnode whole before its exec) lifted 66,079
    // blocks, against 4,188 for a fresh `node -e 0`. Served again, a module
    // writes the same blocks into the same slots, and its compile is shared
    // by content key.
    h.jit.installed_mods.clear();

    let ctx_addr = h.threads[cur()].ctx_addr();
    let c = ctx_addr as usize;
    // A loaded program starts with clear registers; a restored one keeps the
    // linker's (`x0` is `_dl_fini` for `atexit`, and the thread pointer names
    // the TLS block the linker allocated).
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
    let compat = program.as_ref().is_some_and(|p| p.compat());
    write_u64(data, c + ctx::sp_slot(compat), layout.sp);
    let entry = match (&program, &restored) {
        (Some(p), _) => p.entry(),
        (None, Some(l)) => l.entry,
        (None, None) => return 0,
    };
    write_u64(data, c + ctx::PC, entry);
    write_u64(data, c + ctx::MON_SIZE, 0);
    data[c + ctx::SAFEPOINT_COUNTER..c + ctx::SAFEPOINT_COUNTER + 4]
        .copy_from_slice(&1024i32.to_le_bytes());
    // Every other thread goes with the old program (Linux's de_thread): its
    // Worker, parked or running, sees `gone` and ends, without the process.
    let me = cur();
    let others = h.threads.len() > 1;
    for (i, t) in h.threads.iter() {
        if i != me {
            t.gone().store(true, std::sync::atomic::Ordering::Release);
            let mut r = t.run();
            r.waiting = Waiting::Exited;
            r.blocked_on = None;
        }
    }
    h.threads.reset(h.kernel.pid, ctx_addr, ThreadCell::new(Run::new(entry, Waiting::Runnable)));
    set_current(h, 0);
    if others {
        waitq::wake_process();
    }
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

/// How long an `execve` waits between looks at its other threads, and how
/// long it waits for them in all before it replaces the image anyway.
const EXEC_WAIT_STEP_NS: u64 = 1_000_000;
const EXEC_WAIT_MAX_NS: u64 = 2_000_000_000;

/// Whether an `execve` from a threaded process has to wait before it
/// replaces the image (Linux's de_thread): each other thread is told to go
/// (`gone`); one parked in a call is exited where it parked, and woken, and
/// ends there without running guest code again; one not yet in its Worker
/// will not be adopted. One that may be running guest code is waited for:
/// it stops at its next safepoint, dispatch miss or call ([`stop_gone`]),
/// before the image under it changes. A thread that never gets there (lost
/// in a host call) is given up on after `EXEC_WAIT_MAX_NS`.
fn exec_waits(h: &mut Host) -> bool {
    if h.threads.len() < 2 {
        return false;
    }
    let me = cur();
    let mut running = false;
    let mut woke = false;
    for (i, t) in h.threads.iter() {
        if i == me {
            continue;
        }
        t.gone().store(true, std::sync::atomic::Ordering::SeqCst);
        let mut r = t.run();
        match r.waiting {
            Waiting::Exited => {}
            // Running, as far as anyone can tell: its Worker says when not.
            Waiting::Runnable => running |= !t.halted().load(std::sync::atomic::Ordering::SeqCst),
            // Not adopted yet (`gex_adopt_thread` refuses it now), or parked.
            _ => {
                woke |= !matches!(r.waiting, Waiting::Elsewhere);
                r.waiting = Waiting::Exited;
                r.blocked_on = None;
            }
        }
    }
    if woke {
        waitq::wake_process();
    }
    if !running {
        return false;
    }
    let now = turn_now();
    let since = *h.exec_waited_since.get_or_insert(now);
    if now.saturating_sub(since) >= EXEC_WAIT_MAX_NS {
        warn_line(&format!(
            "[gex] pid {}: execve went ahead after {} ms with a thread still running",
            h.kernel.pid,
            (now - since) / 1_000_000
        ));
        return false;
    }
    true
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
fn shared_slot(j: &Jit, page: u64, key: u64) -> Option<u32> {
    // SAFETY: a plain lookup in the kernel's own memory, taking no borrow of
    // anything this module owns.
    if unsafe { k_present() } != 0 {
        let base = unsafe { k_slot_get(page, key) };
        if base != 0 {
            return Some(base);
        }
    }
    j.shared.get(&(page, key)).copied()
}

/// Record it in both, first writer winning in each.
fn record_shared_slot(j: &mut Jit, pid: i32, page: u64, key: u64, base: u32) {
    j.shared.entry((page, key)).or_insert(base);
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
                pid,
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

/// `movz x8, #0x7801` / `movk x8, #0x6765, lsl #16` / `svc #0`, or on
/// x86-64 `mov eax, 0x67657801` / `syscall`.
///
/// No registers touched but the syscall-number one, which is caller-saved
/// and which `_start` does not read -- so `x0`/`rdx`, holding the address of
/// `_dl_fini` that glibc's `__libc_start_main` will register with `atexit`,
/// arrives at the entry exactly as the linker left it. (On x86-64 `_start`
/// takes `_dl_fini` in `rdx`; `rax` is free.)
const SNAP_STUB_CODE: &[u8] = if gex_abi::X86 {
    &[0xb8, 0x01, 0x78, 0x65, 0x67, 0x0f, 0x05]
} else if gex_abi::PPC {
    // lis 0, 0x6765 ; ori 0, 0, 0x7801 ; sc
    &[0x3c, 0x00, 0x67, 0x65, 0x60, 0x00, 0x78, 0x01, 0x44, 0x00, 0x00, 0x02]
} else {
    &[0x28, 0x00, 0x8f, 0xd2, 0xa8, 0xec, 0xac, 0xf2, 0x01, 0x00, 0x00, 0xd4]
};

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

/// How many times this process has `execve`d, so the host can tell.
///
/// A counter rather than a flag: the host reads it after every trap and
/// compares, so it cannot miss two execs between reads and it needs no
/// clearing. See `do_exec` and task 145.
static EXEC_GEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// How many `execve`s this process has done.
#[no_mangle]
pub extern "C" fn gex_exec_gen() -> u32 {
    EXEC_GEN.load(std::sync::atomic::Ordering::Relaxed)
}

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
/// The program and its whole environment: `LD_LIBRARY_PATH`/`LD_PRELOAD` change
/// which objects load where, so a snapshot restored under a different
/// environment would be another program's libraries at these addresses.
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
    data[at..at + SNAP_STUB_CODE.len()].copy_from_slice(SNAP_STUB_CODE);
    kernel.mem.lock().insert_loaded(
        SNAP_STUB,
        SNAP_STUB + 4096,
        gex_kernel::mem::prot::READ | gex_kernel::mem::prot::EXEC,
        "[gex-link]",
        None,
    );
    *SNAP_PENDING.0.borrow_mut() = (key, entry);
    link_state(2);
}

/// A length-prefixed string in the snapshot blob.
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

/// Serialise this process's address space, as the dynamic linker left it. Only
/// what the linker *wrote* is carried (the writable mappings -- GOTs, relocated
/// `.data`, TLS template); read-only text is stored as a path to re-read.
fn snap_take(h: &mut Host, ctx_addr: u64, entry: u64) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"SNP1");
    out.extend_from_slice(&entry.to_le_bytes());
    let (phdr, phent, phnum, interp_base) = *SNAP_FACTS.0.borrow();
    for v in [phdr, phent, phnum, interp_base] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&h.kernel.mem.lock().brk_current().to_le_bytes());
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
        .lock()
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
                // Re-readable only if never writable (the `origin` invariant,
                // re-checked here): re-reading a page the linker wrote gives a
                // restored process an unrelocated GOT.
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
                // By the page, with a bitmap of which hold anything: most of a
                // linked space is zero (`.bss`, break tail, untouched TLS), so
                // carrying only non-zero pages is a fraction of the size.
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

/// Put a recorded address space back, and answer with the entry to resume at.
/// Zero (via `None`) if the blob cannot be read or a file it names has gone, so
/// the caller loads the program the long way.
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

    h.kernel.mem.lock().renew(brk);
    h.kernel.mem.lock().grows_on_demand(arena_grow, arena_back);
    // One read per file, not per mapping: a library is four mappings (text,
    // rodata, relro, data), and re-reading `libc.so.6` for each is wasteful.
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
        // The memory must reach the mapping before anything is written into it:
        // a restored process starts with only the fixed low layout.
        arena_back(start, end)?;
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
            gex_kernel::zero_dirty_bytes(&mut data[a..b]);
            let take = (b - a).min(bytes.len().saturating_sub(off as usize));
            data[a..a + take].copy_from_slice(&bytes[off as usize..off as usize + take]);
            h.kernel.mem.lock().insert_from(start, end, prot, &name, Some((path, off)));
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
            // Zero first: a page absent from the bitmap was zero in the
            // recording, and this memory is not necessarily untouched -- an
            // `execve` runs in the Worker that had the previous program. Only
            // the pages that hold anything, though: a large reservation
            // restored with a fill came back fully resident.
            gex_kernel::zero_dirty_bytes(&mut data[a..b]);
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
            h.kernel.mem.lock().adopt(start, end, prot, &name, code);
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
        let got = k_snap_get(key, blob.as_mut_ptr() as gex_kernel::PAddr, len);
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

/// Most bytes of shared module one dispatch miss will read into this process.
///
/// A miss serves whatever somebody else already translated of the object it
/// landed in, which is strictly cheaper than translating it again -- but the
/// bytes are read into the shim's heap, and the arena those spans come from
/// never takes one back. Unbounded, one miss read every module of the object:
/// hundreds of megabytes, per process, to install sixteen of them.
///
/// Sized to cover an ordinary library whole in one miss (glibc recompiles to
/// 13.6 MiB) while refusing the pathological case, and nothing is lost by
/// refusing -- what a miss leaves behind, the next one takes.
///
/// This is the process's memory high-water, near enough: the batch is read in
/// and then merged, so both exist at once, and neither span is ever given back.
/// At 64 MiB an `apt-get install` served 47 MiB batches and every process in
/// the tab carried ~90 MiB of heap it was not using; at 16 the same run carries
/// a quarter of that. Compiling is per *module* far more than per byte, so what
/// this costs is one more miss, not one more compile per byte.
const SERVE_MAX_BYTES: u64 = 16 << 20;


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
            wasm.as_ptr() as gex_kernel::PAddr,
            wasm.len() as u32,
            slots,
            raw.as_ptr() as gex_kernel::PAddr,
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
/// The coarse half of [`page_identity`]: the file alone, not the address. The
/// grain a process installs at -- having missed once in libc it will miss all
/// over libc, each a module somebody already built.
fn object_identity(p: &Px<'_>, addr: u64) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let mm = p.mm.lock();
    let v = mm.vmas().iter().find(|v| addr >= v.start && addr < v.end)?;
    if v.prot & gex_kernel::mem::prot::WRITE != 0 {
        return None;
    }
    let (id, _) = v.code.as_ref()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    let key = hasher.finish();
    note_key(p, key, id);
    Some(key)
}

/// Whether the object covering this page has been translated whole.
fn obj_finished(p: &Px<'_>, page: u64) -> bool {
    // SAFETY: a scalar in, a scalar out, and no borrow of anything this
    // module owns.
    unsafe {
        if k_present() == 0 {
            return false;
        }
        match object_identity(p, page * gex_abi::DISPATCH_PAGE) {
            Some(obj) => k_obj_is_translated(obj) != 0,
            None => false,
        }
    }
}

/// Every module any process stored for this object that this Worker lacks:
/// `(id, slots, dirs, wasm)` each.
fn shared_modules_of_object(
    j: &Jit,
    obj: u64,
    max_mods: usize,
    max_bytes: u64,
) -> (Vec<(u32, u64, Vec<(u64, u32)>, Vec<u8>)>, u32) {
    phase(13);
    let mut out = Vec::new();
    let mut took = 0u64;
    let mut left_behind = 0usize;
    let mut left_bytes = 0u64;
    let from = j.obj_cursor.get(&obj).copied().unwrap_or(0);
    let mut lead = 0u32;
    // SAFETY: scalars in, and the readers below size their buffers from the
    // kernel's own answer before asking it to fill them.
    unsafe {
        if k_present() == 0 {
            return (out, 0);
        }
        // The ids from the first one not known installed, in one call.
        let mut ids: Vec<u32> = Vec::new();
        loop {
            let mut chunk = vec![0u32; 4096];
            let got = k_mod_ids_for_object(obj, from + ids.len() as u32, chunk.as_mut_ptr() as gex_kernel::PAddr, 4096);
            ids.extend_from_slice(&chunk[..got as usize]);
            if got < 4096 {
                break;
            }
        }
        let mut leading = true;
        for &id in &ids {
            if j.installed_mods.contains(&id) {
                if leading {
                    lead += 1;
                }
                continue;
            }
            leading = false;
            // Full: the rest are the next miss's (only the trace below
            // counts them).
            if out.len() >= max_mods && !tracing() {
                break;
            }
            if out.iter().any(|(i, ..)| *i == id) {
                continue;
            }
            let len = k_mod_len(id) as usize;
            if len == 0 {
                continue;
            }
            // Bounded here, where the bytes are read, not by the caller after
            // the fact.
            //
            // The caller took `per_miss` of these and dropped the rest, so this
            // read every module of the object -- every time any process missed
            // in it -- to keep sixteen. In an `apt-get install` that was 31 GB
            // of module bytes read into per-process heaps across the run, at
            // 19, 39, 79, 158 and 317 MiB a time, and the arena those spans come
            // from never takes one back: a forked child arrived holding 629 MiB
            // of heap before it had run a single guest instruction, the tab
            // peaked at 11 GB, and apt's `http` method died of an allocation
            // failure that reads as "Sub-process http received signal 9".
            //
            // Always at least one, whatever its size, or a module bigger than
            // the whole budget could never be taken and the miss would repeat
            // for ever. Everything skipped is taken by a later miss -- the ones
            // taken are marked installed, so each miss sees the next batch.
            if !out.is_empty() && (out.len() >= max_mods || took + len as u64 > max_bytes) {
                left_behind += 1;
                left_bytes += len as u64;
                continue;
            }
            took += len as u64;
            let mut wasm = vec![0u8; len];
            if k_mod_read(id, wasm.as_mut_ptr() as gex_kernel::PAddr, len as u32) as usize != len {
                continue;
            }
            let dlen = k_mod_dirs_len(id) as usize;
            let mut raw = vec![0u8; dlen];
            if dlen > 0 && k_mod_dirs_read(id, raw.as_mut_ptr() as gex_kernel::PAddr, dlen as u32) as usize != dlen
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
    // No silent cap: a run that stops taking modules gets slower for a reason
    // that is invisible otherwise.
    if left_behind > 0 && tracing() {
        trace_line(&format!(
            "[gex] took {} module(s) of {} KiB for the object, left {left_behind} of \
             {} KiB for the next miss\n",
            out.len(),
            took / 1024,
            left_bytes / 1024,
        ));
    }
    (out, from + lead)
}

/// Fill the registry with every module this process has already installed, so
/// a thread that joins next can `#catchUp` on the code the main Worker built
/// *before* the first `clone`.
///
/// The registry is what a joining Worker replays into its own tables (its
/// tables are per-instance; only the dispatch *directory* is shared). Until a
/// process threads, nothing is kept -- a single-threaded `apt` would otherwise
/// hold ~1,640 modules against a fixed heap (#140) -- so `gex_jit_installed`
/// gates the keep on `ever_threaded`. That gate means every module lifted
/// before the first thread spawned is missing from the registry: the main
/// Worker installed it in *its* tables and wrote the shared directory to point
/// there, but a joining Worker replays an empty registry, leaves its tables
/// bare, and the first branch into a pre-spawn page (musl/pthread startup sits
/// in the low image pages every thread runs) reads the populated directory and
/// traps on an empty slot -- "table index out of bounds, 0 modules installed".
///
/// Called once, at the `false -> true` edge of `ever_threaded`: the bytes are
/// still in the shared store (`share_module` put them there for any sibling to
/// install), so we read each installed id back and keep it. Duplicates are
/// impossible because the registry is empty until this runs -- the keep was
/// gated off -- and every later module is a fresh id pushed by
/// `gex_jit_installed`.
fn backfill_registry(j: &mut Jit) {
    if unsafe { k_present() } == 0 {
        return;
    }
    let ids: Vec<u32> = j.installed_mods.iter().copied().collect();
    for id in ids {
        // SAFETY: `k_mod_len` sizes the buffer the `k_mod_read` below fills;
        // the kernel refuses rather than overruns when the two disagree.
        let len = unsafe { k_mod_len(id) } as usize;
        if len == 0 {
            continue;
        }
        let mut wasm = vec![0u8; len];
        if unsafe { k_mod_read(id, wasm.as_mut_ptr() as gex_kernel::PAddr, len as u32) } as usize != len {
            continue;
        }
        // As this process installed it: at its dense base, or not at all.
        let mut range = (0, 0);
        if dense::on() {
            if let Some(at) = gex_emit::dense::locate(&wasm) {
                let Some(&base) = j.dense_base.get(&id) else { continue };
                gex_emit::dense::set_base(&mut wasm, &at, base);
                range = (base, gex_emit::dense::section(&wasm, &at).map_or(0, |s| s.entries.len() as u32));
            }
        }
        j.registry_dense.push(range);
        wasm.shrink_to_fit();
        #[cfg(feature = "shared-memory")]
        REGISTRY_BYTES.fetch_add(wasm.len() as u64, std::sync::atomic::Ordering::Relaxed);
        j.registry_sums.push(chunk_sums(&wasm));
        regview::publish_module(j.registry.len(), wasm.as_ptr() as u32, wasm.len() as u32, None);
        j.registry.push(wasm);
        j.registry_ext.push(None);
    }
}

/// A hash of each 256-byte chunk of `b`.
fn chunk_sums(b: &[u8]) -> Vec<u64> {
    b.chunks(256)
        .map(|c| {
            let mut h = 0xcbf2_9ce4_8422_2325u64;
            for &x in c {
                h = (h ^ x as u64).wrapping_mul(0x100_0000_01b3);
            }
            h
        })
        .collect()
}

/// The first registry module whose bytes changed since they were pushed:
/// its index, address, the changed chunk's offset and its bytes now. Each
/// module is reported once.
pub(crate) fn verify_registry(j: &mut Jit) -> Option<String> {
    for i in 0..j.registry.len() {
        let now = chunk_sums(&j.registry[i]);
        if let Some(c) = now.iter().zip(&j.registry_sums[i]).position(|(a, b)| a != b) {
            let m = &j.registry[i];
            let at = c * 256;
            let bytes: Vec<String> = m[at..(at + 256).min(m.len())].iter().map(|b| format!("{b:02x}")).collect();
            let s = format!(
                "module {i} at {:#x}+{:#x} (of {} modules): chunk at +{at:#x} (address {:#x}) changed; now {}",
                m.as_ptr() as usize,
                m.len(),
                j.registry.len(),
                m.as_ptr() as usize + at,
                bytes.join(" ")
            );
            j.registry_sums[i] = now;
            return Some(s);
        }
    }
    None
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
        // The first module covering this page that this Worker has not got: a
        // page accumulates one claim per translation unit, and taking only the
        // first left the rest of the page's blocks to be lifted again.
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
        if k_mod_read(id, wasm.as_mut_ptr() as gex_kernel::PAddr, len as u32) as usize != len {
            return None;
        }
        let dlen = k_mod_dirs_len(id) as usize;
        let mut raw = vec![0u8; dlen];
        if dlen > 0 && k_mod_dirs_read(id, raw.as_mut_ptr() as gex_kernel::PAddr, dlen as u32) as usize != dlen {
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

// `?xcache=`: tie each identity key to its file for the kernel's saved store
// (docs/gexcc.md §8).
thread_local! {
    /// Keys this Worker has told the kernel about, each path's size and mtime
    /// (`u64::MAX`: no such file here), and whether this load keeps a cache
    /// (`k_xcache_on`).
    static XKEYS: std::cell::RefCell<(std::collections::HashSet<u64>, std::collections::HashMap<String, (u64, i64)>, Option<bool>)> =
        std::cell::RefCell::new(Default::default());
}

/// Tell the kernel which file `key` was computed from, once per key per
/// Worker, in every process of a load that keeps a cache (`?xcache=`,
/// whatever the process's own options: a forked child's save must name its
/// keys' files too): for the save, and to check a restored key against its
/// file before this process uses it.
fn note_key(p: &Px<'_>, key: u64, path: &str) {
    if unsafe { k_present() } == 0 {
        return;
    }
    XKEYS.with(|x| {
        let mut x = x.borrow_mut();
        // Asked once per Worker: the host enables it before pid 1 runs.
        if !*x.2.get_or_insert_with(|| unsafe { k_xcache_on() } != 0) {
            return;
        }
        if !x.0.insert(key) {
            return;
        }
        let (size, mtime) = *x.1.entry(path.to_string()).or_insert_with(|| match p.ns.metadata(path) {
            Ok(m) => (m.size, m.mtime),
            Err(_) => (u64::MAX, 0),
        });
        if unsafe { k_key_path(key, path.as_ptr() as gex_kernel::PAddr, path.len() as u32, size, mtime) } < 0 {
            warn_line(&format!("[gex] xcache: {path} changed since the save; its translations are dropped\n"));
        }
    });
}

/// The saved store a load's host put in the kernel (`k_store_restore_buf`),
/// laid in; each file is checked when a process first uses it (`note_key`).
/// Returns `k_store_restore`'s answer.
#[no_mangle]
pub extern "C" fn gex_xcache_restore() -> i32 {
    if unsafe { k_present() } == 0 {
        return -1;
    }
    unsafe { k_store_restore() }
}

fn page_identity(p: &Px<'_>, page: u64) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let mm = p.mm.lock();
    let v = mm.vmas().iter().find(|v| page >= v.start && page < v.end)?;
    // The two reasons a page has no identity, counted apart: a writable mapping
    // (the guest could have changed it) versus one the loader declined to name.
    if v.prot & gex_kernel::mem::prot::WRITE != 0 {
        NOIDENT_WHY.with(|c| c.borrow_mut().0 += 1);
        return None;
    }
    if v.code.is_none() {
        NOIDENT_WHY.with(|c| {
            let mut c = c.borrow_mut();
            c.1 += 1;
            // The first few, named: a count says how often, only the mapping
            // says which loader path left it unnamed.
            if tracing() && c.1 <= 8 {
                trace_line(&format!(
                    "[gex] no code identity for {:#x}: vma {:#x}..{:#x} prot {} name {:?}\n",
                    page, v.start, v.end, v.prot, v.name
                ));
            }
        });
    }
    // The `code` identity (what was loaded here), not `origin` (what could be
    // re-read): an image segment and an `mprotect`ed RELRO piece both lose
    // `origin` without the code differing.
    let (id, off) = v.code.as_ref()?;
    // The file and the offset within it, and nothing else. The mapping's extent
    // once lived in the key and made it unstable: `ld.so` splits a vma four
    // ways per library, so which side a page lands on -- and the RELRO
    // `mprotect` after some pages are translated -- changed a page's hash. The
    // reusable-translation question is the *address*, already the other half of
    // the `(page, identity)` key.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    (off + (page - v.start)).hash(&mut hasher);
    let key = hasher.finish();
    note_key(p, key, id);
    Some(key)
}

/// Nanoseconds on the host clock.
fn now_ns() -> u64 {
    host_now_ns()
}

thread_local! {
    /// The clock as this pass first read it ([`turn_now`]); 0 for unread.
    static TURN_NOW: Cell<u64> = const { Cell::new(0) };
    /// Whether a [`NowScope`] is open on this Worker.
    static NOW_SCOPED: Cell<bool> = const { Cell::new(false) };
}

/// The clock for one pass through the kernel: inside a [`NowScope`] (a
/// syscall, a turn of a wasm32 program's wait loop) it is read once, lazily,
/// and every later question in the pass gets the same answer; outside one,
/// each call reads it. The host's clock is a JS call (`performance.now`,
/// about 0.5 us in Chrome), and a syscall that blocked asked it up to five
/// times (its deadline, the process timer, readiness, how the wait ended,
/// how long to park): 37% of a pipe ping-pong's busy time.
pub(crate) fn turn_now() -> u64 {
    if !NOW_SCOPED.with(|s| s.get()) {
        return now_ns();
    }
    TURN_NOW.with(|c| match c.get() {
        0 => {
            let n = now_ns().max(1);
            c.set(n);
            n
        }
        n => n,
    })
}

/// Time has passed (this Worker parked): the next [`turn_now`] reads again.
pub(crate) fn stale_now() {
    TURN_NOW.with(|c| c.set(0));
}

/// No [`NowScope`] is open any more: a trap left the frames that held them
/// (`trap`), and their `Drop` never runs.
fn end_now_scopes() {
    NOW_SCOPED.with(|s| s.set(false));
    stale_now();
}

/// A pass whose clock questions share one reading ([`turn_now`]). Nested
/// scopes start their own reading; the outer one reads again after.
pub(crate) struct NowScope(bool);

impl NowScope {
    pub(crate) fn open() -> NowScope {
        stale_now();
        NowScope(NOW_SCOPED.with(|s| s.replace(true)))
    }
}

impl Drop for NowScope {
    fn drop(&mut self) {
        NOW_SCOPED.with(|s| s.set(self.0));
        stale_now();
    }
}

/// What a scheduler turn that found nothing to run saw, under the one lock
/// it took to decide: everything the idle path then acts on, so that a pass
/// of a waiting Worker's loop takes the kernel's lock once rather than seven
/// times (docs/kernel-locking.md, step 3).
struct IdleLook {
    /// What the page must supply, as `paused()` bits (see `idle_why`).
    why: u8,
    /// Every thread parked on an untimed futex and none of them ready: a
    /// deadlock if it persists. Only asked when `why` is 0.
    all_blocked: bool,
    /// This Worker's thread is in a wait that ends without the page.
    ends_by_itself: bool,
}

/// What the scheduler decided.
enum Pick {
    /// This Worker's thread ran again ([`resume_thread`]): where it resumes,
    /// or `None` when its process ended.
    Resumed(Option<(u64, u64)>),
    /// Nothing in this process can run yet, and what the same look found
    /// about why (see [`IdleLook`]).
    Idle(IdleLook),
    /// This process has finished.
    Over,
    /// This Worker's thread became ready while its wait was being
    /// registered: look again.
    Again,
}

/// Whether a thread's wait is over: what a lock-free idle turn asks of the wait it
/// registered (`waitq`). Reads nothing the kernel lock guards -- handles are
/// reference-counted and synchronise themselves, the futex generations and
/// the process table are the global kernel's.
fn wait_ready(waiting: &Waiting, blocked_on: &Option<gex_kernel::BlockOp>, now: u64, signalled: bool) -> bool {
    // A signal ends a wait -- every interruptible one, all here but a thread
    // exited or running in another Worker. Becoming runnable is all that is
    // needed: the picked thread finishes its parked call, and the code after
    // already asks `deliver_pending`,
    // which either builds the handler's frame or ends the process.
    if SIGNAL_ENDS_WAIT && signalled && !matches!(waiting, Waiting::Exited | Waiting::Elsewhere) {
        return true;
    }
    match waiting {
        Waiting::Runnable => true,
        Waiting::Futex { parked, deadline, val, wake_gen } => {
            // Four ways this wait ends. `woken()` is a same-Worker wake; the
            // deadline is a timed wait. The third is the CROSS-Worker wake, made
            // exact: the shared per-address generation is bumped by every
            // `FUTEX_WAKE` (relayed after the waking syscall), so a change since
            // `wake_gen` is a wake that happened, whatever the word now reads --
            // which is what a thread pool sharing one condvar needs, its counter
            // often back at `val` before a woken worker is scheduled.
            //
            // The word re-read is ONLY a lone-process fallback (no global kernel,
            // so no shared generation to consult), and gating it on that is
            // load-bearing for a threaded process: a plain read of the guest word
            // carries no synchronisation, so a Worker that is scheduling (not
            // parked on the wake atomic) could see the word changed and resume
            // the thread without the acquire the wake atomic would have given it
            // -- and then read other stores the waker made *before* the change
            // as stale. That is exactly a lost `pthread_join` return: the exiting
            // thread writes `result` then clears its tid, and a joiner resumed by
            // the bare tid re-read saw the tid zero but `result` still zero. With
            // a global kernel every real futex wake goes through `FutexTable::wake`
            // -> `woken()` (set under its queue's own lock) and the relayed generation, both
            // properly ordered, so the re-read adds nothing but the hazard.
            parked.woken()
                || deadline.is_some_and(|d| now >= d)
                || (unsafe { k_present() } != 0
                    && unsafe { k_futex_gen(parked.addr()) } != *wake_gen)
                || (unsafe { k_present() } == 0
                    && GuestMem::new(guest_bytes())
                        .read_u32(parked.addr())
                        .map(|w| w != *val)
                        .unwrap_or(false))
        }
        Waiting::Sleep(d) => now >= *d,
        Waiting::Read(handle) => handle.poll_readable(),
        Waiting::Write(handle) => handle.poll_writable(),
        Waiting::Net(deadline) => {
            deadline.is_some_and(|d| now >= d)
                || match blocked_on {
                    Some(gex_kernel::BlockOp::Connect { socket }) => {
                        socket.status() != gex_kernel::net::Status::Connecting
                    }
                    // An epoll set whose interest list changed since its
                    // descriptors were collected: the call again, which
                    // collects them afresh.
                    Some(gex_kernel::BlockOp::Poll {
                        report: gex_kernel::syscalls::PollReport::Epoll { set, generation, .. },
                        ..
                    }) if set.generation() != *generation => true,
                    Some(gex_kernel::BlockOp::Poll { fds, .. }) => {
                        gex_kernel::syscalls::poll_ready(fds)
                    }
                    _ => true,
                }
        }
        // Ready when one of the children this call can collect has a status
        // waiting, and also when there are none at all -- that is `ECHILD`,
        // which is an answer and not a reason to sleep forever.
        Waiting::Child => match blocked_on {
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
        Waiting::Signal => false,
        // One of the set pending: the thread's own, or the process's (what
        // other processes sent is collected before this is asked,
        // `sigs::ready`).
        Waiting::SigWait(deadline) => {
            deadline.is_some_and(|d| now >= d)
                || matches!(blocked_on, Some(gex_kernel::BlockOp::SigWait { set, thread, process, .. })
                    if (thread.pending() | process.shared()) & set != 0)
        }
        Waiting::Pi { addr, gen, deadline } => {
            deadline.is_some_and(|d| now >= d)
                || (unsafe { k_present() } != 0 && unsafe { k_futex_gen(*addr) } != *gen)
                || gex_atomic::load_u32(guest_bytes(), *addr)
                    .is_some_and(|v| v & gex_kernel::futex::pi::TID_MASK == 0)
        }
        Waiting::Exited => false,
        // Somebody else's to run, however ready it is.
        Waiting::Elsewhere => false,
    }
}

/// What every thread of this process is waiting for, as ASCII in the caller's
/// memory, returning the length. The host asks when a run has gone quiet: the
/// other censuses fire on counters a deadlock never reaches, so a wedged run
/// was the one state no instrument could see.
#[no_mangle]
pub extern "C" fn gex_wait_census(out: u32, max: u32) -> u32 {
    // The pause mask and resume address too: a thread that says `runnable` with
    // a loop that parks anyway is unexplained without them.
    let why = paused();
    let resume = resume().unwrap_or(0);
    // Whether this process is over: the run loop checks it first, so a finished
    // process takes the waiting branch however runnable its threads look.
    let over = sigs::home().is_some_and(|h| h.exit().over());
    let text = sigs::home().map(|home| {
        let ns = home.ns();
        let px = Px { mm: home.mm(), ns: &ns, pid: home.pid() };
        let mut s = format!(
            "pid {} worker-runs-t{} paused={why:#x} resume={resume:#x} exited={over}:",
            home.pid(),
            cur(),
        );
        for (i, t) in home.table().iter() {
            let r = t.run();
            s.push_str(&format!(" t{i}={}@{:#x}", waiting_name(&r.waiting), r.pc));
            // The LIVE pc from the context, not `r.pc` (updated only on a
            // block): a spinning `runnable` thread's real pc is here.
            if matches!(r.waiting, Waiting::Runnable) {
                let gm = GuestMem::new(guest_bytes());
                let live = gm.read_u64(t.ctx_addr() + ctx::PC as u64).unwrap_or(0);
                // Also the file offset within the object, so `addr2line` can name
                // the exact function a runnable-but-frozen thread is stuck in.
                let foff = home.mm().lock()
                    .vmas()
                    .iter()
                    .find(|v| live >= v.start && live < v.end)
                    .and_then(|v| v.code.as_ref().map(|(_, off)| off + (live - v.start)));
                s.push_str(&format!(
                    "[live={live:#x} {}+{:#x}]",
                    object_name_at(&px, live),
                    foff.unwrap_or(0)
                ));
            }
            // Name a futex park's object too, so a deadlock says which lib's
            // lock each side is on.
            if let Waiting::Futex { parked, .. } = &r.waiting {
                s.push_str(&format!("[{}]", object_name_at(&px, parked.addr())));
            }
            // Not just "read" but what it is reading -- the question when two
            // processes wait for each other.
            match &r.waiting {
                Waiting::Read(h) | Waiting::Write(h) | Waiting::Accept(h) => {
                    let d = h.debug_state();
                    if !d.is_empty() {
                        s.push_str(&format!("({d})"));
                    }
                }
                _ => {}
            }
            if let Some(gex_kernel::BlockOp::WaitChild { want, .. }) = &r.blocked_on {
                s.push_str(&format!("(want {want})"));
            }
        }
        // This Worker's recent futex traffic (op 0=wait 1=wake), so t0's wakes
        // can be checked against the address t1 parked on.
        s.push_str(" ftx:");
        s.push_str(&ftx_recent());
        s
    })
    .unwrap_or_else(|| "pid ?: the kernel is held".to_string());
    let b = text.as_bytes();
    let n = (b.len() as u32).min(max);
    let mut gm = GuestMem::new(guest_bytes());
    let _ = gm.write(out as u64, &b[..n as usize]);
    n
}

/// What a parked thread is waiting for, in one word for a census: only the
/// shape of the wait matters, not the handles and futex parks `Waiting` holds.
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
        Waiting::Signal => "sigsuspend",
        Waiting::SigWait(_) => "sigtimedwait",
        Waiting::Pi { .. } => "pi-lock",
        Waiting::Exited => "exited",
    }
}

/// How many scheduler turns in a row found nothing to run. A static because
/// each turn is its own call (the scheduler hands the stack back and is
/// re-entered), so a local would reset every time.
static IDLE_SPINS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// One scheduler turn found nothing to run. Reported on powers of two: a guest
/// legitimately waiting for a keystroke passes here a thousand times a second,
/// so a resolved wait says nothing and a stuck one gets louder.
fn report_idle(why: u8) {
    let n = IDLE_SPINS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if !n.is_power_of_two() || n < 1024 {
        return;
    }
    // Without the kernel lock: the thread table, each run state, the map
    // and the descriptor table lock themselves (L23).
    let census = sigs::home().map(|home| {
        let mut out = String::new();
        let table = fds_free();
        // This Worker's process only, with its descriptor table so a polled fd
        // can be named; the other half of a cross-process wait is in the other
        // Worker's census.
        for (pid, p, k) in std::iter::once((home.pid() as usize, home.table(), home)) {
            for (i, t) in p.iter() {
                let r = t.run();
                out.push_str(&format!(" {pid}.{i}={}@{:#x}", waiting_name(&r.waiting), r.pc));
                // The live ctx pc (updated by the JIT at block boundaries), not
                // just the last saved kstate pc: a thread "runnable"/"another
                // worker" whose Worker is stalled in JS shows its real position
                // here, where r.pc is still the entry. Names the block a stranded
                // nested-clone thread died on (native-wayland gtk stall).
                let cpc = GuestMem::new(guest_bytes())
                    .read_u64(t.ctx_addr() + ctx::PC as u64)
                    .unwrap_or(0);
                if cpc != r.pc {
                    let where_ = k.mm().lock().vmas().iter()
                        .find(|v| cpc >= v.start && cpc < v.end)
                        .map(|v| format!(" in {} +{:#x}", v.name, cpc - v.start))
                        .unwrap_or_default();
                    out.push_str(&format!("(cpc={cpc:#x}{where_})"));
                }
                // For a futex park, the lost-wake fingerprint: address, park-time
                // shared gen vs now (equal => no wake relayed here), guest word vs
                // slept value (differ => guest was woken but the gen never moved).
                if let Waiting::Futex { parked, val, wake_gen, .. } = &r.waiting {
                    let a = parked.addr();
                    let gen_now = if unsafe { k_present() } != 0 { unsafe { k_futex_gen(a) } } else { 0 };
                    let word = GuestMem::new(guest_bytes()).read_u32(a).unwrap_or(0);
                    out.push_str(&format!(
                        "[ftx a={a:#x} gen={wake_gen}/{gen_now} word={word:#x}/slept={val:#x} woken={}]",
                        parked.woken()
                    ));
                }
                // For a poll, which descriptors and how each answers now:
                // whether the awaited thing is already true is the difference
                // between a wait and a deadlock.
                if let Some(gex_kernel::BlockOp::Poll { fds, .. }) = &r.blocked_on {
                    out.push('(');
                    for f in fds {
                        let what = table
                            .and_then(|t| t.get(f.fd).ok())
                            .map(|o| {
                                let d = o.handle.debug_state();
                                if d.is_empty() { o.path.clone() } else { d }
                            })
                            .unwrap_or_else(|| "?".into());
                        out.push_str(&format!(
                            "[{} {what} want{:#x} have{:#x}] ",
                            f.fd,
                            f.events,
                            gex_kernel::syscalls::poll_revents(f)
                        ));
                    }
                    out.push(')');
                }
                // For a plain blocking read or write, what it is on: a pipe
                // that says "0 buffered" is a different bug from a terminal
                // waiting for a key nobody will press.
                match &r.waiting {
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
    }).unwrap_or_default();
    // Built from the bits, not matched on the whole mask: a match on exact
    // values misreports any mask carrying a new bit.
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
    let klock = klock_holder();
    trace_line(&format!(
        "[gex] {n} turns with nothing runnable, wanting {asked}, klock={klock}, threads:{census}\n"
    ));
}

/// What the page must supply for a thread's wait to end, as `paused()`
/// bits: 1 a keystroke, 2 the event loop to turn, 4 only the page can end
/// it; of its run state `r`, without the kernel. Its own thread only: a
/// Worker runs one, and another thread's wait is its own Worker's to report.
fn wait_why(r: &Run) -> u8 {
    // Only a terminal is a keystroke; a pipe or socket is written by
    // another process (apt's http method, a pipeline) and wants the
    // event loop, not a key.
    // Bit 1 means "the page has to supply a keystroke", and the JS
    // side answers it by *awaiting* one -- with no timeout when
    // nothing else is set. So it must be reserved for the page's own
    // terminal. A pty is not that: it is two ends in the global
    // kernel, and the end that will write is a program in another
    // Worker. Classed as a keyboard, a `select` on one waited for a
    // keystroke into the page that was never coming, which is what
    // made an interactive bash in xterm go deaf after one character
    // -- readline waits in `select` where a plain shell waits in
    // `read`, and a read happened to also set bit 2 and be retried.
    let by_kind = |handle: &Arc<dyn gex_kernel::vfs::FileHandle>| {
        use gex_kernel::vfs::FileType;
        let stdin_p = Arc::as_ptr(&*STDIN) as *const ();
        if Arc::as_ptr(handle) as *const () == stdin_p {
            return 1;
        }
        if handle.peer_wakes() {
            return 2;
        }
        match handle.metadata().map(|m| m.file_type) {
            Ok(FileType::Socket) | Ok(FileType::Fifo) => 2,
            _ => 1,
        }
    };
    let mut mask = 0u8;
    // Bit 2: only the page can end this wait (a network callback on
    // this Worker's event loop, so it cannot be slept on, unlike a
    // wait another Worker's notify ends). Always set with bit 1,
    // never alone -- alone it answers "paused for input?" and
    // "paused for net?" both no, a silent death read as a clean
    // exit. Per wait, not per process: apt has a network, but its
    // pipe to the http method is filled by another Worker.
    if wait_needs_page(r) {
        mask |= 2 | 4;
    }
    match &r.waiting {
        // Like a read: a pipe or socket is drained by another
        // process, a terminal by the page.
        Waiting::Write(handle) => mask |= by_kind(handle),
        // A listener's client connects from outside the guest,
        // so the event loop must turn.
        Waiting::Accept(_) => mask |= 2,
        Waiting::Read(handle) => {
            mask |= by_kind(handle);
        }
        // Every timed wait and poll parks as `Net`, but most are
        // not the network -- an interactive shell polls its
        // terminal -- so a poll is classified by what it polls.
        Waiting::Net(deadline) => {
            match &r.blocked_on {
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
    mask
}

/// Choose the next guest thread to run, and finish whatever syscall it was in
/// the middle of. It no longer chooses between *processes*: there is one per
/// Worker, and the others run elsewhere. Handing the stack back, this
/// Worker stops counting for the probe cache's indices; resuming, it counts
/// again and is grown first (`cgen::ftab_out`, `cgen::ftab_resume`).
fn schedule() -> i32 {
    let r = schedule_turn();
    if r == 1 {
        cgen::ftab_resume();
    } else {
        cgen::ftab_out(SCHED_OVER.with(|c| c.get()));
    }
    r
}

fn schedule_turn() -> i32 {
    // What this Worker's wait slot read *before* anything was asked about
    // what is runnable, so that a notify arriving while this makes up its
    // mind cannot be slept through: `gex_park` sleeps only while the slot
    // still reads this.
    // A blocking call's own take may have made the first turn already
    // (`FIRST_PICK`), after reading the slot itself.
    let first = FIRST_PICK.with(|c| c.borrow().is_some());
    if !first {
        waitq::begin_turn();
    }
    SCHED_OVER.with(|c| c.set(false));
    // Unconditional and at the top: every other counter sits on a path that
    // can be missed, and this is the last place everything passes through. If
    // it climbs, the loop is out here; if not, the guest is inside a stretch of
    // translated code with a back-edge that never polls.
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
    // A turn of a wait this Worker already registered asks only its own
    // thread, from what that wait was, without the kernel lock: a parked
    // thread woken by something it does not wait for, or by nothing, looks
    // and parks again (docs/kernel-locking.md §5.2 step 2); one whose wait
    // is over finishes its call and runs (§3.7).
    match if first { waitq::Turn::Full } else { waitq::idle_turn() } {
        waitq::Turn::Idle(why, ends_by_itself) => {
            if why != 0 {
                report_idle(why);
                set_paused_to(why);
                return 2;
            }
            if !ends_by_itself {
                report_idle(0);
            }
            set_paused_to(2);
            return 2;
        }
        waitq::Turn::Ready(signalled) => return finish_mine(signalled),
        // Every thread parked on a futex: a deadlock once it lasts.
        waitq::Turn::Stuck => {
            if deadlock_confirmed() {
                report_deadlock();
                return 2;
            }
            set_paused_to(2);
            return 2;
        }
        waitq::Turn::Full => {}
    }
    loop {
        // Before anything is chosen, the compositor: a wayland client writes
        // a request then blocks on the reply, so it must run here, the one
        // place every thread passes through on its way to waiting. Needs the
        // kernel (to find a buffer's shared mapping) and guest memory (the
        // pixels), so it pumps inside `with_exec_lock`; the device is a
        // cheap Arc clone that aliases none of the host's mutable state.
        let pick = match FIRST_PICK.with(|c| c.take()) {
            Some(pick) => pick,
            None => {
                pump_compositor();
                match MY_RUN.with(|c| c.borrow().clone()) {
                    Some(run) => pick_mine(&run),
                    None => {
                        set_paused_to(2);
                        return 2;
                    }
                }
            }
        };
        // Going round again is what picks a thread and, crucially, finishes the
        // syscall it parked in; resuming its saved pc directly leaves x0 with
        // no return value.
        if matches!(pick, Pick::Over) {
            waitq::leave();
            SCHED_OVER.with(|c| c.set(true));
            return 2;
        }
        if matches!(pick, Pick::Again) {
            continue;
        }
        let Pick::Resumed(resume) = pick else {
            let Pick::Idle(look) = pick else { unreachable!() };
            // Nothing can run, but not everything that cannot run is stuck. A
            // thread wanting a keystroke needs the page (bit 0); one waiting on
            // the network needs its event loop to turn (bit 1). Both mean
            // giving the stack back, and the two are not exclusive -- a shell in
            // one process and a download in another want both -- so this is a
            // pair of bits classified from each handle.
            let IdleLook { why, all_blocked, ends_by_itself } = look;
            if why != 0 {
                // Counted, the case that hides: handing the stack back for a
                // wait the page satisfies is right, but doing it a thousand
                // times a second for one nothing satisfies is a silent hang.
                report_idle(why);
                set_paused_to(why);
                return 2;
            }
            if all_blocked && deadlock_confirmed() {
                report_deadlock();
                return 2;
            }
            // Nothing runnable and nothing above recognised the wait: going
            // round again would spin on unchanged state at full speed. So
            // hand the stack back -- yielding cannot be wrong when nothing can
            // run -- and report it, unless every wait is one that ends by
            // itself: a sleep (the clock), a `wait4` (the child, in another
            // Worker, exits), a futex (another Worker wakes it; one nothing
            // can wake is the deadlock caught above). A parent waiting out a
            // long child reported itself "idle" every power of two.
            // (This Worker's thread: a Worker owns exactly one, and the
            // others' entries are their own Workers' business.)
            if !ends_by_itself {
                report_idle(0);
            }
            set_paused_to(2);
            return 2;
        };
        match resume {
            // Both are this Worker's own (`RESUME`, `RESUME_CTX`): no kernel.
            Some((ctx_addr, pc)) => {
                set_resume(pc);
                set_resume_ctx(ctx_addr);
                return 1;
            }
            None => return 2,
        }
    }
}

/// Whether the all-blocked state this Worker's turn found has lasted long
/// enough to be a deadlock. Confirm it persists before acting: a single
/// all-blocked instant is not a deadlock -- during startup one is routine --
/// so it takes a run of them at a wake seq that never moves (a real deadlock
/// runs no more syscalls, so nothing bumps the counter). A moved seq resets
/// the count: something woke, so progress is possible. The seq is this
/// Worker's wait slot's, which every wake of any of the process's threads'
/// waits does not move -- but a wake of this one's does, and a deadlock
/// moves nothing. No kernel lock.
fn deadlock_confirmed() -> bool {
    let seq = if unsafe { k_present() } != 0 { unsafe { k_ws_seq(waitq::slot()) } } else { 0 };
    DEADLOCK_WATCH.with(|w| {
        let (last, n) = w.get();
        let n = if n > 0 && last == seq { n + 1 } else { 1 };
        w.set((seq, n));
        n >= DEADLOCK_CONFIRM
    })
}

/// Every remaining thread parked on a futex nothing will wake. On Linux this
/// hangs; saying which futex and where the guest stands turns it from a
/// symptom into something to look up. The process ends (`set_exit`), which
/// is the kernel's.
fn report_deadlock() {
    // Who is parked where, without the kernel lock (L23); the end is the
    // kernel's.
    let who = sigs::home().map(|home| {
        let mut s = String::new();
        let gm = GuestMem::new(guest_bytes());
        for (i, t) in home.table().iter() {
            let r = t.run();
            if let Waiting::Futex { parked, .. } = &r.waiting {
                let pc = gm.read_u64(t.ctx_addr() + ctx::PC as u64).unwrap_or(0);
                s.push_str(&format!(
                    "gex:   thread {i} parked on {:#x}, pc {:#x}{}\n",
                    parked.addr(),
                    pc,
                    home.mm()
                        .lock()
                        .vmas()
                        .iter()
                        .find(|v| pc >= v.start && pc < v.end)
                        .map(|v| format!(" in {} +{:#x}", v.name, pc - v.start))
                        .unwrap_or_default(),
                ));
            }
        }
        s
    }).unwrap_or_default();
    trace_line("gex: every thread is blocked on a futex\n");
    trace_line(&who);
    set_exit(128 + 31);
}

/// The compositor's work, when it has any, before a scheduler turn: a
/// wayland client writes a request then blocks on the reply, so it must run
/// where every thread passes on its way to waiting. What it writes to a
/// client's socket (a frame callback, a buffer release, input) notifies the
/// socket's waiters. No kernel lock: what it needs of the process is
/// published (`wl::ProcCx`).
fn pump_compositor() {
    wl::pump_if_wanted();
}

/// Every thread gone ends this process, all this Worker has; the status is
/// in the shared table for whoever waits. Asked where a thread exits, under
/// that call's take: the last one out ends it.
pub(crate) fn end_if_all_exited() {
    let Some(home) = sigs::home() else { return };
    if home.table().iter().all(|(_, t)| matches!(t.run().waiting, Waiting::Exited)) {
        exit_process(home.exit().status().unwrap_or(0));
    }
}

/// Whether this Worker, its own thread exited, waits for its process to end
/// rather than ending: the process's first Worker (thread 0), whose end is
/// the process's and whose answer is the process's status. Its thread may
/// `pthread_exit` while another goes on, as on Linux, where the process
/// lives until its last thread ends it (Wine's `ExitProcess` from a second
/// thread does exactly this). It parks on the process's queue, which
/// `exit_process` notifies.
fn leader_waits() -> bool {
    cur() == 0 && !exited()
}

/// Whether a wait that is over timed out, and whether a signal and not the
/// wait ended it (`signalled`: one would do something to the thread).
fn how_ended(r: &Run, now: u64, signalled: bool) -> (bool, bool) {
    let timed_out = match &r.waiting {
        // A cross-Worker FUTEX_WAKE advances the shared generation
        // without touching this Worker's local `Parked` queue. It
        // is still a wake, not a timeout. The old `!parked.woken()`
        // test therefore handed ETIMEDOUT to every such untimed
        // glibc lock wait, which glibc correctly treats as a fatal
        // futex result. Only an expired deadline is a timeout.
        Waiting::Futex { deadline, .. } | Waiting::SigWait(deadline) => deadline.is_some_and(|at| now >= at),
        _ => false,
    };
    // Woken by the signal and not by the wait: ask the same
    // question with the signal taken away.
    let by_signal = signalled && !wait_ready(&r.waiting, &r.blocked_on, now, false);
    (timed_out, by_signal)
}

/// An idle turn's wait found over ([`waitq::idle_turn`]): this Worker's
/// thread runs again, its parked call finished here, with no kernel lock --
/// its run state is its own (`MY_RUN`), what the call owes was taken from
/// the kernel when it parked, and the answer and any signal go into its own
/// context. `schedule`'s answer.
fn finish_mine(signalled: bool) -> i32 {
    let Some(run) = MY_RUN.with(|c| c.borrow().clone()) else {
        set_paused_to(2);
        return 2;
    };
    let mut r = run.lock().unwrap_or_else(|e| e.into_inner());
    // The process ended under it (`exit_process` marks every thread).
    if matches!(r.waiting, Waiting::Exited) {
        drop(r);
        if leader_waits() {
            set_paused_to(2);
            return 2;
        }
        waitq::leave();
        SCHED_OVER.with(|c| c.set(true));
        return 2;
    }
    let now = turn_now();
    if !wait_ready(&r.waiting, &r.blocked_on, now, signalled) {
        // What the idle turn asked was the registered copy of this same
        // wait; the next turn asks again.
        set_paused_to(2);
        return 2;
    }
    let (timed_out, by_signal) = how_ended(&r, now, signalled);
    waitq::leave();
    IDLE_SPINS.store(0, std::sync::atomic::Ordering::Relaxed);
    let done = finish_wait(&mut r, cur(), my_ctx(), my_tid(), timed_out, by_signal);
    drop(r);
    match done {
        Done::At(ctx_addr, pc) => {
            set_resume(pc);
            set_resume_ctx(ctx_addr);
            1
        }
        Done::Ends(code) => {
            exit_here(code, None);
            2
        }
    }
}

/// A call of the lock-free class (`gex_kernel::syscalls::free_class`:
/// `read`, `write`, their vectored and positioned forms, `lseek`,
/// `rt_sigprocmask` and the poll family), made with no kernel lock, blocking included
/// (docs/kernel-locking.md step 9); `None` for any other call, decided by
/// its number before anything is done. `gex_syscall`'s stop value.
fn free_syscall(ctx: i32, pc: i64, mapped: &mut Vec<gex_kernel::CodeMap>) -> Option<i32> {
    use gex_kernel::signal::Delivery;
    use gex_kernel::syscalls::{nr, FreeOutcome};
    let ctx_addr = ctx as u32 as u64;
    let mut gm = GuestMem::new(guest_bytes());
    let compat = CLOCK_ABI.load(std::sync::atomic::Ordering::Acquire) == 1;
    // The dynamic linker's jump to gex's stub entry, at a process's start
    // (`SNAP_SYSCALL`): what it built is kept as an exec's image is made,
    // under the exec lock.
    if gm.read_u64(ctx_addr + ctx::SYSCALL_NR as u64).unwrap_or(0) == SNAP_SYSCALL && SNAP_PENDING.0.borrow().1 != 0 {
        return None;
    }
    let call = gex_kernel::syscalls::free_class(&gm, ctx_addr, compat)?;
    // Its process has exited under it (as the full call): the thread goes,
    // without the call.
    if exited() {
        set_resume_none();
        if let Some(run) = MY_RUN.with(|c| c.borrow().clone()) {
            run.lock().unwrap_or_else(|e| e.into_inner()).waiting = Waiting::Exited;
        }
        return Some(3);
    }
    let n = call.number();
    let [a0, a1, a2] = call.args();
    let slot = sys_note(n, a0, a1);
    let outcome = if matches!(n, nr::MMAP | nr::MUNMAP | nr::MPROTECT | nr::MREMAP) {
        // A call that can take code away: made and its code retired under
        // the JIT lock, so no thread runs or translates what it took away
        // between the two, and an address it frees cannot be mapped again
        // with new code until its translations are gone (as one kernel-lock
        // section made them; docs/kernel-locking.md §4.3, L12b). Its mapped
        // code is queued below, as the locked section's is.
        with_jit_only(|_| {
            let o = sigs::free_call(&mut gm, ctx_addr, call, compat, pc as u64);
            *mapped = map_drain();
            o
        })
    } else {
        sigs::free_call(&mut gm, ctx_addr, call, compat, pc as u64)
    };
    if n == nr::FUTEX || n == nr::EXIT {
        // Every futex this call woke (an `exit` wakes its thread's joiner),
        // relayed to the shared per-address counter, so a waiter parked in
        // another Worker sees the wake.
        if unsafe { k_present() } != 0 {
            if let Some(home) = sigs::home() {
                for addr in home.futexes.take_woken() {
                    unsafe { k_futex_wake(addr) };
                }
            }
        }
        // Every futex answer that is an error, by name (`ftx_recent`).
        if let (nr::FUTEX, FreeOutcome::Done(_)) = (n, &outcome) {
            let ret = gm.read_u64(ctx_addr + ctx::x(0) as u64).unwrap_or(0);
            ftx_note(a1, a0, ret as i64, pc as u64, my_tid());
        }
    }
    if matches!(n, nr::SETITIMER | nr::ALARM) {
        // A timer armed sooner than the parked threads sleep to: they look
        // again (as the locked call's end does).
        waitq::publish_timer();
    }
    if matches!(n, nr::KILL | nr::TKILL | nr::TGKILL) {
        // As the locked call's end: the waits look again, an abort is named.
        let sig = if n == nr::TGKILL { a2 } else { a1 };
        with_px(|p| kill_sent(&gm, ctx_addr, my_pid(), sig, p));
    }
    if let FreeOutcome::Done(_) = &outcome {
        let ret = gm.read_u64(ctx_addr + ctx::x(0) as u64).unwrap_or(0);
        sys_ret(slot, ret);
        if !gex_abi::X86 && !compat && (a0 == 1 || a0 == 2) && (n == nr::WRITE || n == nr::WRITEV) {
            stderr_capture(&gm, a0, n == nr::WRITEV, a1, a2, ret);
        }
    }
    Some(match outcome {
        FreeOutcome::Done(Delivery::None) => 0,
        // A handler's frame, built: the guest goes on there.
        FreeOutcome::Done(Delivery::Resume(target)) => {
            set_resume(target);
            1
        }
        // The process ends: its teardown is the kernel's.
        FreeOutcome::Done(Delivery::Terminate { signal, .. }) => {
            let at = gm.read_u64(ctx_addr + ctx::PC as u64).unwrap_or(0);
            exit_here(-(signal as i32), Some(at))
        }
        FreeOutcome::Exit(code) => exit_here(code, None),
        // `exit` from one of several threads (L25a): this thread is over;
        // the last one out of the thread table ends the process.
        FreeOutcome::ThreadExit => {
            if let Some(run) = MY_RUN.with(|c| c.borrow().clone()) {
                run.lock().unwrap_or_else(|e| e.into_inner()).waiting = Waiting::Exited;
            }
            end_if_all_exited();
            // Nowhere to resume this thread (as the kernel's `ThreadExit`).
            set_resume_none();
            set_resume_ctx_none();
            3
        }
        FreeOutcome::SpawnThread { ctx_addr, tid } => thread_made(tid, ctx_addr, pc as u64),
        FreeOutcome::Blocking(op, restore_sigmask) => park_free(op, restore_sigmask, pc as u64),
        // A wasm32 program's sigreturn put back a call its handler
        // interrupted: that call, made now as a syscall of its own (without
        // the lock if it is a class call, as any other).
        FreeOutcome::Again => return free_syscall(ctx, pc, mapped),
    })
}

/// Park a class call ([`free_syscall`]) on its stream, without the kernel:
/// the wait goes into the thread's own run state, and the scheduler's first
/// turn is made here (`FIRST_PICK`), as the locked `Blocking` arm makes it
/// under its take. `gex_syscall`'s stop value.
fn park_free(op: gex_kernel::BlockOp, restore_sigmask: Option<u64>, pc: u64) -> i32 {
    let waiting = match &op {
        gex_kernel::BlockOp::ReadStream { handle, .. } => {
            if tracing() {
                trace_line(&format!("[gex:{}] park read {}\n", my_pid(), handle.debug_state()));
            }
            Waiting::Read(Arc::clone(handle))
        }
        gex_kernel::BlockOp::WriteStream { handle, .. } => Waiting::Write(Arc::clone(handle)),
        // As the kernel's park: re-checked on the scheduler's way round,
        // woken by the descriptors' queues (`waitq::queues_of`).
        gex_kernel::BlockOp::Poll { deadline, .. } => Waiting::Net(deadline.map(|d| turn_now() + d.as_nanos() as u64)),
        // A futex wait, as the kernel's park: the shared generation of the
        // address sampled now, after the word was checked (a wake since
        // bumps it, so the first look sees it), and the place the call took
        // before it checked the word (a wake between the two is seen there).
        gex_kernel::BlockOp::FutexWait { table, uaddr, deadline, parked, val } => Waiting::Futex {
            val: *val,
            wake_gen: if unsafe { k_present() } != 0 { unsafe { k_futex_gen(*uaddr) } } else { 0 },
            parked: parked.clone().unwrap_or_else(|| table.park(*uaddr)),
            deadline: deadline.map(|d| turn_now() + d.as_nanos() as u64),
        },
        // A priority-inheritance lock found held: tried again when the
        // thread runs (`finish_parked`), its call made again until it is
        // taken.
        gex_kernel::BlockOp::FutexLockPi { uaddr, deadline, .. } => Waiting::Pi {
            addr: *uaddr,
            gen: if unsafe { k_present() } != 0 { unsafe { k_futex_gen(*uaddr) } } else { 0 },
            deadline: deadline.map(|d| turn_now() + d.as_nanos() as u64),
        },
        // A sleep, until its time (the clock alone ends it).
        gex_kernel::BlockOp::Sleep(d) => Waiting::Sleep(turn_now() + d.as_nanos() as u64),
        // A `recvmsg` on a blocking socket: until it is readable, then made
        // again (`Owed::reissue`).
        gex_kernel::BlockOp::AcceptWait { socket } => Waiting::Accept(Arc::clone(socket)),
        // A blocking `connect` (L18), as the kernel's park: until the
        // connection is up, refused or gone (`wait_ready`'s `Net`).
        gex_kernel::BlockOp::Connect { .. } => Waiting::Net(None),
        // `rt_sigsuspend` (L22d), as the kernel's park: until a signal it
        // can see arrives (another process's notifies this process's wait
        // queue, a thread's of its own `waitq::wake_process`, a timer's is
        // the wait's deadline), or at once if one already can be.
        gex_kernel::BlockOp::SigSuspend { .. } => {
            if sigs::ready() || unsafe { k_present() } == 0 {
                Waiting::Runnable
            } else {
                Waiting::Signal
            }
        }
        // `rt_sigtimedwait`, as the kernel's park.
        gex_kernel::BlockOp::SigWait { timeout, .. } => {
            Waiting::SigWait(timeout.map(|d| turn_now().saturating_add(d.as_nanos() as u64)))
        }
        // `wait4` (L25a), as the kernel's park: `WNOHANG` answers on the
        // first turn; anything else waits for the child, which may not have
        // started yet.
        gex_kernel::BlockOp::WaitChild { nohang, .. } => {
            if *nohang {
                Waiting::Runnable
            } else {
                Waiting::Child
            }
        }
        _ => unreachable!("a class call parks only on a stream, a poll, a futex, a sleep, a socket, a signal or a child"),
    };
    let write_blocked = matches!(op, gex_kernel::BlockOp::WriteStream { .. });
    let reissue = matches!(op, gex_kernel::BlockOp::AcceptWait { .. });
    // A new wait: whatever this Worker's thread was registered for is over.
    waitq::leave();
    let run = MY_RUN
        .with(|c| c.borrow().clone())
        .expect("a syscall from a Worker with no thread bound");
    {
        let mut r = run.lock().unwrap_or_else(|e| e.into_inner());
        r.pc = pc + gex_abi::SYSCALL_INST_BYTES;
        r.waiting = waiting;
        r.blocked_on = Some(op);
        r.owed = Owed { restore_sigmask, write_blocked, reissue };
    }
    waitq::begin_turn();
    // The compositor first, as the locked turn pumps it (`schedule`): this is
    // the turn a Wayland client's wait makes after it wrote a request, and
    // `schedule` skips the pump for a first turn made here. Without it the
    // request sat in the socket until some thread of the process next took a
    // full turn: a destroyed menu stayed drawn, and took clicks, for 2-10 s
    // (X's B2), and a lone client blocked on its first reply never got one
    // (tests/browser-wl-unmap.sh).
    pump_compositor();
    let pick = pick_mine(&run);
    FIRST_PICK.with(|c| *c.borrow_mut() = Some(pick));
    3
}

/// One turn of `schedule` for this Worker's thread: finish its parked call
/// if the wait is over, or register the wait and say why nothing runs. No
/// kernel lock: its run state, its signals and its wait slot are its own,
/// and what the call owes was taken from the kernel when it parked
/// (docs/kernel-locking.md §3.7). The first turn of a class call's park
/// ([`park_free`]) and every turn with no registered wait to ask.
fn pick_mine(run: &Arc<ThreadCell>) -> Pick {
    loop {
        let sig = sigs::ready();
        let mut r = run.lock().unwrap_or_else(|e| e.into_inner());
        let now = turn_now();
        if wait_ready(&r.waiting, &r.blocked_on, now, sig) {
            let (timed_out, by_signal) = how_ended(&r, now, sig);
            waitq::leave();
            IDLE_SPINS.store(0, std::sync::atomic::Ordering::Relaxed);
            return match finish_wait(&mut r, cur(), my_ctx(), my_tid(), timed_out, by_signal) {
                Done::At(c, pc) => Pick::Resumed(Some((c, pc))),
                Done::Ends(code) => {
                    drop(r);
                    exit_here(code, None);
                    Pick::Resumed(None)
                }
            };
        }
        // Its process ended under it (`exit_process` marks every thread).
        if matches!(r.waiting, Waiting::Exited) && !leader_waits() {
            return Pick::Over;
        }
        // A futex ends by itself (another Worker wakes it); one with no
        // deadline may be the last of the process's threads to park, which
        // the deadlock watch confirms (`all_futex_blocked`). A stream does
        // neither.
        let why = wait_why(&r);
        // A wait that ends by itself: a sleep (the clock), a `wait4` (the
        // child, in another Worker, exits), a futex (another Worker wakes
        // it), a signal's, an exited leader's (its process ends), a thread
        // running elsewhere.
        let ends = matches!(
            r.waiting,
            Waiting::Futex { .. }
                | Waiting::Pi { .. }
                | Waiting::Sleep(_)
                | Waiting::Child
                | Waiting::Signal
                | Waiting::SigWait(_)
                | Waiting::Exited
                | Waiting::Elsewhere
        );
        let untimed = matches!(r.waiting, Waiting::Futex { deadline: None, .. });
        let fresh = waitq::enter(cur(), &r, why, false, ends);
        drop(r);
        let all_blocked = why == 0 && untimed && all_futex_blocked();
        if all_blocked {
            waitq::note_all_blocked();
        }
        // Registered now, so a change from here on notifies: one before
        // that is seen by asking again.
        if fresh && {
            let sig = sigs::ready();
            let r = run.lock().unwrap_or_else(|e| e.into_inner());
            wait_ready(&r.waiting, &r.blocked_on, turn_now(), sig)
        } {
            continue;
        }
        return Pick::Idle(IdleLook { why, all_blocked, ends_by_itself: ends });
    }
}

/// A signal with no handler kills this *process*, not the run (`exit_process`),
/// its thread at `at`: said loudly, unless another process ended it on purpose.
fn killed(signal: i32, at: u64) -> i32 {
    let Some(home) = sigs::home() else { return exit_process(-signal) };
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
    // Who sent it, if the global kernel knows: "killed by signal 1"
    // is true of a hangup, a terminal's line discipline and any
    // guest's `kill`, and they are not the same bug.
    let mut sent = None;
    let from = if unsafe { k_present() } != 0 {
        let v = unsafe { k_last_kill() };
        let caller = (v >> 40) as i32;
        let target = ((v >> 8) & 0xffff_ffff) as i32;
        let signo = (v & 0xff) as u32;
        sent = Some((caller, target, signo));
        format!(
            " (signal #{}: {signo} from pid {caller} to {target})",
            unsafe { k_last_kill_seq() }
        )
    } else {
        String::new()
    };
    // Ended on purpose by another process (SIGINT, SIGKILL or
    // SIGTERM it sent this one): one quiet line, not the report.
    // apt ends every method this way after a clean run.
    let pid = home.pid();
    if matches!(sent, Some((caller, target, signo))
        if signo == signal as u32 && matches!(signo, 2 | 9 | 15)
            && caller != pid && (target == pid || target == -pid || target == 0))
    {
        trace_line(&format!("[gex] pid {pid} ended by signal {signal}{from}\n"));
    } else {
    warn_line(&format!(
        "[gex] pid {} killed by signal {}{from} at pc={at:#x}\n  stderr: {}\n  threads:{}\n  \
         last calls:{}\n  address space:{}\n  futexes:{}",
        pid,
        signal,
        last_stderr(),
        // Every thread, not only the one that died. Two threads
        // sharing a register context would clobber each other and
        // everything downstream of that looks like corruption
        // somewhere else entirely -- which is the shape #126 has.
        // Cheap to rule in or out, and never recorded before.
        home.table()
            .iter()
            .map(|(i, t)| {
                let r = t.run();
                format!(
                    " t{i}(tid={} ctx={:#x} pc={:#x} {})",
                    t.tid(),
                    t.ctx_addr(),
                    r.pc,
                    waiting_name(&r.waiting),
                )
            })
            .collect::<String>(),
        sys_recent(),
        mem_recent(),
        ftx_recent(),
    ));
    }
    exit_process(-signal)
}

/// End this Worker's process (`exit_process`), from outside the kernel: a
/// signal's default action, met by a thread delivering to itself, or a
/// halted machine. With `killed_at`, the thread's pc, a signal's death is
/// reported as a syscall's is ([`killed`]). The stop value (2).
fn exit_here(code: i32, killed_at: Option<u64>) -> i32 {
    match killed_at {
        Some(at) if code < 0 => killed(-code, at),
        _ => exit_process(code),
    }
}

/// How a parked call ended for its thread.
enum Done {
    /// It runs on, at this context and pc.
    At(u64, u64),
    /// A signal ends the process, with this status (`exit_process`'s).
    Ends(i32),
}

/// Finish the call thread `i` parked in, its wait over (`timed_out`,
/// `by_signal` as [`how_ended`] found them): its answer into its context
/// (`ctx_addr`), what else the call owes ([`Owed`]), and a signal to
/// deliver. Needs no kernel: `r` is the thread's own run state, locked, and
/// this Worker's signal binding is the thread's.
fn finish_wait(r: &mut Run, i: usize, ctx_addr: u64, tid: i32, timed_out: bool, by_signal: bool) -> Done {
    let was_futex = matches!(r.waiting, Waiting::Futex { .. });
    r.waiting = Waiting::Runnable;
    let op = r.blocked_on.take();
    let owed = std::mem::take(&mut r.owed);
    let pc = r.pc;
    if tracing() {
        trace_line(&format!("[gex] switch to thread {i} at pc={pc:#x} blocked={}\n", op.is_some()));
    }
    let mut gm = GuestMem::new(guest_bytes());
    let Some(op) = op else {
        // A futex thread arriving with nothing to finish must still receive
        // a legal result. Linux permits spurious futex wakes, and zero is
        // exactly that result; leaving x0 untouched hands glibc the uaddr
        // as an errno and makes it call futex_fatal_error.
        if was_futex {
            warn_line(&format!("[gex] pid {} recovered a spurious futex wake for thread {i}", my_pid()));
            let _ = gex_kernel::syscalls::set_result(&mut gm, ctx_addr, 0);
            ftx_note(!0, 0, 0, pc, tid);
        }
        return Done::At(ctx_addr, pc);
    };
    // A priority-inheritance lock's wait is over, however it ends: no
    // longer counted as waiting (`sys_futex`).
    if let gex_kernel::BlockOp::FutexLockPi { table, uaddr, .. } = &op {
        table.pi_done(*uaddr);
    }
    // A wait a signal ended that Linux would restart (signal(7)): the
    // handler runs, then the call again (`restart_call`).
    if by_signal && sigs::restarts(op.restart()) {
        let wasm32 = WASM32.load(std::sync::atomic::Ordering::Acquire);
        if let Some(back) = gex_kernel::syscalls::restart_call(&mut gm, ctx_addr, pc, wasm32) {
            match sigs::finish(&mut gm, ctx_addr, None, false) {
                // A wasm32 program's handler runs from the frame its own
                // code finds, and its sigreturn makes the call again.
                gex_kernel::signal::Delivery::Resume(_) if wasm32 => return Done::At(ctx_addr, pc),
                gex_kernel::signal::Delivery::Resume(target) => return Done::At(ctx_addr, target),
                gex_kernel::signal::Delivery::Terminate { signal, .. } => return Done::Ends(-(signal as i32)),
                gex_kernel::signal::Delivery::None if !wasm32 => return Done::At(ctx_addr, back),
                // `EINTR` after all (`restart_call`).
                gex_kernel::signal::Delivery::None => gex_kernel::syscalls::restart_undo(&mut gm, ctx_addr),
            }
        }
    }
    let res = finish_parked(&op, timed_out, by_signal, &mut gm);
    // The syscall again, from its instruction (the registers are as the
    // guest made the call): a priority-inheritance lock still held, a
    // stream another reader or writer got to first, or a call that does
    // its real work only once its wait is over (`Owed::reissue`: an
    // `accept`, a `recvmsg`, a transfer), unless a signal ended it.
    if res == Err(PI_RETRY) || (owed.reissue && res != Err(Errno::EINTR)) {
        return Done::At(ctx_addr, pc - gex_abi::SYSCALL_INST_BYTES);
    }
    // A parked call's answer never reaches the syscall ring (the
    // ring is written when the call is made), so an error here is
    // otherwise invisible -- and glibc aborts over a bad futex one.
    if let gex_kernel::BlockOp::FutexWait { uaddr, .. } = &op {
        ftx_note(
            !0,
            *uaddr,
            match &res {
                Ok(v) => *v as i64,
                Err(e) => -(e.0 as i64),
            },
            pc,
            tid,
        );
    }
    let sigpipe = owed.write_blocked && res == Err(Errno::EPIPE);
    let _ = gex_kernel::syscalls::set_result(&mut gm, ctx_addr, gex_kernel::errno::encode(res));
    match sigs::finish(&mut gm, ctx_addr, owed.restore_sigmask, sigpipe) {
        gex_kernel::signal::Delivery::None => Done::At(ctx_addr, pc),
        // A wasm32 program's handler runs from the frame its own code
        // finds; it carries on after the call.
        gex_kernel::signal::Delivery::Resume(_) if WASM32.load(std::sync::atomic::Ordering::Acquire) => {
            Done::At(ctx_addr, pc)
        }
        // Finishing a blocked syscall can *deliver a signal*, whose
        // frame says where to go; this is the only place that can
        // honour it, since the saved pc is after the `svc`, not the
        // handler. Dropping it broke every shell `fork`, where a
        // child's SIGCHLD must run a handler.
        gex_kernel::signal::Delivery::Resume(target) => {
            if tracing() {
                trace_line(&format!("[sig] resume at {target:#x}\n"));
            }
            Done::At(ctx_addr, target)
        }
        // One process, not the run: `exit_process` publishes the
        // status the parent's `wait4` collects.
        gex_kernel::signal::Delivery::Terminate { signal, .. } => Done::Ends(-(signal as i32)),
    }
}

/// What a parked syscall should return now that its thread is running again.
///
/// A woken futex returns zero and the guest re-checks its own condition, the
/// same as on Linux, where a wake is never a promise about the word.
/// `finish_parked`'s answer for a priority-inheritance lock still held: make
/// the syscall again (never seen by the guest).
const PI_RETRY: Errno = Errno(0x7fff);

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
        // Take the lock if it is free (the waiters bit stays set, so the
        // next unlock goes through the kernel); else `PI_RETRY`: the caller
        // makes the syscall again, which parks once more.
        gex_kernel::BlockOp::FutexLockPi { uaddr, tid, .. } => {
            use gex_kernel::futex::pi;
            loop {
                let v = gex_atomic::load_u32(gm.bytes, *uaddr).ok_or(Errno::EFAULT)?;
                match v & pi::TID_MASK {
                    0 => {
                        if gex_atomic::cas_u32(gm.bytes, *uaddr, v, *tid | pi::WAITERS).ok_or(Errno::EFAULT)?.is_ok() {
                            return Ok(0);
                        }
                        continue;
                    }
                    owner if owner == *tid => return Err(Errno::EDEADLK),
                    _ => {}
                }
                let want = v | pi::WAITERS;
                if v != want && gex_atomic::cas_u32(gm.bytes, *uaddr, v, want).ok_or(Errno::EFAULT)?.is_err() {
                    continue;
                }
                return Err(PI_RETRY);
            }
        }
        // The read the guest asked for happens now, into the buffer it named.
        // Doing it here rather than re-executing the instruction is what keeps
        // the guest's pc moving forward: a syscall that parks is finished by
        // the scheduler, never retried.
        gex_kernel::BlockOp::ReadStream { handle, vectors: Some(v), len, .. } => {
            // A `readv`: one read, scattered, so no other reader takes the
            // bytes between two vectors.
            let mut tmp = vec![0u8; *len];
            match handle.read_at(0, &mut tmp) {
                Err(Errno::EAGAIN) => Err(PI_RETRY),
                Ok(n) => gex_kernel::syscalls::scatter(gm, v, &tmp[..n as usize]).map(|()| n),
                r => r,
            }
        }
        gex_kernel::BlockOp::ReadStream { handle, buf, len, vectors: None } => {
            // Into the guest's buffer, with nothing in between.
            //
            // This is the read that parked, finished by the scheduler, and it
            // allocated the length the guest asked for -- up to 16 MiB, which
            // is exactly the arena's chunk, so each one made the allocator
            // claim a fresh span. Counting refills against the syscall in
            // flight put almost all of them in nr 63. `sys_read` had the same
            // shape and was fixed with it. See task 140.
            let dst = gm.read_mut(*buf, *len)?;
            match handle.read_at(0, dst) {
                // Another reader of the same pipe took what woke this one:
                // the syscall again, which parks once more.
                Err(Errno::EAGAIN) => Err(PI_RETRY),
                r => r,
            }
        }
        gex_kernel::BlockOp::WriteStream { handle, buf, len, gathered } => {
            // One turn's worth; the scheduler brings the thread back while
            // anything is left, because the descriptor stays unwritable until
            // the reader has drained.
            // Still no room (another writer got there first): the syscall
            // again, which parks once more, rather than EAGAIN on a
            // descriptor that is not non-blocking.
            let data = match gathered {
                Some(g) => g.to_vec(),
                None => gm.read(*buf, *len)?.to_vec(),
            };
            match handle.write_at(0, &data) {
                Err(Errno::EAGAIN) => Err(PI_RETRY),
                r => r,
            }
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
        // `rt_sigtimedwait`: one of its set pending is taken by the call
        // made again (with its `siginfo_t`); none, at the deadline, is
        // `EAGAIN`; none before it (another thread took it) waits again.
        gex_kernel::BlockOp::SigWait { set, thread, process, .. } => {
            if (thread.pending() | process.shared()) & set == 0 && timed_out {
                Err(Errno::EAGAIN)
            } else {
                Err(PI_RETRY)
            }
        }
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
    // The JIT state's lock is the one guard kept where a trap can reach it
    // (`JIT_HELD`, this Worker's TLS): a bug to be holding it here, said, and
    // given back by its own `Drop`, so that the lock, its rank and the held
    // count below agree. Left, the count was zeroed while the guard stayed.
    if jit_holds() {
        warn_line(&format!(
            "[gex] BUG: trapped out to JS holding the JIT lock, in syscall {}, taken at {}; given back (docs/kernel-locking.md §4.1)\n",
            LAST_SYSCALL.load(std::sync::atomic::Ordering::Relaxed) as i32,
            JIT_TAKER.with(|t| t.get()).map_or("?".to_string(), |l| format!("{}:{}", l.file(), l.line())),
        ));
        jit_give();
    }
    // A kernel lock guard alive here never drops: a trap does not unwind, so
    // the lock would stay taken for good (docs/kernel-locking.md §4.1). Lock
    // regions are leaf computations; one that reaches a trap is a bug.
    if let Err(n) = gex_kernel::sync::check_trap_out() {
        warn_line(&format!("[gex] trapped out to JS holding {n} kernel lock guard(s)\n"));
    }
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
    // Nor in a pass of the clock (`NowScope`), whose `Drop` this skips as
    // well: left set, every later clock question on this Worker got the
    // reading the trapping syscall took, and a timed `poll` or `epoll_wait`
    // whose stack went back to the page never reached its deadline.
    end_now_scopes();
    check_scopes_at_trap();
    warch::unreachable()
}

/// The shim's own scopes, checked where it traps out, as the kernel's lock
/// guards are (`check_trap_out`, docs/kernel-locking.md §4.1). None may be
/// open here: a trap runs no `Drop`, so each would stay open on this Worker
/// for good.
///
/// - the kernel: `KLock` in the shared build, which every other Worker
///   would then wait on, or the `HOST` borrow in the plain one, which would
///   make the next call panic as a re-entry;
/// - a GL submit's deferral (`gl::DEFER`), under which a submit waits for
///   the end of a syscall that has already gone.
///
/// Scopes that do outlive a trap are left alone: the ones JS sets back
/// when it catches one (`CB_DEPTH`, `GUEST_CALLS`, the call-out stack, the
/// NT layer's exception levels), and the ones a trap hands to the next
/// entry (`NATIVE_LEFT`, `NO_LEAVE`, the resume point). Those this shim
/// can end itself, `trap` ends.
fn check_scopes_at_trap() {
    #[cfg(feature = "shared-memory")]
    let kernel = DEPTH.with(|d| d.get()) > 0;
    #[cfg(not(feature = "shared-memory"))]
    let kernel = HOST.with(|h| h.try_borrow_mut().is_err());
    let defer = gl::DEFER.with(|d| d.get());
    if kernel || defer {
        warn_line(&format!(
            "[gex] BUG: trapped out to JS with{}{} still open, in syscall {} (docs/kernel-locking.md §4.1)\n",
            if kernel { " the kernel" } else { "" },
            if defer { " a GL deferral" } else { "" },
            LAST_SYSCALL.load(std::sync::atomic::Ordering::Relaxed) as i32,
        ));
    }
}

/// Which of the shim's own entry points is running, for a report that cannot
/// call anything to find out.
///
/// A forked child does its whole setup before it makes a single syscall, so
/// `LAST_SYSCALL` reads `-1` for all of it and says nothing about where a fault
/// or a huge allocation came from. This is the one thing that does. A plain
/// atomic set on entry, never cleared: the last one entered is the answer.
static PHASE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Name a phase by its code, for the reports that print one.
fn phase_name(n: u32) -> &'static str {
    match n {
        1 => "adopt_child",
        2 => "adopt_vma_blob",
        3 => "adopt_page",
        4 => "shared_seed",
        5 => "adopt_fds",
        6 => "reset_directory",
        7 => "set_metadata",
        8 => "alloc",
        9 => "adopt_thread",
        11 => "translate_runtime",
        12 => "translate_object",
        13 => "shared_modules_of_object",
        _ => "-",
    }
}

fn phase(n: u32) {
    PHASE.store(n, std::sync::atomic::Ordering::Relaxed);
}

/// The syscall number the guest is inside, or the last one it was inside.
///
/// A plain static, deliberately: it is read after a crash, when the kernel
/// may still be borrowed by whatever crashed, and anything that took the
/// kernel to answer would panic instead of answering.
static LAST_SYSCALL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(!0);

/// Whether the guest is *still* inside that syscall, rather than past it:
/// `LAST_SYSCALL` alone reads the same for a crash in a handler and a crash in
/// translated code after a harmless call. Cleared by dropping [`InSyscall`],
/// which a trap does not run, so a crash truly inside a handler leaves this set.
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
    let at = ctx_addr_get();
    if at == 0 {
        return 0;
    }
    GuestMem::new(guest_bytes())
        .read_u64(at + ctx::LAST_TARGET as u64)
        .unwrap_or(0) as i64
}

/// Set by [`trap`], cleared by [`gex_handoff`]. Per Worker: a shared static let
/// one Worker's trap flag be consumed by another's run loop.
thread_local! {
    static HANDOFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Did the shim mean to give the stack back? Clears the flag.
///
/// Not `with_exec_lock`: this is asked on the way out of a trap, which is exactly
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
    // An end claimed and still under way in another Worker: `exited` is set
    // at the claim and the status recorded last (`end_process_once`), so a
    // Worker that stopped on `exited` meanwhile would report 0 for it
    // (main-thread-exit under load). It is a few calls away: wait for it,
    // bounded, as the kernel lock used to make this wait.
    if let Some(home) = sigs::home() {
        if home.exit().over() && home.exit().status().is_none() {
            let until = now_ns() + 5_000_000_000;
            while home.exit().status().is_none() && now_ns() < until {
                std::hint::spin_loop();
            }
        }
    }
    exit_status().unwrap_or(0)
}

fn write_u64(data: &mut [u8], off: usize, v: u64) {
    data[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// Fused multiply-add, which wasm cannot express. Rust's `mul_add` is the
/// hardware instruction where there is one and libm's exact software fallback
/// where there is not, which is what the guest needs either way.
#[no_mangle]
pub extern "C" fn gex_fma(_ctx: i32, a: i64, b: i64, c: i64, double: i32) -> i64 {
    // Counted: wasm has no FMA, so each of these is a cross-module call in a
    // float inner loop, which can cost more than the arithmetic.
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

/// An x87 operation the inline fast path left to the host (see
/// `gex_abi::imports::X87`): the first result returned, an extended
/// result's sign and exponent left at `ctx::X87_TMP + 8` and the
/// status-word bits at `ctx::X87_TMP + 12`.
#[no_mangle]
pub extern "C" fn gex_x87(ctx: i32, op: i32, a_sig: i64, a_se: i32, b_sig: i64, b_se: i32, cw: i32) -> i64 {
    use gex_kernel::f80::{self, F80};
    let (a, b) = (F80 { sig: a_sig as u64, se: a_se as u16 }, F80 { sig: b_sig as u64, se: b_se as u16 });
    let cw = cw as u16;
    let (r, se, sw) = match op {
        1 | 2 => {
            let (r, sw) = f80::narrow_op(a.sig, a.se, cw, op == 1);
            (r, 0, sw)
        }
        _ if op & 0xf00 == 0x100 => {
            let (r, sw) = f80::to_int_op(a.sig, a.se, cw, ((op >> 1) & 0x7f) as u8, op & 1 != 0);
            (r, 0, sw)
        }
        _ if op & 0xf00 == 0x200 => {
            let (r, sw) = f80::arith_op((op & 3) as u8, a, b, cw);
            (r.sig, r.se as u32, sw)
        }
        _ if op & 0xf00 == 0x300 => {
            let (cc, sw) = f80::compare_op(a, b, op & 1 != 0);
            (cc as u64, 0, sw)
        }
        _ => panic!("x87 import: unknown operation {op:#x}"),
    };
    let mut gm = GuestMem::new(guest_bytes());
    let base = ctx as u32 as u64 + ctx::X87_TMP as u64;
    let _ = gm.write_u32(base + 8, se);
    let _ = gm.write_u32(base + 12, sw);
    r as i64
}

/// How many times the guest has asked for a fused multiply-add.
pub static FMA_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Read and reset the count, for a harness that wants it per phase.
#[no_mangle]
pub extern "C" fn gex_fma_calls() -> i64 {
    FMA_CALLS.swap(0, std::sync::atomic::Ordering::Relaxed) as i64
}

/// NTSTATUS codes a guest fault hook is given (shims/native's `fault_code`).
mod fault_code {
    pub const ACCESS_VIOLATION: u32 = 0xc000_0005;
    pub const ILLEGAL_INSTRUCTION: u32 = 0xc000_001d;
    pub const BREAKPOINT: u32 = 0x8000_0003;
    pub const INT_DIVIDE_BY_ZERO: u32 = 0xc000_0094;
    pub const INT_OVERFLOW: u32 = 0xc000_0095;
}

/// A fault for the process's guest fault hook (Wine's ntdll.so), as
/// shims/native's `deliver_guest_fault`: the context's PC set to `pc` (where
/// the hook sees the fault, after an int3 for a breakpoint), the fault
/// staged for the host (`gex_fault_take`), and out to it. Returns only
/// when the process has no hook.
fn hook_fault(pc: u64, code: u32, addr: u64) {
    if cgen::fault_hook() == 0 {
        return;
    }
    // No lock: the context is this Worker's thread's own (`my_ctx`).
    let mut gm = GuestMem::new(guest_bytes());
    let c = my_ctx();
    let _ = gm.write_u64(c + ctx::PC as u64, pc);
    cgen::fault_stage(code, addr, 0);
    set_resume(pc);
    set_resume_ctx(c);
    trap();
}

#[no_mangle]
pub extern "C" fn gex_illegal(_ctx: i32, pc: i64, raw: i32) {
    // Whole-image units mark a fault they can go on after
    // (`gex_cgen::RESUMABLE_PC`); the browser still traps out of them.
    let pc = fault_pc((pc as u64 & !gex_cgen::RESUMABLE_PC) as i64);
    if nt::active() {
        nt::fault(_ctx, gex_kernel::nt::Fault::Illegal, pc as u64);
    }
    // A process with a guest fault hook (Wine's ntdll.so): the hook takes
    // the illegal instruction, as shims/native's `deliver_guest_fault` does.
    // 64-bit native Wine's system and Unix calls are a ud2 at a fixed
    // address (signal_wasm64.c's W64_SYSCALL_ENTRY), which the hook runs and
    // steps over.
    hook_fault(pc as u64, fault_code::ILLEGAL_INSTRUCTION, pc as u64);
    // Which process and what is mapped there: with a Worker per process the
    // console interleaves them all.
    let vma = named_mm()
        .and_then(|mm| {
            mm.lock()
                .vmas()
                .iter()
                .find(|v| (pc as u64) >= v.start && (pc as u64) < v.end)
                .map(|v| format!("{:#x}-{:#x} {}", v.start, v.end, v.name))
        })
        .unwrap_or_else(|| "unmapped".into());
    trace_line(&format!(
        "gex: pid {}: illegal instruction {:#010x} at pc={:#x} ({vma})\n",
        my_pid(),
        raw as u32,
        pc
    ));
    set_exit(128 + 4);
    trap();
}
/// `log2` of the guest bytes one dispatch slot spans (`gex_abi::DISPATCH_SHIFT`).
/// The glue's slot arithmetic asks rather than assumes, so one `gex.mjs`
/// serves both shims.
#[no_mangle]
pub extern "C" fn gex_inst_align_shift() -> i32 {
    gex_abi::DISPATCH_SHIFT as i32
}

/// The per-block register trace (`GEX_TRACE_BLOCKS` natively): every block
/// translated after this call starts by reporting its pc and registers to the
/// debug console. Enormous; pipe it to `tail`. The one tool that shows what ran
/// between two syscalls when the guest lands somewhere wild.
/// Whether this process is a 32-bit (i386) one, for the JS side to carry to
/// the Worker a fork child or a new thread runs in: each Worker has its own
/// shim instance, whose decoder mode starts out 64-bit.
#[no_mangle]
pub extern "C" fn gex_compat() -> i32 {
    compat_abi() as i32
}

/// The other half: said in the child's or thread's Worker once its kernel
/// state has been adopted, before any block is translated there.
#[no_mangle]
pub extern "C" fn gex_set_compat(on: i32) {
    // Once per process, at a child's adoption in its own Worker, before any
    // of its threads run: the process is this Worker's alone, and its
    // fields are written directly (L25c). A thread's Worker never calls it
    // (what it sets is shared).
    with_own_host(|h| {
        h.kernel.compat = on != 0;
        // A child's process starts now.
        if h.kernel.started == 0 {
            h.kernel.started = now_ns() as u128;
        }
        clock_constants(h.kernel.compat, h.kernel.started);
    });
    // An adopted child's stack (`gex_adopt_child`, before this) went to
    // `ctx::SP`; an i386-ABI process's is `esp` (`ctx::sp_slot`), which on
    // an AArch64 build is another slot (its i386-ABI processes are wasm32
    // programs: posix_spawn's child starts on that stack).
    if on != 0 && !gex_abi::X86 && !gex_abi::PPC {
        compat_stack(my_ctx());
    }
    gex_xlate::set_compat(on != 0);
}

/// An adopted i386-ABI child's stack, from `ctx::SP` (where its adoption
/// put it) to `esp` (`ctx::sp_slot`), which on an AArch64 build is another
/// slot: its context at `c`, its own.
pub(crate) fn compat_stack(c: u64) {
    let mut gm = GuestMem::new(guest_bytes());
    let sp = gm.read_u64(c + ctx::SP as u64).unwrap_or(0);
    if sp != 0 {
        let _ = gm.write_u64(c + ctx::sp_slot(true) as u64, sp);
        let _ = gm.write_u64(c + ctx::SP as u64, 0);
    }
}

#[no_mangle]
pub extern "C" fn gex_set_trace_blocks(on: i32) {
    gex_xlate::set_trace_blocks(on != 0);
}

#[no_mangle]
pub extern "C" fn gex_breakpoint(_ctx: i32, pc: i64, raw: i32) {
    if raw == gex_abi::imports::BRK_KUSER_UNFOLDED {
        kuser_unfolded(pc);
    }
    // As in `gex_illegal`: the browser traps out of a resumable fault too.
    let pc = fault_pc((pc as u64 & !gex_cgen::RESUMABLE_PC) as i64);
    // Native Wine's return pad (its int3 filler), translated by an earlier
    // stray return, reached past the call-outs waiting: as the miss path.
    if cgen::native_return_stray(pc as u64, my_ctx()) {
        set_resume(pc as u64);
        set_resume_ctx(my_ctx());
        trap();
    }
    if raw == gex_abi::imports::BRK_TRACE {
        let line = (|| {
            let gm = GuestMem::new(guest_bytes());
            let c = my_ctx();
            let r = |i: u8| gm.read_u64(c + ctx::x(i) as u64).unwrap_or(0);
            if gex_abi::X86 {
                format!(
                    "[trace] t{} pc={:#x} rax={:#x} rcx={:#x} rdx={:#x} rbx={:#x} rsp={:#x} rbp={:#x} rsi={:#x} rdi={:#x} r8={:#x} r9={:#x} r10={:#x} r11={:#x} r12={:#x} r13={:#x} r14={:#x} r15={:#x}\n",
                    cur(), pc as u64,
                    r(0), r(1), r(2), r(3), r(4), r(5), r(6), r(7),
                    r(8), r(9), r(10), r(11), r(12), r(13), r(14), r(15)
                )
            } else {
                let sp = gm.read_u64(c + ctx::SP as u64).unwrap_or(0);
                format!(
                    "[trace] t{} pc={:#x} x0={:#x} x1={:#x} x2={:#x} x3={:#x} x8={:#x} x19={:#x} x20={:#x} x29={:#x} x30={:#x} sp={sp:#x}\n",
                    cur(), pc as u64,
                    r(0), r(1), r(2), r(3), r(8), r(19), r(20), r(29), r(30)
                )
            }
        })();
        trace_line(&line);
        return;
    }
    // GEX_NT_TRAP_PCS: noted in the thread's ring, silently.
    #[cfg(feature = "shared-memory")]
    if nt::active() && raw == gex_abi::imports::BRK_TRAP_PC {
        nt::trap_note(_ctx as u32 as u64, pc as u64);
        return;
    }
    // In a Windows process every other breakpoint is a processor exception
    // the program may handle (`int3`, #DE, `hlt`).
    if nt::active() && raw != gex_abi::imports::BRK_TRAP_PC {
        nt::fault(_ctx, gex_kernel::nt::Fault::from_brk(raw), pc as u64);
    }
    // A process with a guest fault hook (Wine's ntdll.so) handles its own
    // processor exceptions, as natively (shims/native's brk import): a
    // divide error, a general-protection fault, and int3 (seen after the
    // instruction, as hardware leaves it). A diagnostic breakpoint, a bad
    // stack pointer and an unmasked x87 exception are gex's own.
    {
        use gex_abi::imports as i;
        let pc = pc as u64;
        if raw == i::BRK_DIVIDE_BY_ZERO {
            hook_fault(pc, fault_code::INT_DIVIDE_BY_ZERO, 0);
        } else if raw == i::BRK_DIVIDE_OVERFLOW {
            hook_fault(pc, fault_code::INT_OVERFLOW, 0);
        } else if raw == i::BRK_GENERAL_PROTECTION {
            hook_fault(pc, fault_code::ACCESS_VIOLATION, 0xffff_ffff);
        } else if raw != i::BRK_TRAP_PC && raw != i::BRK_BAD_SP && raw != i::BRK_X87_EXCEPTION {
            hook_fault(pc + 1, fault_code::BREAKPOINT, pc);
        }
    }
    // A diagnostic breakpoint planted at a chosen guest pc: dump the live
    // registers (x30 names the caller) and RETURN so the block runs on. Pointed
    // at __stack_chk_fail, it deterministically names the smashed function --
    // the one thing the downstream abort cannot recover. First few hits only.
    if raw == gex_abi::imports::BRK_TRAP_PC {
        static mut HITS: u64 = 0;
        let n = unsafe {
            HITS = HITS.wrapping_add(1);
            HITS
        };
        {
            let (line, unusual) = (|| {
                let Some(home) = sigs::home() else { return Default::default() };
                let gm = GuestMem::new(guest_bytes());
                let c = my_ctx();
                let r = |i: u8| gm.read_u64(c + ctx::x(i) as u64).unwrap_or(0);
                let sp = gm.read_u64(c + ctx::SP as u64).unwrap_or(0);
                let x30 = r(30);
                let object = |a: u64| {
                    home.mm().lock()
                        .vmas()
                        .iter()
                        .find(|v| a >= v.start && a < v.end)
                        .map_or(String::new(), |v| format!("{} +{:#x}", v.name, a - v.start))
                };
                (format!(
                    "gex: TRAP_PC #{n} tid={} at pc={:#x}: x30={x30:#x} ({}) sp={sp:#x} \
                     x0={:#x} x1={:#x} x19={:#x} x20={:#x}\n",
                    my_tid(),
                    pc as u64,
                    object(x30),
                    r(0),
                    r(1),
                    r(19),
                    r(20),
                ), r(1) > 0xff)
            })();
            // The first hits establish the normal call shape. Keep reporting
            // a wildly non-ABI-looking x1 afterward: that is precisely how a
            // wrong function target hides behind thousands of valid lock
            // entries before the one malformed call.
            if n <= 24 || unusual {
                warn_line(&line);
            }
        }
        return;
    }
    // A divide error (`Stmt::Trap`): x86 #DE, SIGFPE on Linux. This shim
    // does not deliver synchronous faults to a guest handler (as with
    // SIGILL); the process dies of the signal.
    if raw == gex_abi::imports::BRK_DIVIDE_BY_ZERO || raw == gex_abi::imports::BRK_DIVIDE_OVERFLOW {
        trace_line(&format!(
            "gex: integer divide {} at pc={:#x} (SIGFPE)\n",
            if raw == gex_abi::imports::BRK_DIVIDE_BY_ZERO { "by zero" } else { "overflow" },
            pc
        ));
        set_exit(128 + 8);
        trap();
    }
    // A general-protection fault (a privileged instruction in user mode):
    // SIGSEGV on Linux, fatal here.
    if raw == gex_abi::imports::BRK_GENERAL_PROTECTION {
        trace_line(&format!("gex: general protection fault at pc={pc:#x} (SIGSEGV)\n"));
        set_exit(128 + 11);
        trap();
    }
    // An x87 exception the control word unmasks: gex does not deliver
    // them, so the process dies of SIGFPE, saying so.
    if raw == gex_abi::imports::BRK_X87_EXCEPTION {
        trace_line(&format!(
            "gex: unmasked x87 floating-point exception at pc={pc:#x}; gex does not deliver x87 exceptions (SIGFPE)\n"
        ));
        set_exit(128 + 8);
        trap();
    }
    // The stack-pointer check borrows this import rather than adding one, so an
    // older shim still links. Names the object, since the block that *writes* a
    // bad `sp` is elsewhere from the one that dies on it.
    if raw == gex_abi::imports::BRK_BAD_SP {
        let line = (|| {
            let Some(home) = sigs::home() else { return Default::default() };
            let gm = GuestMem::new(guest_bytes());
            let c = my_ctx();
            let sp = gm.read_u64(c + ctx::SP as u64).unwrap_or(0);
            let x30 = gm.read_u64(c + ctx::x(30) as u64).unwrap_or(0);
            // `x30` is the link register, so it names whoever called into
            // this block -- which, for a block reached by `ret`, is the last
            // place the guest was somewhere sane.
            let object = |a: u64| {
                home.mm().lock()
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
        })();
        trace_line(&line);
        set_exit(128 + 11);
        trap();
    }
    // A bare `brk` is almost always glibc's `abort()` (the trap it ends on when
    // `raise(SIGABRT)` returns) or a compiler `__builtin_trap`. Name where it
    // was reached from and walk the stack a little, so a crash with no printed
    // message (a corrupted call, a fortify failure) still says which library
    // and which caller -- otherwise "breakpoint at pc" names only glibc's abort.
    let line = (|| {
        let Some(home) = sigs::home() else { return Default::default() };
        let gm = GuestMem::new(guest_bytes());
        let c = my_ctx();
        let sp = gm.read_u64(c + ctx::SP as u64).unwrap_or(0);
        let x30 = gm.read_u64(c + ctx::x(30) as u64).unwrap_or(0);
        let object = |a: u64| {
            home.mm().lock()
                .vmas()
                .iter()
                .find(|v| a >= v.start && a < v.end)
                .map_or(String::new(), |v| format!("{} +{:#x}", v.name, a - v.start))
        };
        // Stack words that look like a return address into some library's text:
        // the caller of `abort` is saved on its frame, so this recovers it even
        // though `x30` has been clobbered by the calls `abort` itself made.
        let mut bt = String::new();
        for i in 0..48u64 {
            let w = gm.read_u64(sp + i * 8).unwrap_or(0);
            let o = object(w);
            if o.starts_with("/") || o.contains(".so") {
                bt.push_str(&format!("\n  [sp+{:#x}] {w:#x} {o}", i * 8));
            }
        }
        format!(
            "gex: breakpoint (abort/trap) at pc={pc:#x} {}, x30={x30:#x} {}, sp={sp:#x}{bt}\n",
            object(pc as u64),
            object(x30),
        )
    })();
    trace_line(&line);
    set_exit(128 + 5);
    trap();
}
#[no_mangle]
pub extern "C" fn gex_sysreg_read(_ctx: i32, _pc: i64, reg: i32) -> i64 {
    let _pc = fault_pc(_pc);
    match reg {
        // CNTVCT_EL0, at the frequency the lifter tells the guest CNTFRQ is,
        // so the guest's own arithmetic on it comes out exact.
        -3 => host_now_ns() as i64,
        // MIDR_EL1: a plausible Cortex-A72, the same one the native host
        // claims to be.
        -7 => 0x410f_d083,
        _ => 0,
    }
}
#[no_mangle]
pub extern "C" fn gex_sysreg_write(ctx: i32, pc: i64, reg: i32, val: i64) {
    let pc = fault_pc(pc);
    // A gex-eject library replacement (`Intrinsic::Replace`). This shim has no
    // replacement modules to run it with (eject v0 runs them natively only),
    // and returning would carry on as if the replaced function had run.
    if reg == gex_xlate::intrinsic::REPLACE_SYSREG {
        panic!(
            "gex: replacement #{val} (the function at {:#x}) cannot run in the browser shim; \
             run the ejected project natively",
            pc as u64
        );
    }
    // PowerPC: a non-default FPSCR rounding mode, which wasm arithmetic cannot
    // honour, and mffs under GEX_FP_STRICT=1 (docs/ppc.md §8). Both fail
    // loudly, as an illegal instruction would.
    if gex_abi::PPC
        && ((reg == gex_kernel::ppc::SYSREG_FPSCR_RN && val != 0)
            || reg == gex_kernel::ppc::SYSREG_MFFS_STRICT)
    {
        trace_line(&format!(
            "gex: {} at pc={pc:#x} (FPSCR={val:#x}); gex computes round-to-nearest only and no FP exception flags\n",
            if reg == gex_kernel::ppc::SYSREG_MFFS_STRICT { "mffs under GEX_FP_STRICT" } else { "non-default FP rounding mode" },
        ));
        set_exit(128 + 4);
        trap();
    }
    // The x86-64 lifter's `cpuid`: the leaf and subleaf arrive in `val`, the
    // four results go back into the context.
    if reg == gex_kernel::x86::SYSREG_CPUID {
        let mut gm = GuestMem::new(guest_bytes());
        gex_kernel::x86::serve_cpuid(&mut gm, ctx as u32 as u64, val as u64);
    }
    // An x87 transcendental or remainder, computed on the register stack in
    // the context.
    if reg == gex_kernel::x86::SYSREG_X87 {
        let mut gm = GuestMem::new(guest_bytes());
        gex_kernel::x86::serve_x87(&mut gm, ctx as u32 as u64, val as u64);
    }
    // `cmpxchg16b`, under the host's lock.
    if reg == gex_kernel::x86::SYSREG_CMPXCHG16B {
        if (val as u64) < gex_abi::GUEST_FLOOR {
            low_access("a cmpxchg16b", val as u64, ctx, pc as u64);
        }
        let mut gm = GuestMem::new(guest_bytes());
        gex_kernel::x86::serve_cmpxchg16b(&mut gm, ctx as u32 as u64, val as u64);
    }
}

/// A guest access into the runtime's own low memory (below
/// `gex_abi::GUEST_FLOOR`): where hardware would fault, gex would write into
/// its contexts or dispatch directory. Said, then raised: an access
/// violation through SEH for a Windows program, SIGSEGV otherwise.
fn low_access(what: &str, addr: u64, ctx: i32, pc: u64) -> ! {
    let gm = GuestMem::new(guest_bytes());
    let c = ctx as u32 as u64;
    let r: Vec<String> = (0..16u8).map(|i| format!("{:#x}", gm.read_u64(c + ctx::x(i) as u64).unwrap_or(0))).collect();
    warn_line(&format!(
        "gex: {what} at {addr:#x}, in the runtime's low memory (a null pointer's field?), in block {}: an access violation. regs [{}]\n",
        nt::describe(pc),
        r.join(" ")
    ));
    if nt::active() {
        nt::fault(ctx, gex_kernel::nt::Fault::LowWrite(addr), pc);
    }
    set_exit(128 + 11);
    trap();
}

/// Cache maintenance. Only `dc zva` does anything, and it must: the lifter
/// reports a 64-byte block through `DCZID_EL0` and musl's `memset` zeroes whole
/// blocks with it, so a no-op leaves large `memset`s silently un-zeroed.
/// How many times self-modifying guest code has forced a page re-lift. See
/// `gex_cache_op`'s `IC_INVALIDATE` handling.
static SMC_INVALIDATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Directory entries `heal_dirs` wrote (the NT stats line).
pub(crate) static DIRS_HEALED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Of those, the ones in a unit's install window (`cgen::in_install_window`),
/// which are not a bug.
pub(crate) static DIRS_HEALED_EARLY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// For a block at `start` that failed its page check: its page and the next
/// (an instruction across the boundary), wherever the page table numbers the
/// page and the directory does not name that base, get the directory entry
/// the page table says. Every heal outside a unit's install window is said:
/// since units are published once (`cgen::CLAIMED`) and list every page they
/// cover (`cgen::bridges`), none should happen, so a heal line is a bug
/// report (something numbered a page and left its entry unwritten).
fn heal_dirs(start: u64) {
    let first = gex_abi::dispatch_page(start);
    let healed: Vec<(u64, u32, u32)> = with_jit_only(|j| {
        let mut gm = GuestMem::new(guest_bytes());
        let mut out = Vec::new();
        for page in [first, first + 1] {
            if page >= gex_abi::DISPATCH_MAX_PAGES {
                continue;
            }
            let Some(base) = j.page_table.lookup(page) else { continue };
            let at = gex_abi::DISPATCH_DIR + page * 4;
            let dir = gm.read_u32(at).unwrap_or(u32::MAX);
            if dir != base {
                dense::note_entry(base);
                let _ = gm.write_u32(at, base);
                out.push((page, dir, base));
            }
        }
        out
    });
    for (page, was, base) in healed {
        // A unit being published while another thread already runs its code
        // (two threads loading one DLL): the claimant's install writes this
        // entry shortly. Counted, not said.
        if was == 0 && cgen::in_install_window(page) {
            DIRS_HEALED_EARLY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            continue;
        }
        let n = DIRS_HEALED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        warn_line(&format!(
            "gex: heal: pid {} pc {start:#x} page {page:#x}: directory entry {was:#x}, the page table numbers it {base:#x}; entry written (heal {n}; a bug: something numbered the page and left its entry unwritten)\n",
            my_pid()
        ));
    }
}

#[no_mangle]
pub extern "C" fn gex_cache_op(_ctx: i32, _pc: i64, op: i32, addr: i64) {
    let _pc = fault_pc(_pc);
    if op == gex_abi::cache_op::LOW_ACCESS {
        low_access("a locked access", addr as u64, _ctx, _pc as u64);
    }
    if op == gex_abi::cache_op::SPLIT_LOCK {
        let gm = GuestMem::new(guest_bytes());
        let c = _ctx as u32 as u64;
        let r: Vec<String> = (0..16u8).map(|i| format!("{:#x}", gm.read_u64(c + ctx::x(i) as u64).unwrap_or(0))).collect();
        warn_line(&format!(
            "gex: a locked access at {:#x} (block {}) spans two 8-byte words; such accesses run under one global lock, atomic against each other but not against plain accesses; regs [{}]\n",
            addr as u64,
            nt::describe(_pc as u64),
            r.join(" ")
        ));
        return;
    }
    if op == gex_abi::cache_op::PAGE_STALE {
        // A block failed its page check (gex-emit `emit_page_check`):
        // translate its address again if its module still provides it.
        let start = _pc as u64;
        // The same stale block asking twice in a row: the catch-up below did
        // not move this Worker's directory off it, so the newer translation
        // it waits for is never coming here (seen in an exec'd aarch64
        // program's `_dl_fini`, which re-entered the block from JavaScript
        // for ever). Translating it again is always correct, only slower.
        let repeat = STALE_LAST.with(|c| {
            let same = c.get() == (start, addr as u64);
            c.set((start, addr as u64));
            same
        });
        // A page the block runs onto (or starts on) that the page table
        // numbers but whose directory entry says otherwise: a numbering
        // whose entry was never written (the publisher's install had not
        // come, or never did). Every translation of the block checks the
        // page table's base, so retranslating alone never converges; write
        // the entry (its slots not filled here yet only miss and catch up).
        // Paint's startup stall: uiribbon's function at 0x604eefff,
        // translated about 155 times a second for minutes.
        heal_dirs(start);
        let again = with_jit_only(|j| {
            let again = repeat || j.provider.get(&start).map_or(true, |&m| m == addr as u64);
            if again {
                j.translated.remove(&start);
                j.provider.remove(&start);
            }
            again
        });
        if again {
            gex_dispatch_miss(_ctx, start as i64);
        } else {
            // The newer translation is another Worker's, maybe not in this
            // Worker's tables yet; the block would dispatch to itself again,
            // and ask again, for ever. Resume at it from JavaScript, which
            // catches up first (the block start is a clean point: the page
            // check runs before anything else in it).
            set_resume(start);
            set_resume_ctx(_ctx as u32 as u64);
            trap();
        }
        return;
    }
    if op == gex_abi::cache_op::FTAB_TIER1 || op == gex_abi::cache_op::SLOT_CONFLICT {
        // The dispatch miss that follows ignores its answer.
        NO_LEAVE.with(|c| c.set(true));
    }
    if op == gex_abi::cache_op::FTAB_TIER1 {
        // The bridge left for this very address having run nothing: tier-1
        // takes the slot over, and the function JIT serves it no more.
        fjit::tier1_only(addr as u64);
        with_jit_only(|j| {
            j.translated.remove(&(addr as u64));
        });
        return;
    }
    if op == gex_abi::cache_op::SLOT_CONFLICT {
        // A block entered for a neighbour sharing its slot: forget the
        // neighbour so the miss that follows lifts it (with this block as a
        // mate) into a trampoline. See `gex_abi::DISPATCH_SHIFT`.
        // `nt::slot_log()`: the first few hundred, named.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if nt::slot_log() && N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 400 {
            warn_line(&format!(
                "gex: slot conflict: block {:#x} entered for {:#x}\n",
                _pc as u64, addr as u64
            ));
        }
        with_jit_only(|j| {
            j.translated.remove(&(addr as u64));
        });
        return;
    }
    if op == gex_abi::cache_op::IC_INVALIDATE {
        // Self-modifying / JIT-generated guest code (PCRE2's regex JIT does this)
        // wrote new machine code to a page it had already run, and is flushing
        // the icache before executing it. gex's translation of that page is now
        // STALE -- running it executes the OLD code, which for PCRE2 smashed
        // `pcre2_jit_match_16`'s stack canary. Invalidate: drop the page->slot
        // mapping so the next miss RE-LIFTS the new bytes into a FRESH slot
        // (reusing the old slot would leave the directory naming one block and
        // the table holding another -- an illegal instruction, see the miss
        // path), clear the directory word so the next branch misses, and drop
        // the page's block records. The old slots leak, which is harmless: they
        // are never reused and, once the directory is cleared, unreachable.
        let page = gex_abi::dispatch_page(addr as u64);
        with_jit_only(|j| {
            if j.page_table.forget(page).is_none() {
                return;
            }
            j.retired_pages.insert(page);
            let mut gm = GuestMem::new(guest_bytes());
            let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, 0);
            let lo = page * gex_abi::DISPATCH_PAGE;
            let hi = lo + gex_abi::DISPATCH_PAGE;
            j.translated.retain(|&a| a < lo || a >= hi);
            xlate_add(3, 1);
            let n = SMC_INVALIDATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            // Informational: a JIT (V8 under node) rewrites its code all the
            // time and this is the runtime keeping up, not a fault.
            if n <= 8 || tracing() {
                trace_line(&format!(
                    "gex: self-modifying-code: re-lifting page {page:#x} \
                     (addr={:#x}, {n} so far)\n",
                    addr as u64,
                ));
            }
        });
        return;
    }
    if op != gex_abi::cache_op::DC_ZVA {
        // Clean and barrier are no-ops: the memory is coherent.
        return;
    }
    let block = gex_abi::DCZID_BLOCK_BYTES as usize;
    let start = (addr as u64 & !(gex_abi::DCZID_BLOCK_BYTES - 1)) as usize;
    let mem = guest_bytes();
    if let Some(dst) = mem.get_mut(start..start + block) {
        dst.fill(0);
    }
}
/// A poll on a loop back-edge: the point where the guest's registers are
/// flushed to its context and the pc is the branch target, so the stack can be
/// handed back, a pending signal delivered, and the heartbeat sampled. (Thread
/// preemption is the engine's job now; see the end of this function.)
#[no_mangle]
pub extern "C" fn gex_safepoint(ctx: i32, pc: i64) {
    fjit::watch_check(pc as u64, "safepoint");
    // A Windows process's clocks in KUSER_SHARED_DATA, which a guest polls
    // without a syscall: a loop that spins on them passes here.
    nt::tick();
    // A stateful provider's collection stops guest threads here (its
    // registers are in the context at a safepoint), and a thread that came
    // back from a syscall through JS is running guest code again.
    if cgen::libprov::any_registered() {
        let tid = my_tid() as u32;
        cgen::libprov::guest_again(tid);
    }
    // A loop that has left an exception dispatched nested (nt::exc_escape).
    if nt::exc_escape(ctx, pc as u64) {
        set_resume_none();
        trap();
    }
    // A thread another thread's `execve` took the process from, still
    // running the old program's code (a loop that makes no syscall): it goes
    // here, as on Linux it would at once.
    if thread_gone() {
        stop_gone();
        trap();
    }
    // Rearm: translated code counts a word down to zero and calls here; leaving
    // it at zero makes every later back-edge call out.
    {
        let mut gm = GuestMem::new(guest_bytes());
        let _ = gm.write_u32(
            ctx as u32 as u64 + ctx::SAFEPOINT_COUNTER as u64,
            SAFEPOINT_INTERVAL,
        );
    }
    // Catch a worker running on a stray SP that reaches into the MAIN thread's
    // stack -- a store there smashes a canary the quiet main thread detects much
    // later, which is the complex-Qt (designer) stack smash. Cheap (one guest
    // read at safepoint cadence), loud, off unless asked. A non-main thread whose
    // SP is inside [STACK_TOP-STACK_SIZE, STACK_TOP) is the smoking gun.
    if SP_GUARD.load(std::sync::atomic::Ordering::Relaxed) {
        let ctxa = ctx as u32 as u64;
        if ctxa != gex_abi::ctx_addr(0) {
            let gm = GuestMem::new(guest_bytes());
            let sp = gm.read_u64(ctxa + ctx::SP as u64).unwrap_or(0);
            let main_lo = gex_abi::STACK_TOP - gex_abi::STACK_SIZE;
            if sp >= main_lo && sp < gex_abi::STACK_TOP {
                static mut HITS: u64 = 0;
                let n = unsafe {
                    HITS = HITS.wrapping_add(1);
                    HITS
                };
                if n <= 16 {
                    warn_line(&format!(
                        "gex: SP-GUARD hit #{n}: worker ctx={ctxa:#x} SP={sp:#x} is in MAIN \
                         stack [{main_lo:#x},{:#x}) at pc={:#x}\n",
                        gex_abi::STACK_TOP,
                        pc as u64,
                    ));
                }
            }
        }
    }
    // A heartbeat when tracing: nothing else runs while the guest executes
    // translated code, so back-edges are the only place to say where it is and
    // tell a long stretch of execution from a hang.
    let every = unsafe { HEARTBEAT };
    // Counting is its own switch, not a side effect of the heartbeat: "say
    // where the guest is every N safepoints" (a stuck run) and "which back
    // edges are hot" (a slow one) are different questions.
    if every != 0 || unsafe { HOT_ON } {
        static mut BEAT: u64 = 0;
        let beat = unsafe {
            BEAT = BEAT.wrapping_add(1);
            BEAT
        };
        // Where the guest's back edges are, counted over every safepoint: a
        // single heartbeat sample prints whichever pc was current, which for a
        // loop is the same few addresses and says nothing about the work.
        unsafe {
            let slot = (pc as u64 as usize >> gex_abi::DISPATCH_SHIFT) % HOT_SLOTS;
            if HOT_PC[slot] == pc as u64 {
                HOT_N[slot] += 1;
            } else if HOT_N[slot] == 0 {
                HOT_PC[slot] = pc as u64;
                HOT_N[slot] = 1;
            } else {
                // Someone else's slot: age it rather than fight, so a hot
                // address wins within a few thousand samples.
                HOT_N[slot] -= 1;
            }
        }
        if every != 0 && beat.is_multiple_of(every) {
            // With the threads (a spin says nothing alone) and the three
            // hottest back edges symbolised: a page cannot run `gex_hot_blocks`
            // after exit, so the beat carries it and a slow run names its object.
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
            // The threads and the map without the kernel lock (L23), the
            // block count from the JIT state alone.
            let blocks = with_jit_only(|j| j.blocks);
            let (census, whose) = sigs::home().map(|home| {
                let mut out = String::new();
                for (i, t) in home.table().iter() {
                    let r = t.run();
                    out.push_str(&format!(" {i}={}", waiting_name(&r.waiting)));
                }
                let name = |addr: u64| {
                    home.mm()
                        .lock()
                        .vmas()
                        .iter()
                        .find(|v| addr >= v.start && addr < v.end)
                        .map(|v| format!("{}+{:#x}", v.name, addr - v.start))
                        .unwrap_or_else(|| format!("{addr:#x}"))
                };
                for (n, addr) in &top {
                    out.push_str(&format!(" hot[{}]={n}", name(*addr)));
                }
                // And which object the pc is in: the library and offset turn a
                // bare spinning address into a disassembly.
                let whose = home
                    .mm()
                    .lock()
                    .vmas()
                    .iter()
                    .find(|v| (pc as u64) >= v.start && (pc as u64) < v.end)
                    .map(|v| format!("{}+{:#x}", v.name, pc as u64 - v.start))
                    .unwrap_or_default();
                (out, whose)
            }).unwrap_or_default();
            trace_line(&format!(
                "[beat] {beat} safepoints, pc={:#x} ({whose}), {blocks} blocks, \
                 threads:{census}\n",
                pc as u64
            ));
        }
    }
    // A signal, if one is pending: without this a signal was only noticed
    // leaving a syscall, so a loop with no syscall (`while :; do :; done`)
    // could not be `^C`'d.
    // A signal, asked about and delivered without the kernel lock (`sigs`):
    // this Worker's own thread's pending word and mask, its process's
    // shared word, dispositions and timers, and the inbox other processes
    // fill, collected when anyone has sent anything since this Worker last
    // looked.
    if SAFEPOINT_SIGNALS && sigs::ready() {
        let ctx_addr = ctx as u32 as u64;
        let mut gm = GuestMem::new(guest_bytes());
        // The context pc must be exact before a handler frame is built on
        // it: the frame records where to return, and a branch stores no pc.
        let _ = gm.write_u64(ctx_addr + ctx::PC as u64, pc as u64);
        let d = sigs::deliver(&mut gm, ctx_addr);
        // Which signals a process actually takes, capped. A signal that
        // should never have been sent looks like a program misbehaving:
        // xterm tearing down a live session, a shell exiting for no reason.
        // Where it came from is in `k_last_kill`.
        if !matches!(d, gex_kernel::signal::Delivery::None) {
            use std::sync::atomic::{AtomicU32, Ordering};
            static SAID: AtomicU32 = AtomicU32::new(0);
            if SAID.fetch_add(1, Ordering::Relaxed) < 32 {
                let last = if unsafe { k_present() } != 0 {
                    let v = unsafe { k_last_kill() };
                    format!(
                        " (last sent: {} from pid {} to {})",
                        v & 0xff,
                        (v >> 40) as i32,
                        ((v >> 8) & 0xffff_ffff) as i32
                    )
                } else {
                    String::new()
                };
                warn_line(&format!("[sig] pid {} takes a signal{last}\n", my_pid()));
            }
        }
        match d {
            gex_kernel::signal::Delivery::None => {}
            // The thread runs on at the handler: its own run state and
            // resume, so no kernel.
            gex_kernel::signal::Delivery::Resume(target) => {
                if let Some(run) = MY_RUN.with(|c| c.borrow().clone()) {
                    let mut r = run.lock().unwrap_or_else(|e| e.into_inner());
                    r.pc = target;
                    r.waiting = Waiting::Runnable;
                }
                set_resume(target);
                set_resume_ctx(my_ctx());
                trap();
            }
            // The process ends, which is the kernel's.
            gex_kernel::signal::Delivery::Terminate { signal, .. } => {
                exit_here(-(signal as i32), None);
                trap();
            }
        }
    }
    // Preemption is the engine's job now: there is no other thread here to hand
    // the stack to, since each is a Worker already running.
}

/// A dispatch miss: translate the block at `pc`, and everything reachable
/// from it, into a module for JS to instantiate.
///
/// Lets a page ship a small module: only what the entry point reaches is
/// translated ahead of time, and the rest arrives here as needed. The shim
/// lifts and encodes but cannot compile, so it leaves the module behind and
/// traps out for JS to instantiate before resuming at the address that missed.
/// A pc that comes to translation again and again is a retranslation
/// livelock (S3 found natively that a page retired between a translation and
/// its publish came back without its directory entry): say what dispatch
/// sees there, once per pc, the 64th time.
thread_local! {
    /// Set by `cache_op` SLOT_CONFLICT / FTAB_TIER1, whose caller drops the
    /// next dispatch miss's answer: that miss must not answer
    /// `DISPATCH_MISS_LEAVE` (the caller would dispatch again, for ever).
    static NO_LEAVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The last `PAGE_STALE` (block, module), to tell a stale block that
    /// keeps coming back from one met once. See `gex_cache_op`.
    static STALE_LAST: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

/// Dispatch misses by how `gex_dispatch_miss` dealt with them (the NT
/// stats line): all, a unit catch-up, a call-out's return, a dense block
/// to instantiate, the function JIT, a published unit pc, translated.
pub(crate) static MISSES: [std::sync::atomic::AtomicU64; 7] = [const { std::sync::atomic::AtomicU64::new(0) }; 7];

fn miss_count(k: usize) {
    MISSES[k].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

fn remiss(pc: u64) {
    // Every miss that reaches translation counts, whether or not `translated`
    // still has the pc: a stale page check (`PAGE_STALE`) and the bridge's
    // hand-over (`FTAB_TIER1`) clear it before they come here, and a pc
    // translated again and again that way was never counted (Paint's
    // startup stall at 0x604eefff, scratch/P/stall85).
    let said = with_jit_only(|j| {
        let n = j.remiss.entry(pc).or_insert(0);
        *n += 1;
        if *n != 64 {
            return None;
        }
        let page = pc / gex_abi::DISPATCH_PAGE;
        let dir = GuestMem::new(guest_bytes()).read_u32(gex_abi::DISPATCH_DIR + page * 4).unwrap_or(u32::MAX);
        Some(format!(
            "gex: {pc:#x} came to translation 64 times: page {page:#x} directory entry {dir:#x}, page table {:?}, retired {}, {} translations so far\n",
            j.page_table.lookup(page),
            j.retired_pages.contains(&page),
            j.remiss.values().sum::<u32>()
        ))
    });
    if let Some(s) = said {
        warn_line(&s);
    }
}

/// Misses at one pc this many times within [`LIVELOCK_WINDOW`] misses of a
/// Worker: a dispatch that does not converge. Said loudly, once per pc, with
/// what dispatch sees there and how the miss path last dealt with it (a
/// diagnostic: nothing here changes what the miss does). A Paint startup
/// spun for minutes re-translating one AOT entry (0x604eefff, uiribbon;
/// scratch/P/stall85), and no line said so.
const LIVELOCK_MISSES: u32 = 256;
const LIVELOCK_WINDOW: u32 = 8192;

/// Pcs the livelock check has warned about (the NT stats line).
pub(crate) static LIVELOCKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    /// Misses in this window, and per pc in it.
    static LIVE_WINDOW: std::cell::RefCell<(u32, std::collections::HashMap<u64, u32>)> =
        std::cell::RefCell::new((0, std::collections::HashMap::new()));
    /// The pcs warned about already.
    static LIVE_SAID: std::cell::RefCell<std::collections::HashSet<u64>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
    /// How the miss path last dealt with a miss, per pc, for the warning:
    /// (pc, way, how many times running).
    static MISS_WAY: std::cell::Cell<(u64, u8, u32)> = const { std::cell::Cell::new((0, 0, 0)) };
}

/// The ways out of `gex_dispatch_miss`, for the livelock warning.
const MISS_WAYS: [&str; 14] = [
    "none yet", "unit catch-up (behind)", "exception escape", "call-out return", "dense catch-up",
    "(unused)", "function JIT", "provider re-bridge", "W3 relocated image", "?", "closure fill",
    "AOT catch-up (ftab knows it)", "recompile", "tier-1 translation",
];

fn miss_way(pc: u64, way: u8) {
    MISS_WAY.with(|c| {
        let (p, w, n) = c.get();
        c.set((pc, way, if p == pc && w == way { n.saturating_add(1) } else { 1 }));
    });
}

fn livelock_note(pc: u64) {
    let n = LIVE_WINDOW.with(|w| {
        let mut w = w.borrow_mut();
        w.0 += 1;
        if w.0 > LIVELOCK_WINDOW {
            w.0 = 1;
            w.1.clear();
        }
        let n = w.1.entry(pc).or_insert(0);
        *n += 1;
        *n
    });
    if n != LIVELOCK_MISSES || !LIVE_SAID.with(|s| s.borrow_mut().insert(pc)) {
        return;
    }
    LIVELOCKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (wpc, way, wn) = MISS_WAY.with(|c| c.get());
    let way = if wpc == pc { format!("{} ({wn} times running)", MISS_WAYS.get(way as usize).unwrap_or(&"?")) } else { format!("last miss here was not dealt with; the last one was at {wpc:#x}") };
    let mut pages = Vec::new();
    for page in [gex_abi::dispatch_page(pc), gex_abi::dispatch_page(pc) + 1] {
        let dir = GuestMem::new(guest_bytes()).read_u32(gex_abi::DISPATCH_DIR + page * 4).unwrap_or(u32::MAX);
        let (pt, retired) = with_jit_only(|j| (j.page_table.lookup(page), j.retired_pages.contains(&page)));
        pages.push(format!("page {page:#x}: directory {dir:#x}, page table {pt:x?}, retired {retired}"));
    }
    let (translated, remiss) = with_jit_only(|j| (j.translated.contains(&pc), j.remiss.get(&pc).copied().unwrap_or(0)));
    let line = format!(
        "gex: LIVELOCK? pid {} missed dispatch at {pc:#x} {LIVELOCK_MISSES} times in {LIVELOCK_WINDOW} misses ({}); \
         the miss path: {way}; {}; dense index {}; function table: knows {}, index {:?}, unit serves {}, behind {}; \
         translated {translated}, translations of it {remiss}; aot {}\n",
        my_pid(),
        object_off_at(pc),
        pages.join("; "),
        dense::lookup(pc),
        cgen::knows(pc),
        cgen::index_of(pc),
        cgen::unit_serves(pc),
        cgen::behind(),
        aot(),
    );
    warn_line(&line);
}

/// The pc of the most recent dispatch miss, for the host's install check.
// Per Worker: set on the way into a trap or a check and read back by the
// same Worker (see `CTX_ADDR` for what a process-wide one did).
thread_local! {
    static LAST_MISS_PC: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A guest address to watch (`GEX_WATCH_MEM`): its 8 bytes are re-read at every
/// syscall and dispatch miss, and a change is reported with what ran between.
/// For "who wrote this word": a GOT slot that turns into code bytes, say.
static WATCH_MEM: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static WATCH_LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[no_mangle]
pub extern "C" fn gex_set_watch_mem(addr: i64) {
    WATCH_MEM.store(addr as u64, std::sync::atomic::Ordering::Relaxed);
    WATCH_LAST.store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
}

fn watch_mem(where_: &str) {
    let addr = WATCH_MEM.load(std::sync::atomic::Ordering::Relaxed);
    if addr == 0 {
        return;
    }
    let now = GuestMem::new(guest_bytes()).read_u64(addr).unwrap_or(u64::MAX - 1);
    let last = WATCH_LAST.swap(now, std::sync::atomic::Ordering::Relaxed);
    if now != last {
        warn_line(&format!(
            "[watch] pid {} [{addr:#x}] = {now:#x} (was {last:#x}) at {where_}, last syscall {}\n",
            my_pid(),
            LAST_SYSCALL.load(std::sync::atomic::Ordering::Relaxed),
        ));
    }
}

#[no_mangle]
pub extern "C" fn gex_miss_pc() -> i64 {
    LAST_MISS_PC.with(|c| c.get()) as i64
}

/// This process's pid, from the static every syscall refreshes.
#[no_mangle]
pub extern "C" fn gex_my_pid() -> i32 {
    my_pid()
}

#[no_mangle]
pub extern "C" fn gex_dispatch_miss(_ctx: i32, pc: i64) -> i32 {
    let pc = pc as u64;
    // A thread another thread's `execve` took the process from, which got
    // here before a safepoint or syscall stopped it: the code it was running
    // is gone, and the new program's memory is under this address. It goes,
    // as at a safepoint, rather than translating whatever is there now.
    if thread_gone() {
        stop_gone();
        trap();
    }
    LAST_MISS_PC.with(|c| c.set(pc));
    watch_mem("miss");
    miss_count(0);
    livelock_note(pc);
    // A miss a slot conflict or the bridge asks for (they drop the answer
    // and dispatch again): never left by returning.
    let no_leave = NO_LEAVE.with(|c| c.replace(false));
    // A function-tier unit another Worker published that this one has not
    // installed yet: the miss is a catch-up, done in JS, not a translation.
    if cgen::behind() {
        miss_way(pc, 1);
        miss_count(1);
        // AOT-only: catch up inside the call and dispatch again, keeping the
        // thread's frames (leaving would resume at continuations no unit
        // has entries for).
        if aot() {
            nt::serve_now();
            return gex_emit::module::DISPATCH_MISS_REDISPATCH;
        }
        set_resume(pc);
        set_resume_ctx(my_ctx());
        trap();
    }
    // Control leaving an exception dispatched nested (nt::exc_escape).
    if nt::exc_escape(_ctx, pc) {
        miss_way(pc, 2);
        set_resume_none();
        trap();
    }
    // The return a call-out of this Worker waits for, untranslated: that
    // call-out is done (cgen::returned; JS's nested loop ends it).
    if cgen::returned(pc, _ctx as u32 as u64) {
        miss_way(pc, 3);
        miss_count(2);
        if !no_leave && cgen::may_leave() {
            return gex_emit::module::DISPATCH_MISS_LEAVE;
        }
        set_resume(pc);
        set_resume_ctx(my_ctx());
        trap();
    }
    // Native Wine's return pad, reached past the call-outs waiting: they
    // leave (`cgen::native_return_stray`), and it is not translated.
    if cgen::native_return_stray(pc, my_ctx()) {
        miss_way(pc, 3);
        set_resume(pc);
        set_resume_ctx(my_ctx());
        trap();
    }
    // A block this process has (the slot names it in `blocks`) that this
    // Worker has not instantiated: JavaScript catches up, then the dispatch
    // is retried (`dense::missing`).
    if dense::missing(pc) {
        miss_way(pc, 4);
        miss_count(3);
        dense::want(pc);
        if in_place(_ctx, pc) {
            return gex_emit::module::DISPATCH_MISS_REDISPATCH;
        }
        if unsafe { TRACE_MISSES } {
            trace_line(&format!("[gex] dense miss at pc={pc:#x}: index {} not in this Worker's blocks\n", dense::lookup(pc)));
        }
        if FILL_POSSIBLE.load(std::sync::atomic::Ordering::Relaxed) {
            // A module inherited lazily is instantiated by the fill path.
            let page = gex_abi::dispatch_page(pc);
            let width = FILL_WIDTH.load(std::sync::atomic::Ordering::Relaxed);
            FILL_FROM.with(|c| c.set(((pc & 0xfff) >> gex_abi::DISPATCH_SHIFT) as u32 & !(width - 1)));
            FILL_PC.with(|c| c.set(pc));
            FILL_WANTED.with(|c| c.set(page as i64));
            FILL_PENDING.with(|c| c.set(page as i64));
        }
        set_resume(pc);
        set_resume_ctx(my_ctx());
        trap();
    }
    // An ejected project's training run: a pc of a unit image no function
    // holds is a web entry, whether the function JIT (below) or tier-1 then
    // runs it (the AOT-only run would stop here).
    if cgen::entry_log() && !cgen::knows(pc) {
        crate::cgen::note_web_entry(pc);
    }
    // A function serves it, or the function JIT makes one: tier-1 reaches
    // it through the bridge once this Worker has installed it.
    if fjit::serve(pc) {
        miss_way(pc, 6);
        miss_count(4);
        if in_place(_ctx, pc) {
            return gex_emit::module::DISPATCH_MISS_REDISPATCH;
        }
        set_resume(pc);
        set_resume_ctx(my_ctx());
        trap();
    }
    // An address an ELF export provider (or a replaced library's provider)
    // holds whose slot lost its bridge.
    if cgen::elfprov::rebridge(pc) {
        miss_way(pc, 7);
        set_resume(pc);
        set_resume_ctx(my_ctx());
        trap();
    }
    if cgen::libprov::replaced(pc) {
        cgen::libprov::note_stray(pc);
    }
    // W3: the first miss in an image Wine relocated after mapping it, whose
    // unit was left until now (cgen.rs `w3_miss`): JS serves it, then the
    // guest goes on at this pc.
    if cgen::w3_miss(pc) {
        miss_way(pc, 8);
        set_resume(pc);
        set_resume_ctx(my_ctx());
        trap();
    }
    remiss(pc);
    miss_way(pc, 13);
    if tracing() || unsafe { TRACE_MISSES } {
        // Before the translation: the question is which block the translator
        // did not come back from, and a line printed after never appears.
        trace_line(&format!("[gex] miss at pc={pc:#x}\n"));
    }
    // The other half of the heartbeat: a safepoint beat says nothing while the
    // host is lifting and the engine compiling (most of the time for something
    // Mesa-sized), so without this there is no telling translating from wedged.
    if unsafe { HEARTBEAT } != 0 {
        static mut MISSES: u64 = 0;
        let n = unsafe {
            MISSES = MISSES.wrapping_add(1);
            MISSES
        };
        if n.is_multiple_of(200) {
            let (blocks, ms, intr) = with_jit_only(|j| {
                (j.blocks, j.nanos / 1_000_000, j.intrinsics.len())
            });
            trace_line(&format!(
                "[beat] miss {n} at pc={pc:#x}, {blocks} blocks, {ms}ms lifting, \
                 {intr} intrinsics\n"
            ));
        }
    }
    // A signal handler returning: with no `sa_restorer` the kernel puts
    // `SIGRETURN_TRAMPOLINE` in x30, an address with nothing translated at it,
    // so the handler's `ret` arrives here -- the whole mechanism.
    // No kernel lock: the thread's signal state and run state are its own.
    if pc == gex_kernel::signal::SIGRETURN_TRAMPOLINE {
        let mut gm = GuestMem::new(guest_bytes());
        let ctx_addr = my_ctx();
        match sigs::sigreturn(&mut gm, ctx_addr) {
            Ok(target) => {
                if tracing() {
                    trace_line(&format!("[sig] sigreturn to {target:#x}\n"));
                }
                if let Some(run) = MY_RUN.with(|c| c.borrow().clone()) {
                    run.lock().unwrap_or_else(|e| e.into_inner()).pc = target;
                }
                set_resume(target);
                set_resume_ctx(ctx_addr);
            }
            Err(_) => {
                warn_line("gex: corrupt signal frame on sigreturn\n");
                set_exit(128 + 11);
            }
        }
        trap();
    }
    // A branch to memory nothing executable is mapped at: a bad return
    // address or function pointer. Said before any slot arithmetic, because a
    // wild `pc` makes a wild directory index, and a shim trap on that reads as
    // a runtime bug where the guest's own crash is the story.
    let wild = gex_abi::X86 && !sigs::executable(pc);
    if wild && nt::active() {
        nt::fault(_ctx, gex_kernel::nt::Fault::NotExecutable, pc);
    }
    // A process with a guest fault hook (Wine's ntdll.so, W3): an access
    // violation, executing, handed to the hook by the host, which resumes
    // where the hook leaves the context (shims/native's
    // `deliver_guest_fault_kind`). Without it a W3 program's own null call
    // (Windows 7's comctl32 calls LpkEditControl's table, null without
    // lpk.dll, and expects the exception to be caught) ended the thread.
    if wild && cgen::fault_hook() != 0 {
        (|| {
            let mut gm = GuestMem::new(guest_bytes());
            let c = my_ctx();
            let _ = gm.write_u64(c + ctx::PC as u64, pc);
            cgen::fault_stage(0xc000_0005, pc, 8);
            set_resume(pc);
            set_resume_ctx(c);
        })();
        trap();
    }
    if wild {
        (|| {
            let Some(home) = sigs::home() else { return Default::default() };
            let gm = GuestMem::new(guest_bytes());
            let c = my_ctx();
            let last = gm.read_u64(c + ctx::LAST_TARGET as u64).unwrap_or(0);
            let src = gm.read_u64(c + ctx::LAST_SOURCE as u64).unwrap_or(0);
            let sp = gm.read_u64(c + ctx::SP as u64).unwrap_or(0);
            let stack: Vec<String> = (0..8)
                .map(|i| format!("{:#x}", gm.read_u64(sp + i * 8).unwrap_or(0)))
                .collect();
            // The stack around sp, named, and the registers (a bad return's frame).
            let around: Vec<String> = (0..24u64)
                .map(|i| {
                    let a = sp.wrapping_sub(0x60).wrapping_add(i * 8);
                    let v = gm.read_u64(a).unwrap_or(0);
                    format!("{a:#x}={v:#x}{}", Some(object_off_at(v)).filter(|n| !n.is_empty()).map(|n| format!(" ({n})")).unwrap_or_default())
                })
                .collect();
            let regs: Vec<String> = (0..16u8).map(|r| format!("r{r}={:#x}", gm.read_u64(c + ctx::x(r) as u64).unwrap_or(0))).collect();
            warn_line(&format!("[gex] wild branch: stack {}; regs {}\n", around.join(", "), regs.join(" ")));
            warn_line(&format!(
                "[gex] pid {} thread {} branched to {pc:#x}, which is not executable guest memory (SIGSEGV); last indirect target {last:#x} ({}) from block {src:#x} ({}), sp={sp:#x} stack=[{}], last syscall {}\n",
                home.pid(),
                cur(),
                object_off_at(last),
                object_off_at(src),
                stack.join(", "),
                LAST_SYSCALL.load(std::sync::atomic::Ordering::Relaxed),
            ));
            // A return a call-out should have caught: the call-outs open, and
            // the callback depth this Worker is at.
            let open = cgen::callouts_open();
            if !open.is_empty() || cgen::libprov::provider_return(pc) {
                warn_line(&format!(
                    "[gex] call-outs open: {:x?} (return, sp wanted, callback depth); sp now {sp:#x}; last provider guest calls (target, return, sp): {:x?}\n",
                    open,
                    cgen::provider_calls_recent()
                ));
                warn_line(&format!("[gex] last lookups (address, index): {:x?}\n", cgen::lookups_recent()));
                warn_line(&format!(
                    "[gex] last call-outs (target, return, sp wanted, depth, result, sp after): {:x?}; provider guest call open {}\n",
                    cgen::callouts_recent(),
                    cgen::libprov::guest_call_open()
                ));
                warn_line(&format!(
                    "[gex] last resumes set (lib.rs line, pc): {:x?}\n",
                    RESUME_RING.with(|r| r.borrow().iter().map(|&(l, v)| (l, v)).collect::<Vec<_>>())
                ));
            }
            set_exit(128 + 11);
        })();
        trap();
    }
    // Which of this object's functions the backend serves itself, before either
    // path translates anything.
    note_intrinsics(pc);
    // First, though: does a module this process already holds cover this page
    // with its slots unwritten? Filling one page is microseconds against
    // seconds to recompile the object around it, so this must sit above the
    // recompile check below. Once per page: an unfillable page falls through.
    if FILL_POSSIBLE.load(std::sync::atomic::Ordering::Relaxed) {
        // Once per *span*, not per page: a fill writes `FILL_SPAN` slots around
        // the one that missed, so a branch elsewhere on the page must ask again,
        // while a second ask for one span would loop for ever.
        let page = gex_abi::dispatch_page(pc);
        let width = FILL_WIDTH.load(std::sync::atomic::Ordering::Relaxed);
        let from = ((pc & 0xfff) >> gex_abi::DISPATCH_SHIFT) as u32 & !(width - 1);
        let spans = gex_abi::SLOTS_PER_PAGE as u32 / width;
        let key = page * spans as u64 + (from / width) as u64;
        // Under the fill's own lock (`FILL_ASKED`): two threads missing at
        // once (the main thread and a dbus/gsettings worker, both faulting in
        // glib/gobject closure pages) ask together.
        let fresh = FILL_ASKED.lock().asked.insert(key);
        if fresh {
            miss_way(pc, 10);
            FILL_FROM.with(|c| c.set(from));
            FILL_PC.with(|c| c.set(pc));
            FILL_WANTED.with(|c| c.set(page as i64));
            FILL_PENDING.with(|c| c.set(page as i64));
            set_resume(pc);
            set_resume_ctx(my_ctx());
            trap();
        }
    }
    // An ejected project's training run (`run.mjs --record-entries`): code
    // inside an image with a unit, entered where the unit has no entry, is
    // what the web build's region entries must cover.
    crate::cgen::note_web_entry(pc as u64);
    // AOT-only, a pc the function table has (published by another Worker
    // after this one last caught up): catch up and dispatch again.
    if aot() && cgen::knows(pc) {
        thread_local! {
            static AGAIN: std::cell::Cell<(u64, u32)> = const { std::cell::Cell::new((0, 0)) };
        }
        let n = AGAIN.with(|a| {
            let (p, n) = a.get();
            let n = if p == pc { n + 1 } else { 1 };
            a.set((pc, n));
            n
        });
        if n <= 3 {
            miss_way(pc, 11);
            miss_count(5);
            nt::serve_now();
            return gex_emit::module::DISPATCH_MISS_REDISPATCH;
        }
    }
    // An ejected project without a JIT fallback (`gex_set_jit(0)`): nothing
    // is translated at run time, so code the build never saw stops the run
    // here, saying so, rather than being lifted.
    if !JIT_ON.load(std::sync::atomic::Ordering::Relaxed) {
        (|| {
            let Some(home) = sigs::home() else { return Default::default() };
            let stop = format!(
                "gex-eject: code at {pc:#x} ({}) was not translated when this project was built: \
                 the recorded session never reached it. Record a session that reaches it and \
                 re-eject, or build with the JIT fallback\n",
                object_off_at(pc)
            );
            warn_line(&stop);
            // And on the guest's stderr: under Node a Worker's console output
            // can be lost when the run ends here.
            unsafe { write_stderr(&stop) };
            // A resume after the C nest unwound (a blocking call's
            // continuation) needs region entries for every frame still on
            // the guest stack, not only this one: name the likely ones
            // (stack words just after a call instruction in an image) so a
            // single run lists what user/web-entries.txt needs.
            let gm = GuestMem::new(guest_bytes());
            let sp = gm.read_u64(my_ctx() + ctx::SP as u64).unwrap_or(0);
            let mut cands = Vec::new();
            for i in 0..4096u64 {
                let Ok(v) = gm.read_u32(sp + 4 * i) else { break };
                let v = v as u64;
                let in_image = home.mm().lock().vmas().iter().any(|m| m.prot & gex_kernel::mem::prot::EXEC != 0 && !m.name.is_empty() && v >= m.start + 6 && v < m.end);
                if !in_image || cands.contains(&v) {
                    continue;
                }
                let Ok(b) = gm.read(v - 6, 6) else { continue };
                // e8 rel32; ff /2 with modrm (reg, [reg], disp8, disp32 / abs).
                let call = b[1] == 0xe8
                    || (b[4] == 0xff && (b[5] & 0x38) == 0x10 && b[5] >> 6 != 1 && b[5] >> 6 != 2)
                    || (b[3] == 0xff && (b[4] & 0x38) == 0x10 && b[4] >> 6 == 1)
                    || (b[0] == 0xff && (b[1] & 0x38) == 0x10 && (b[1] >> 6 == 2 || b[1] & 0xc7 == 0x05));
                if call {
                    cands.push(v);
                }
            }
            let list: Vec<String> = cands.iter().map(|v| format!("{v:#x} ({})", object_off_at(*v))).collect();
            // In lines of 16: a page's console logger may cut long lines.
            warn_line(&format!("gex-eject: {} return addresses on the stack from esp {sp:#x} (web entry candidates):\n", list.len()));
            for chunk in list.chunks(16) {
                warn_line(&format!("gex-eject:   {}\n", chunk.join(" ")));
            }
            // The frame-pointer chain: the frames really live (MSVC and
            // Wine code keep EBP frames), unlike stale stack words. A frame
            // without EBP is skipped over, not named; the scan above has it.
            let mut ebp = gm.read_u64(my_ctx() + ctx::x(5) as u64).unwrap_or(0) & 0xffff_ffff;
            let mut chain = Vec::new();
            for _ in 0..256 {
                if ebp < sp || ebp & 3 != 0 || ebp - sp > 8 << 20 {
                    break;
                }
                let (Ok(next), Ok(ra)) = (gm.read_u32(ebp), gm.read_u32(ebp + 4)) else { break };
                chain.push(format!("{:#x} ({})", ra, object_off_at(ra as u64)));
                if (next as u64) <= ebp {
                    break;
                }
                ebp = next as u64;
            }
            warn_line(&format!("gex-eject: {} frames on the EBP chain (live web entry candidates):\n", chain.len()));
            for chunk in chain.chunks(16) {
                warn_line(&format!("gex-eject:   {}\n", chunk.join(" ")));
            }
            set_exit(128 + 11);
        })();
        trap();
    }
    miss_count(6);
    // Past the fill and a closure library still claims this page: everything
    // looks right (pin took, module covers it, directory names it) yet the
    // block is lifted, so count how much the closure fails to save.
    if gex_closure_owner(gex_abi::dispatch_page(pc)) >= 0 {
        let n = with_jit_only(|j| {
            j.lift_in_closure += 1;
            j.lift_in_closure
        });
        if n <= 8 || (tracing() && n % 1000 == 0) {
            trace_line(&format!(
                "[closure] page unfilled: lifting {pc:#x}, page {} is claimed ({n} so far)\n",
                gex_abi::dispatch_page(pc)
            ));
        }
    }
    // Why this miss is about to translate, counted as it does:
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
            let from = ((pc & 0xfff) >> gex_abi::DISPATCH_SHIFT) as u32 & !(width - 1);
            let spans = gex_abi::SLOTS_PER_PAGE as u32 / width;
            let key = page * spans as u64 + (from / width) as u64;
            if FILL_ASKED.lock().asked.contains(&key) { 2 } else { 3 }
        };
        LIFT_WHY[why].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Keep the addresses of `why == 2` (a span the fill already answered):
        // whether the slot is written now decides between a dispatch fault and
        // a block-address disagreement, and only JS can read the table, so
        // record and let the Worker look afterwards.
        if why == 2 {
            let n = LIFT_PC_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as usize;
            if n < LIFT_PCS.len() {
                LIFT_PCS[n].store(pc, std::sync::atomic::Ordering::Relaxed);
                // And the base *now*: a forked child runs its parent's code
                // until it execs, so a probe reading at exit asks about the
                // wrong program.
                LIFT_BASES[n].store(
                    gex_page_base(gex_abi::dispatch_page(pc) as u32),
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
        }
    }
    if maybe_recompile_at(pc) {
        miss_way(pc, 12);
        // The same resume the ordinary path uses: come back at the address
        // that missed, once JavaScript has instantiated what was queued and
        // the address resolves.
        set_resume(pc);
        set_resume_ctx(my_ctx());
        trap();
    }
    // Three parts: what to translate, under the lock; the translation, with
    // the lock given back and the JIT state out (`jitout`); and its install,
    // under the lock again. A sibling's syscalls no longer wait for a lift.
    enum MissPre {
        Done(bool),
        Go {
            exec: Vec<(u64, u64)>,
            known: std::collections::HashSet<u64>,
            table: gex_xlate::PageTable,
            intr: std::collections::HashMap<u64, gex_xlate::Intrinsic>,
            page: u64,
            started: u64,
            epoch: (u32, u32),
        },
    }
    let pre = with_px(|px| with_jit_only(|j| {
        if !j.described {
            return MissPre::Done(false);
        }
        let exec: Vec<(u64, u64)> = px
            .mm
            .lock()
            .vmas()
            .iter()
            .filter(|v| v.prot & gex_kernel::mem::prot::EXEC != 0)
            .map(|v| (v.start, v.end))
            .collect();
        if !exec.iter().any(|(a, b)| pc >= *a && pc + gex_abi::INST_ALIGN <= *b) {
            return MissPre::Done(false);
        }
        // Nothing can be lifted at a misaligned pc (the emitter asserts a
        // block's base is instruction-aligned, and a panic there takes the
        // whole runtime down). On arm64 it is a PC alignment fault; here
        // the process ends as for code that is not there. A provider's
        // IFUNC resolver+1 used to get here once the re-bridge guard gave
        // up on it (see `ftab::rebridge_gives_up`).
        if pc % gex_abi::INST_ALIGN != 0 {
            crate::warn_line(&format!("gex: pid {} jumped to a misaligned pc {pc:#x}; not translating it\n", px.pid));
            return MissPre::Done(false);
        }
        // Before anything is translated: if this page's code has been
        // translated already, by another process or by this one before an
        // `execve`, point the page at those slots instead of claiming more.
        // The page table is keyed by dispatch page, which is an index; the
        // identity is asked about an address, because that is what names a
        // mapping.
        let page = gex_abi::dispatch_page(pc);
        if j.page_table.lookup(page).is_some() {
            j.reuse_have += 1;
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
                    // The held form: this runs under the JIT lock already.
                    let owner = closure_owner_held(px, page);
                    warn_line(&format!(
                        "[gex] miss pid={} addr={addr:#x} page={page:#x} \
                         below_interp={below} in_image_pages={in_images} \
                         owner={}\n",
                        px.pid,
                        owner,
                    ));
                }
            }
            let ident = page_identity(px, page * gex_abi::DISPATCH_PAGE);
            if ident.is_none() {
                j.reuse_noident += 1;
            }
            if let Some(key) = ident {
                if shared_slot(j, page, key).is_some() {
                    j.reuse_hit += 1;
                } else {
                    j.reuse_miss += 1;
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
                // All of it, when the object has been translated whole.
                //
                // The cap exists because a stored unit holds only what was
                // reachable in the process that built it, so most do not contain
                // the wanted address and taking hundreds is compiles for
                // nothing. Once the object is complete the cap drops: everything
                // is covered, so taking the lot ends the object's misses.
                let whole = obj_finished(px, page);
                // A byte budget either way. "Take the lot" was a count with no
                // size behind it, and the lot is however many megabytes of wasm
                // somebody stored: a finished object served 317 MiB in one miss.
                // What this leaves is taken by the next miss.
                let obj = object_identity(px, page * gex_abi::DISPATCH_PAGE).unwrap_or(0);
                let (ready, cursor) = shared_modules_of_object(
                    j,
                    obj,
                    if whole { usize::MAX } else { per_miss },
                    SERVE_MAX_BYTES,
                );
                j.obj_cursor.insert(obj, cursor);
                // Only modules whose every page base agrees with this process's
                // numbering (or names a page not numbered here yet). A module is
                // written at the bases the process that BUILT it gave its pages;
                // where this process gave the same page a different base, its
                // blocks would land in slots that mean another page here --
                // over a block this process already wrote, or under a directory
                // entry that dispatches into it as something else. That is the
                // x86 pipeline wild branch (`ls | cat`): 4096 slots a page makes
                // two processes' numberings diverge in a few misses.
                let before = ready.len();
                let ready: Vec<_> = ready
                    .into_iter()
                    .filter(|(_, _, d, _)| {
                        // Nor one covering a page retired here: it may have
                        // been built from the code before the change (a
                        // translation of noita.exe before Detours patched it),
                        // and serving it would run that code again.
                        //
                        // AArch64 too. Its numberings diverge across an
                        // `execve` from a fork: the exec'd program keeps the
                        // page bases it inherited for ld.so (outside the
                        // arena), which are not the ones the registry has, and
                        // stores its modules under them. Served to a fresh
                        // process numbering ld.so by the registry, their calls
                        // into ld.so landed on empty slots, missed, and lifted
                        // the callee again at the registry's base, once per
                        // call: 32,997 ld.so blocks (30,371 at
                        // `_dl_lookup_symbol_x` alone) in a `node -e 0` after
                        // `spawnSync(node)`, 62 s against 30 in the page.
                        d.iter().all(|(p, b)| match j.page_table.lookup(*p) {
                            Some(have) if gex_abi::X86 => have == *b,
                            // On AArch64 only against blocks this process has
                            // installed: a page numbered here with nothing
                            // installed under the number loses nothing by
                            // taking the module's, and refusing those cost a
                            // second `node -e 0` its libc (4,979 blocks lifted
                            // became 15,975).
                            Some(have) => have == *b || LIVE_BASE.0.borrow().get(p) != Some(&have),
                            None => !gex_abi::X86 || !j.retired_pages.contains(p),
                        })
                    })
                    .collect();
                if tracing() && ready.len() != before {
                    trace_line(&format!(
                        "[gex] serve: {} of {} stored module(s) number a page differently here, not taken\n",
                        before - ready.len(),
                        before
                    ));
                }
                if !ready.is_empty() {
                    if tracing() {
                        trace_line(&format!(
                            "[gex] serve {} module(s) of the object at pc={pc:#x}\n",
                            ready.len()
                        ));
                    }
                    // Merged into one module: each separate install is a
                    // `compile` and `instantiate`, which dominates once a
                    // process can identify its pages. The merge copies bodies
                    // and patches only recorded immediates, so the result is
                    // byte-identical. See `gex_emit::merge`.
                    let mut slots = 0u64;
                    let mut dirs: Vec<(u64, u32)> = Vec::new();
                    let mut kept: Vec<Vec<(u64, u32)>> = Vec::with_capacity(ready.len());
                    for (id, s, d, _) in &ready {
                        j.installed_mods.insert(*id);
                        slots = slots.max(*s);
                        // Every page they cover, before the install: install
                        // asks the shim which slot each block belongs in, and
                        // the answer comes from here.
                        //
                        // Only pages whose code HERE is what the stored unit
                        // was built from: a libc unit's blocks run into the
                        // program that called it, so its covered pages include
                        // the image pages of the process that built it -- another
                        // program at the same PIE base (dash's `_init` page
                        // served into qt6ct). The identity map says which base a
                        // (page, identity) has; a page whose identity here does
                        // not name this base is another program's, and the
                        // entry is dropped (its blocks land in slots nothing
                        // dispatches to).
                        let mut keep: Vec<(u64, u32)> = Vec::new();
                        for (p, b) in d {
                            if !gex_abi::X86 {
                                j.page_table.assign(*p, *b);
                                keep.push((*p, *b));
                                continue;
                            }
                            let same = page_identity(px, *p * gex_abi::DISPATCH_PAGE)
                                // SAFETY: a lookup in the kernel's memory, borrowing nothing here.
                                .map_or(false, |k| unsafe { k_slot_get(*p, k) } == *b);
                            if !same {
                                if tracing() {
                                    trace_line(&format!(
                                        "[gex] serve: page {p:#x} base {b:#x} is another program's here, entry dropped\n"
                                    ));
                                }
                                continue;
                            }
                            // Only pages this process has not numbered: see
                            // `gex_jit_installed` for why a second base is fatal.
                            if j.page_table.lookup(*p).is_none() {
                                j.page_table.assign(*p, *b);
                            }
                            keep.push((*p, *b));
                        }
                        note_live(&keep);
                        dirs.extend_from_slice(&keep);
                        kept.push(keep);
                        j.reuse_served += 1;
                    }
                    // Does any of this answer the block that faulted? A unit
                    // holds only what was reachable in the process that built
                    // it, so serving a batch that does not cover this address is
                    // not progress -- the branch misses again and the next miss
                    // takes the next batch, spinning without producing the block.
                    // Only if this is the FIRST serve of the page for a fault.
                    // `dirs` naming the page means a served unit has *a* block
                    // on it, not the one at `pc`; a child's post-fork block is
                    // on a page its parent translated yet is in no unit, so
                    // serving covers=true, resumes, and faults on the same empty
                    // slot forever. Serving once then falling through to
                    // translate produces the missing block.
                    let covers = dirs.iter().any(|(p, _)| *p == page)
                        && SERVED_PAGE.0.borrow_mut().insert(page);
                    let bytes: Vec<&[u8]> = ready.iter().map(|(_, _, _, w)| w.as_slice()).collect();
                    let merged = if TEST_SERVE_UNMERGED.load(std::sync::atomic::Ordering::Relaxed) { None } else { gex_xlate::merge::merge(&bytes) };
                    match merged {
                        Some(wasm) => push_pending(j, dirs, slots, wasm, 0),
                        None => {
                            // Not the shape the merger expects. One at a time
                            // rather than lose the code, each with the entries
                            // kept above, never its own whole list: those name
                            // the pages of the program that built it, and
                            // installing them pointed this process's page at
                            // that program's blocks (W3: winlogon.exe, at
                            // 0x400000 like the i386 winedevice.exe whose
                            // sechost unit it was served, ran winedevice's
                            // code and faulted at 0x90900040).
                            for ((id, s, _, w), keep) in ready.into_iter().zip(kept) {
                                push_pending(j, keep, s, w, id);
                            }
                        }
                    }
                    publish_pending(j);
                    if covers {
                        set_resume(pc);
                        set_resume_ctx(my_ctx());
                        return MissPre::Done(true);
                    }
                    // Otherwise fall through and lift the block in this same
                    // turn. What was served is queued either way, and the
                    // trap at the end of translation installs all of it.
                }
                // Only point the directory at a shared base this process can
                // actually reach. `shared_slot` answers from the system-wide map
                // (and `j.shared`, which outlives fork), so a forked child
                // gets bases some *other* process installed but that this one
                // never put in its own tables. With no shared modules installed
                // here, every such base names an absent slot; writing it sends
                // the next indirect branch past the end of the table and traps
                // ("table index is out of bounds", 0 modules installed -- the
                // dpkg-deb/xz half of the debian-deb/lzma crash). The serve path
                // above is what installs a shared module; until it has, leave
                // the page at the miss stub so it translates locally instead.
                if !j.installed_mods.is_empty()
                    && (!gex_abi::X86 || j.page_table.lookup(page).is_none())
                {
                    if let Some(base) = shared_slot(j, page, key) {
                        j.page_table.assign(page, base);
                        // And the guest's directory, which `execve` cleared: only
                        // freshly allocated pages get an entry from the unit, and
                        // this page already has a base, so without writing it here
                        // the branch misses again next instruction.
                        let mut gm = GuestMem::new(guest_bytes());
                        dir_note(page, base, "miss-shared-slot");
                        let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, base);
                    }
                }
            }
        }
        let started = now_ns();
        // x86-64: what the page table's identity hooks answer from, as of now,
        // and which pages are a JIT's (writable and executable), so their
        // blocks carry a self-modifying-code guard. See `Block::guard`.
        if gex_abi::X86 {
        refresh_ident_snapshot(px);
        gex_xlate::set_volatile_ranges(
            px.mm
                .lock()
                .vmas()
                .iter()
                .filter(|v| {
                    v.prot & gex_kernel::mem::prot::EXEC != 0
                        && v.prot & gex_kernel::mem::prot::WRITE != 0
                })
                .map(|v| (v.start, v.end))
                .collect(),
        );
        }
        phase(11);
        // The page table moves out and back: cloning it (a BTreeMap of every
        // page the process has numbered) on each miss, twice, was a second of
        // a node start-up's 7,400 misses. Out of `Host` and marked so, until
        // `jit_in` puts it back.
        let epoch = (
            jitout::EPOCH.load(std::sync::atomic::Ordering::Acquire),
            jitout::EXEC_EPOCH.load(std::sync::atomic::Ordering::Acquire),
        );
        let go = MissPre::Go {
            exec,
            known: std::mem::take(&mut j.translated),
            table: std::mem::take(&mut j.page_table),
            intr: std::mem::take(&mut j.intrinsics),
            page,
            started,
            epoch,
        };
        jitout::OUT.store(jit_me(), std::sync::atomic::Ordering::Release);
        go
    }));
    let translated = match pre {
        MissPre::Done(t) => t,
        MissPre::Go { exec, mut known, table, intr, page, started, epoch } => {
    // A provider's address counts as known: a miss there is bridged again
    // (`cgen::provider_serves`), its bytes may be a `ud2` stub.
    let (mut unit, stats) = gex_xlate::translate_runtime(
            guest_bytes(),
            pc,
            &|a| exec.iter().any(|(s, e)| a >= *s && a + gex_abi::INST_ALIGN <= *e),
            &|a| known.contains(&a) || crate::cgen::provider_serves(a),
            table,
            Default::default(),
            // A cap keeps one miss from translating a whole library at once;
            // the next miss picks up where this stopped.
            LIFT_MAX.load(std::sync::atomic::Ordering::Relaxed),
            // Must match the memory this shim was built against: a module the
            // translator emits is instantiated against the same memory, and an
            // import that disagrees about sharing does not link.
            cfg!(feature = "shared-memory"),
            &intr,
        );
        note_kuser_folds(pc);
        // The blocks are counted translated before the set goes back, so a
        // sibling that takes it up first does not lift them again; taken out
        // again below if the unit is dropped.
        let fresh: Vec<u64> = unit
            .exported_blocks
            .iter()
            .map(|(a, _)| *a)
            .filter(|a| known.insert(*a))
            .collect();
        *jitout::BOX.0.borrow_mut() = Some((std::mem::take(&mut unit.page_table), known, intr));
        jitout::OUT.store(jit_me() | jitout::DONE, std::sync::atomic::Ordering::Release);
        #[cfg(feature = "shared-memory")]
        // SAFETY: the address of a `static AtomicU32` in the shared memory.
        unsafe {
            warch::memory_atomic_notify(jitout::OUT.as_ptr() as *mut i32, u32::MAX);
        }
        with_px(|px| with_jit_only(|j| {
        if pc == 0x10f6d24 {
            let ok = exec.iter().any(|(s, e)| pc >= *s && pc + gex_abi::INST_ALIGN <= *e);
            let has_block = unit.covered.iter().any(|(p, _)| *p == 0x10f6);
            warn_line(&format!(
                "[trace] translate pc=0x10f6d24 blocks={} exec={} covers_page_10f6={} exec_regions={}\n",
                stats.blocks, ok, has_block, exec.len(),
            ));
        }
        // Code retired while this lifted: installed all the same, then those
        // ranges retired again below (see `jitout::EPOCH`). The program
        // replaced, or more retired than the log holds: the unit is dropped,
        // and the branch misses again.
        let retired_since = retired_since(epoch.0);
        let execd = jitout::EXEC_EPOCH.load(std::sync::atomic::Ordering::Acquire) != epoch.1;
        if retired_since.is_none() || execd {
            for a in &fresh {
                j.translated.remove(a);
            }
            // And the pages it numbered. The translator gives a page its base
            // in the page table it was handed, which came back with the
            // result; the directory entry is written only when the unit is
            // installed. Kept numbered with the entry never written, the next
            // lift there sees an old page, writes no entry for it either, and
            // every block of it that runs into that page (an instruction
            // across the boundary) fails its page check against the empty
            // entry, asks to be translated again, and gets the same check: a
            // miss loop for ever (Undertale, frozen at its first frame, when
            // any retire dropped the unit). After an `execve` the table is
            // the new program's, and nothing of this unit is in it.
            if !execd {
                for (p, b) in &unit.dir_entries {
                    if j.page_table.lookup(*p) == Some(*b) {
                        j.page_table.forget(*p);
                    }
                }
            }
            static DROPPED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if tracing() || n <= 4 || n.is_power_of_two() {
                warn_line(&format!(
                    "[gex] miss at pc={pc:#x}: program replaced (or too much retired) during its translation, unit dropped ({n} so far)\n"
                ));
            }
            set_resume(pc);
            set_resume_ctx(my_ctx());
            return true;
        }
        if stats.blocks == 0 {
            return false;
        }
        xlate_add(0, stats.instructions as u64);
        xlate_add(1, stats.wasm_bytes as u64);
        xlate_add(2, 1);
        // Into the system-wide store, so a sibling installs it rather than
        // re-lifting. Published by what the unit *covers*, not what it allocated
        // (handed the allocation list, nine of ten units published nothing); the
        // slot count travels with it, since the installer's high-water differs.
        // Every unit, however many the store already holds for these pages.
        //
        // Capping it looks obviously right and is not: every process publishes
        // about 1.5 MiB of near-duplicates into the one memory every process
        // shares, which only grows, so a 60-process run added 92 MiB to it. But
        // a unit is route-dependent -- it holds what was reachable from the
        // block that faulted in the process that built it -- so those
        // "duplicates" are alternative routes, and they are what later
        // processes are served instead of lifting their own. Measured on a
        // 60-process loop:
        //
        //     claims kept per page   kernel growth   blocks lifted   wall
        //     unbounded                    92 MiB           6,162   20.7s
        //     16                           42 MiB          12,084   47.9s
        //     3                             7 MiB          13,940   52.3s
        //
        // A bound trades a bounded amount of shared memory for unbounded
        // re-translation in every process that comes later. The growth is real
        // and worth fixing, but the fix is route-independent units -- one
        // publication that covers a page for everybody -- not a smaller cache.
        // Not into the store if code was retired while it lifted: it may hold
        // the old bytes, and another process would be served them.
        let stale = retired_since.as_ref().is_some_and(|r| !r.is_empty());
        let mod_id = if stale { 0 } else { share_module(&unit.wasm, unit.table_slots, &unit.covered) };
        if mod_id == 0 {
            j.store_full += 1;
        } else {
            j.store_ok += 1;
        }
        // How many of this unit's pages could be named: `noident` counts only
        // the miss path, so a page identifiable at fault but not at store time
        // is invisible, and the store happens here.
        {
            let total = unit.covered.len();
            let named = unit
                .covered
                .iter()
                .filter(|(pg, _)| page_identity(px, *pg * gex_abi::DISPATCH_PAGE).is_some())
                .count();
            static NTH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if tracing() && NTH.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                warn_line(&format!(
                    "[gex] storing a unit: {named} of {total} pages can be named\n"
                ));
            }
        }
        for (page, base) in &unit.covered {
            if let Some(key) = page_identity(px, *page * gex_abi::DISPATCH_PAGE) {
                record_shared_slot(j, px.pid, *page, key, *base);
                if mod_id != 0 {
                    // SAFETY: a call into the kernel taking no borrow here.
                    unsafe { k_mod_claim(*page, key, mod_id) };
                }
            }
        }
        // Claim against the objects this unit emitted code for, from
        // `exported_blocks`: `dir_entries` holds only freshly allocated pages,
        // so a unit landing on already-based pages would claim nothing.
        if mod_id != 0 {
            let mut claimed: Vec<u64> = Vec::new();
            for (addr, _) in &unit.exported_blocks {
                if let Some(obj) = object_identity(px, *addr) {
                    if !claimed.contains(&obj) {
                        claimed.push(obj);
                        // SAFETY: a call into the kernel taking no borrow.
                        unsafe { k_mod_claim_object(obj, mod_id) };
                    }
                }
            }
        }
        if mod_id != 0 {
            j.installed_mods.insert(mod_id);
        }
        for (addr, _) in &unit.exported_blocks {
            j.translated.insert(*addr);
            j.provider.insert(*addr, unit.module_id);
        }
        // The base this unit gave the faulting page, captured before `wasm`
        // is moved. `dir_entries` holds only *freshly allocated* pages, so a
        // page that already had a page-table base is excluded and
        // `publish_pending` never writes its guest-directory entry -- which is
        // the fork-of-a-fork dispatch loop: the grandchild inherits the base
        // in its page table (a fork carries `#lazyInherited`) but its guest
        // directory entry stays 0 (the middle process never installed the
        // module), so the block at `pc` misses, re-translates, and misses
        // again forever. Reconcile it below, after the unit is installed.
        let fault_base = unit.covered.iter().find(|(p, _)| *p == page).map(|(_, b)| *b);
        note_live(&unit.covered);
        push_pending(j, unit.dir_entries.clone(), unit.table_slots, unit.wasm, mod_id);
        publish_pending(j);
        // Point the guest directory at the slot this unit's block actually
        // lives in, for the faulting page only (minimal, to keep the shared
        // dispatch path load-bearing for apt/closures). Only when it differs.
        if let Some(base) = fault_base {
            let mut gm = GuestMem::new(guest_bytes());
            let at = gex_abi::DISPATCH_DIR + page * 4;
            if gm.read_u32(at).unwrap_or(0) != base {
                dir_note(page, base, "miss-reconcile-covered");
                let _ = gm.write_u32(at, base);
            }
        }
        j.blocks += stats.blocks as u64;
        let whose = object_name_at(px, pc);
        if whose.is_empty() {
            xlate_add(7, stats.blocks as u64);
            xlate_add(9, stats.wasm_bytes as u64);
        }
        if j.retired_pages.contains(&page) {
            xlate_add(6, stats.blocks as u64);
            xlate_add(8, stats.wasm_bytes as u64);
        }
        *j.obj_wasm.entry(whose.clone()).or_insert(0) += stats.wasm_bytes as u64;
        note_blocks(&mut j.by_object, whose, pc, stats.blocks as u64);
        // `GEX_DISCOVER_SKIP` bit 8: what discovery lifted and why, one
        // `address why depth object` line a block, read at exit
        // (`gex_dl_text`, run-node's GEX_DL_FILE; docs/jit-guest.md).
        let dlog = gex_xlate::take_discover_log();
        if !dlog.is_empty() {
            let mut t = DL_TEXT.lock().unwrap_or_else(|e| e.into_inner());
            for (a, why, depth) in dlog {
                t.push_str(&format!("{a:x} {why} {depth} {}\n", object_name_at(px, a)));
            }
        }
        let every = XLATE_REPORT.load(std::sync::atomic::Ordering::Relaxed);
        if every != 0 && XLATE_STAT[2].load(std::sync::atomic::Ordering::Relaxed) % every == 0 {
            xlate_report(j);
        }
        j.compiles += 1;
        j.nanos += now_ns().saturating_sub(started);
        if tracing() {
            // Whether the block that faulted is in what was built, and at which
            // slot the builder placed it: a unit without it means the resume
            // misses again; a unit with it at a slot the directory does not
            // name means the resume runs something else.
            let has = unit.exported_blocks.iter().any(|(a, _)| *a == pc);
            let entry = j.page_table.lookup(page).unwrap_or(0);
            trace_line(&format!(
                "[gex]   unit has block at pc={pc:#x}: {has}; page entry t{} base {} -> slot {}; blocks {:?}\n",
                gex_abi::dispatch_entry_table(entry),
                gex_abi::dispatch_entry_base(entry),
                gex_abi::dispatch_index(pc, entry),
                unit.exported_blocks.iter().take(12).map(|(a, _)| format!("{a:#x}")).collect::<Vec<_>>(),
            ));
            trace_line(&format!(
                "[gex]   translated {} blocks in {} ms, {} vmas, {} slots, {} pages\n",
                stats.blocks,
                (now_ns().saturating_sub(started)) / 1_000_000,
                exec.len(),
                unit.table_slots,
                unit.dir_entries.len()
            ));
        }
        if let Some(r) = retired_since.filter(|r| !r.is_empty()) {
            static REPLAYED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = REPLAYED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if tracing() || n <= 4 || n.is_power_of_two() {
                warn_line(&format!(
                    "[gex] miss at pc={pc:#x}: {} range(s) retired during its translation, retired again after its install ({n} so far)\n",
                    r.len()
                ));
            }
            retire_ranges(j, &r);
        }
        // Come back at the address that missed, now that it will resolve.
        set_resume(pc);
        set_resume_ctx(my_ctx());
        true
        }))
        }
    };
    if translated && in_place(_ctx, pc) {
        return gex_emit::module::DISPATCH_MISS_REDISPATCH;
    }
    if !translated {
        if !with_jit_only(|j| j.described) {
            trace_line("gex: the guest module carries no gex.meta section, so there is\n");
            trace_line("gex: nowhere for runtime translation to carry on from. Rebuild it.\n");
        }
        (|| {
            let Some(home) = sigs::home() else { return Default::default() };
            let gm = GuestMem::new(guest_bytes());
            let c = my_ctx();
            let r = |i: u8| gm.read_u64(c + ctx::x(i) as u64).unwrap_or(0);
            trace_line(&format!(
                "gex: pid {} thread {} ctx {c:#x} miss at {pc:#x} sp={:#x} x30={:#x} x0={:#x} x1={:#x}\n",
                home.pid(),
                cur(),
                gm.read_u64(c + ctx::SP as u64).unwrap_or(0),
                r(30),
                r(0),
                r(1)
            ));
            // And what this process is: a zeroed context means a child that
            // lost its image or never had one, and the program it was told to
            // become names which.
            trace_line(&format!(
                "gex: running {:?}, {} vmas, last syscall {}, ctx pc={:#x}\n",
                home.exe(),
                home.mm().lock().vmas().len(),
                LAST_SYSCALL.load(std::sync::atomic::Ordering::Relaxed),
                gm.read_u64(c + ctx::PC as u64).unwrap_or(0),
            ));
        })();
        trace_line(&format!("gex: nothing to translate at pc={pc:#x}\n"));
        set_exit(128 + 11);
    }
    trap();
}


/// The browser half of the native in-place redispatch (65f9a719): instead
/// of trapping out of every wasm frame of the thread (unit frames included)
/// to resume at `pc`, have JS install what the miss needs synchronously and
/// let the miss stub dispatch again. At most a few times in a row at one
/// pc, then the trap as before (it also catches up on everything else).
fn in_place(ctx: i32, pc: u64) -> bool {
    thread_local! {
        static LAST: std::cell::Cell<(u64, u32)> = const { std::cell::Cell::new((0, 0)) };
    }
    if !IN_PLACE.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    let n = LAST.with(|l| {
        let (p, n) = l.get();
        let n = if p == pc { n + 1 } else { 1 };
        l.set((pc, n));
        n
    });
    if n > 3 {
        return false;
    }
    // SAFETY: a host import taking scalars; nothing here holds the host.
    let ok = unsafe { jit_install(ctx) } == 1;
    if ok {
        IN_PLACE_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // The resume the miss path staged for a trap is not taken: left, the
        // next trap that means "wait" (no resume of its own) would resume here.
        set_resume_none();
        RESUME_CTX.with(|c| c.set(None));
    }
    ok
}

/// `gex_set_in_place(0)`: every miss traps out, as before (for measuring).
static IN_PLACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
pub(crate) static IN_PLACE_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[no_mangle]
pub extern "C" fn gex_set_in_place(on: i32) {
    IN_PLACE.store(on != 0, std::sync::atomic::Ordering::Relaxed);
}

/// `BRK_KUSER_UNFOLDED` (W3 with KUSER_SHARED_DATA folded): an access at
/// or above Windows' old KUSER page through a pointer the translator could
/// not fold. Said by name, then an access violation for Wine's handler (the
/// fault hook), as the wild-branch path raises one; without a hook, SIGSEGV.
/// Never a read of the old page. Does not return.
fn kuser_unfolded(arg: i64) -> ! {
    let (pc, addr) = gex_abi::kuser_miss_unpack(arg);
    {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n <= 32 || n.is_power_of_two() {
            warn_line(&format!(
                "[gex] KUSER access not folded at pc {pc:#x} ({}), address {addr:#x}: an access violation ({n} so far)\n",
                w3_image_off_at(pc)
            ));
        }
        let mut gm = GuestMem::new(guest_bytes());
        let c = my_ctx();
        let _ = gm.write_u64(c + ctx::PC as u64, pc);
        if cgen::fault_hook() != 0 {
            cgen::fault_stage(0xc000_0005, addr, 0);
            set_resume(pc);
            set_resume_ctx(c);
        } else {
            set_exit(128 + 11);
        }
    }
    trap()
}

/// The PE images Wine mapped in this process (`gex_image_mapped`): base,
/// end and file name. Wine maps an image into anonymous memory, so the
/// memory map has no name for its code; the KUSER lines name it from here.
static W3_IMAGES: Mutex<Vec<(u64, u64, String)>> = Mutex::new(Vec::new());

pub(crate) fn note_w3_image(base: u64, size: u64, path: &str) {
    let name = path.rsplit('/').next().unwrap_or(path).to_string();
    let mut g = W3_IMAGES.lock().unwrap_or_else(|e| e.into_inner());
    // A range mapped again (an image unloaded, another in its place).
    g.retain(|&(b, e, _)| e <= base || base + size <= b);
    g.push((base, base + size, name));
}

/// The W3 image holding `pc` and the offset into it.
fn w3_image_at(pc: u64) -> Option<(String, u64)> {
    let g = W3_IMAGES.lock().unwrap_or_else(|e| e.into_inner());
    g.iter().find(|&&(b, e, _)| b <= pc && pc < e).map(|(b, _, n)| (n.clone(), pc - b))
}

/// "module +off" for `pc`: a W3 image, else whatever the memory map has.
fn w3_image_off_at(pc: u64) -> String {
    match w3_image_at(pc) {
        Some((n, off)) => format!("{n} +{off:#x}"),
        None => object_off_at(pc),
    }
}

/// What each translation folded of KUSER_SHARED_DATA (W3), by object: the
/// translator's line for it, one per translation that folded any.
pub(crate) fn note_kuser_folds(pc: u64) {
    let n = gex_ir::take_kuser_folds();
    if n == 0 {
        return;
    }
    static BY: Mutex<Option<std::collections::BTreeMap<String, u64>>> = Mutex::new(None);
    let name = w3_image_at(pc).map(|(n, _)| n).unwrap_or_else(|| object_file_off(pc).0);
    let mut g = BY.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(Default::default);
    *m.entry(name.clone()).or_default() += n;
    let list: Vec<String> = m.iter().map(|(k, v)| format!("{k} {v}")).collect();
    warn_line(&format!("[gex] kuser: {n} KUSER_SHARED_DATA access(es) folded translating {pc:#x} in {name}; by object: {}\n", list.join(", ")));
}

/// A fault import's pc without bit 62, which a unit sets on a fault it can
/// resume nested (R: the native host dispatches those nested; guest pcs are
/// below 2^48). The browser still traps out for them.
fn fault_pc(pc: i64) -> i64 {
    pc & !(1i64 << 62)
}

/// Code the guest unmapped or reprotected since the last syscall loses its
/// translations, as natively (`retire_translations`): the pages leave the
/// shared page table and their directory entries go back to the miss stub,
/// so the next branch there, from any Worker, lifts what is mapped there
/// now. Without this a `dlclose` and a `dlopen` that reuses the address ran
/// the old library's blocks (Half-Life: SDL's libudev landing where an
/// earlier library had been, entered in the middle of an instruction).
fn retire_unmapped_code_in(h: &mut Host) {
    let mm = &h.kernel.mem;
    with_jit_only(|_| {
        let (retired, recheck, _) = take_logged(mm, false);
        retire_noted(mm, &retired, &recheck);
    });
}

/// The code the map's log says was retired, rechecked and (with `mapped`)
/// mapped since it was last looked at, taken out of it.
///
/// Only under the JIT lock, held until what is taken is retired: the
/// entries leave the log there, so a thread whose own mapping call reuses
/// a retired address, and drains the log before it returns, either finds
/// them still there and retires them itself, or waits for whoever took
/// them to have retired them. A call that makes code executable (`mmap`,
/// `mprotect`, the NT layer's) always drains, so no thread runs new code
/// at an address whose old translations are still in place, whichever
/// path, locked or not, unmapped it (docs/kernel-locking.md §4.3, L12b).
pub(crate) fn take_logged(
    mm: &gex_kernel::mem::Mm,
    mapped: bool,
) -> (Vec<(u64, u64)>, Vec<(u64, u64)>, Vec<gex_kernel::CodeMap>) {
    assert!(jit_holds(), "gex: the map's log drained without the JIT lock");
    let mut g = mm.lock();
    let log = &mut g.log;
    (
        std::mem::take(&mut log.retired_code),
        std::mem::take(&mut log.recheck_code),
        if mapped { std::mem::take(&mut log.new_code) } else { Vec::new() },
    )
}

/// What a mapping call made without the kernel lock (`free_syscall`) noted
/// in the map's log: its retired code retired, as `retire_unmapped_code_in`
/// does under the lock, and the code it mapped, for the caller to queue.
/// The JIT lock for the retire, the mm lock for the log and the map: no
/// kernel lock (docs/kernel-locking.md §4.3, L12b).
fn map_drain() -> Vec<gex_kernel::CodeMap> {
    let Some(home) = sigs::home() else { return Vec::new() };
    let mm = home.mm();
    with_jit_only(|_| {
        let (retired, recheck, mapped) = take_logged(mm, true);
        retire_noted(mm, &retired, &recheck);
        mapped
    })
}

/// Retire `retired` (code unmapped or made writable) and compare `recheck`
/// (made executable again), whoever noted them.
fn retire_noted(mm: &gex_kernel::mem::Mm, retired: &[(u64, u64)], recheck: &[(u64, u64)]) {
    if !retired.is_empty() {
        crate::cgen::on_retire(retired, &gone_code(mm, retired));
        crate::fjit::on_retire(retired);
        with_jit_only(|j| retire_code(j, retired));
    }
    // Made executable again: the units' pages are compared, nothing is
    // forgotten (`MapLog::recheck_code`).
    if !recheck.is_empty() {
        crate::cgen::on_retire(recheck, &[]);
    }
}

/// The pages of `ranges` no longer mapped executable (an unmapped object,
/// code made non-executable), merged into ranges: `cgen::on_retire` drops
/// the units there whatever the bytes are.
pub(crate) fn gone_code(mm: &gex_kernel::mem::Mm, ranges: &[(u64, u64)]) -> Vec<(u64, u64)> {
    use gex_kernel::mem::prot;
    let page = gex_abi::DISPATCH_PAGE;
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &(start, end) in ranges {
        let mut a = start / page * page;
        while a < end {
            let vma = mm.lock().vma_at(a).map(|v| v.prot);
            let exec = vma.is_some_and(|p| p & prot::EXEC != 0);
            // A PE image's page made non-executable for a while keeps its
            // unit (`cgen::in_pe_image`).
            let dormant = vma.is_some_and(|p| p != 0) && crate::cgen::in_pe_image(a);
            if !exec && !dormant {
                match out.last_mut() {
                    Some(last) if last.1 == a => last.1 = a + page,
                    _ => out.push((a, a + page)),
                }
            }
            a += page;
        }
    }
    out
}

/// Forget every translation in `ranges` (see `retire_unmapped_code`). The
/// old slots are left behind, unreachable.
pub(crate) fn retire_code(j: &mut Jit, ranges: &[(u64, u64)]) {
    use std::sync::atomic::Ordering;
    // Under the JIT lock, so it waits for a translation in flight; and a
    // translation that may have read the old code retires these ranges again
    // once its unit is queued (`jitout::EPOCH`).
    jit_in_held(j);
    if !ranges.is_empty() {
        let e = jitout::EPOCH.fetch_add(1, Ordering::Release).wrapping_add(1);
        let mut log = jitout::RETIRED.0.borrow_mut();
        for &(start, end) in ranges {
            log.push_back((e, start, end));
        }
        while log.len() > 256 {
            log.pop_front();
        }
        drop(log);
    }
    retire_ranges(j, ranges);
}

/// `gex_test_serve_unmerged(1)` (GEX_TEST_SERVE_UNMERGED): modules served
/// at a miss are installed one at a time, as when `gex_xlate::merge`
/// declines them. A test switch, off in every real run.
static TEST_SERVE_UNMERGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[no_mangle]
pub extern "C" fn gex_test_serve_unmerged(on: i32) {
    TEST_SERVE_UNMERGED.store(on != 0, std::sync::atomic::Ordering::Relaxed);
}

/// `gex_test_unpublished_retire(1)` (GEX_TEST_UNPUBLISHED_RETIRE): see
/// `retire_ranges`. A test switch, off in every real run.
static TEST_UNPUBLISHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[no_mangle]
pub extern "C" fn gex_test_unpublished_retire(on: i32) {
    TEST_UNPUBLISHED.store(on != 0, std::sync::atomic::Ordering::Relaxed);
}

/// `retire_code`'s work on the JIT state, without the bookkeeping.
fn retire_ranges(j: &mut Jit, ranges: &[(u64, u64)]) {
    use std::sync::atomic::{AtomicU32, Ordering};
    for &(start, end) in ranges {
        if end <= start {
            continue;
        }
        j.translated.retain(|a| *a < start || *a >= end);
        xlate_add(5, 1);
        for page in start / gex_abi::DISPATCH_PAGE..=(end - 1) / gex_abi::DISPATCH_PAGE {
            if page >= gex_abi::DISPATCH_MAX_PAGES {
                break;
            }
            if j.page_table.lookup(page).is_some() {
                xlate_add(4, 1);
            }
            j.page_table.forget(page);
            j.retired_pages.insert(page);
            let at = gex_abi::DISPATCH_DIR + page * 4;
            // SAFETY: the directory is in the shared memory, four-byte
            // aligned, and every Worker's dispatch reads it as a word.
            unsafe { (*(at as usize as *const AtomicU32)).store(0, Ordering::Release) };
            // TEST(gex_test_unpublished_retire): the retired page numbered
            // again at once and its entry left unwritten, as a publisher
            // that numbered pages and has not installed (or never will)
            // leaves them (tests/x86-programs/page-cross-entry.c).
            if TEST_UNPUBLISHED.load(Ordering::Relaxed) {
                let _ = j.page_table.base(page);
            }
        }
    }
}

/// What `retire_code` retired since a translation took the JIT state out at
/// `epoch` (`jitout::EPOCH`), or `None` when the log no longer holds all of
/// it. Asked with the JIT lock held and the state back in, when no retire can
/// run beside it.
fn retired_since(epoch: u32) -> Option<Vec<(u64, u64)>> {
    let now = jitout::EPOCH.load(std::sync::atomic::Ordering::Acquire);
    let log = jitout::RETIRED.0.borrow();
    let covered = now == epoch || log.iter().any(|(e, _, _)| *e == epoch.wrapping_add(1));
    covered.then(|| {
        log.iter()
            .filter(|(e, _, _)| e.wrapping_sub(epoch).wrapping_sub(1) < u32::MAX / 2)
            .map(|&(_, s, e)| (s, e))
            .collect()
    })
}

/// Queue objects the guest mapped since the last syscall as recompile
/// candidates. Off unless the page asks. Bulk static recompilation (see
/// `recompile_object`) is what `--recompile-objects` does natively; without it
/// a page meets a library one dispatch miss at a time.
fn recompile_objects(mapped: Vec<gex_kernel::CodeMap>) {
    if unsafe { RECOMPILE_OBJECTS } == 0 || mapped.is_empty() {
        return;
    }
    // The JIT state's own lock alone (docs/kernel-locking.md, L12c).
    with_jit_only(|j| j.candidates.extend(mapped.into_iter().map(|m| (m, 0))));
}

/// How many dispatch misses an object takes before the rest is translated in
/// bulk. Waiting beats recompiling at map time (fewer objects, fewer slots),
/// but bulk recompilation stays off by default: it only helps a run that is
/// long *and* library-bound, and translating the rest of a library the demand
/// path already covers just costs V8 a larger module.
const RECOMPILE_AFTER_MISSES: u32 = 24;

/// Slots the demand path is left holding whatever else happens: enough for a
/// few hundred pages of translated code in every process still to come, so
/// running out of budget for *bulk* recompilation never means running out for
/// the blocks a guest actually branches to.
const SLOT_RESERVE: u64 = 8 * 1024 * gex_abi::SLOTS_PER_PAGE;

/// Can the slot space pay for recompiling the whole of `map`, and still keep
/// the reserve? A page of guest text costs `SLOTS_PER_PAGE` slots whether one
/// block of it is reached or all of them, so the object's mapped size is the
/// price.
fn slots_affordable(map: &gex_kernel::CodeMap) -> bool {
    // Without a global kernel there is one process and one counter, and the
    // whole space is its own.
    if unsafe { k_present() } == 0 {
        return true;
    }
    let pages = map.end.saturating_sub(map.start) / gex_abi::DISPATCH_PAGE + 1;
    let want = pages * gex_abi::SLOTS_PER_PAGE;
    // SAFETY: takes nothing, returns a scalar, borrows nothing here.
    let left = unsafe { k_slot_left() };
    if want + SLOT_RESERVE <= left {
        return true;
    }
    // Once, and named: a run that quietly stops recompiling gets slower for a
    // reason nobody can see otherwise.
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
        warn_line(&format!(
            "[gex] not recompiling {} in bulk: it wants {want} dispatch slots and \
             {left} are left of the runtime space. Demand translation carries on; \
             this run has used its budget for whole objects.\n",
            map.path,
        ));
    }
    false
}

/// A dispatch miss landed at `pc`. If it is in an object that has now been
/// missed often enough, translate the rest of that object.
///
/// Returns whether anything was queued, which the caller takes as "the work
/// is done, do not also translate one block".
fn maybe_recompile_at(pc: u64) -> bool {
    if unsafe { RECOMPILE_OBJECTS } == 0 {
        return false;
    }
    let due = with_jit_only(|j| {
        let at = j.candidates.iter().position(|(m, _)| pc >= m.start && pc < m.end)?;
        j.candidates[at].1 += 1;
        if j.candidates[at].1 < RECOMPILE_AFTER_MISSES {
            return None;
        }
        Some(j.candidates.remove(at).0)
    });
    let Some(map) = due else { return false };
    // Only if the slot space can afford it. Bulk recompilation claims slots for
    // every page of the object, in every process that gets hot in it, and
    // nothing ever gives a slot back -- so on a run of many processes this
    // spends a budget the demand path would have made last.
    //
    // That is not a slow-down when it runs out, it is the end of the run: past
    // the last dispatch table a page's entry names table `DISPATCH_TABLES` and
    // the module built from it imports a table that does not exist, so the
    // engine refuses it ("out of bounds table index 8") in whichever forked
    // child was translating. `apt-get update` died that way at its 77th
    // process; with this it stays on the demand path instead and finishes.
    if !slots_affordable(&map) {
        return false;
    }
    let began = now_ns();
    let blocks = recompile_object(&map);
    if blocks == 0 {
        return false;
    }
    // Under `trace` as well as `heartbeat`: which objects a run recompiles in
    // bulk, and how much of the shared slot space each one costs, is the first
    // thing to ask when a many-process run runs out of slots -- and it was
    // visible only under a flag nobody reaches for.
    if tracing() || unsafe { HEARTBEAT } != 0 {
        let ms = (now_ns() - began) / 1_000_000;
        let (slots, mods, bytes) = with_jit_only(|j| {
            (
                j.page_table.slots(),
                j.pending.len(),
                j.pending.iter().map(|p| p.wasm.len()).sum::<usize>(),
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

/// Whether to statically recompile each object as it is mapped, and the size
/// cap. Zero is off; otherwise a limit in KiB of mapped text (`i32::MAX` = no
/// limit). The size doubles as a bisection handle for a bad translation.
static mut RECOMPILE_OBJECTS: u64 = 0;

#[no_mangle]
pub extern "C" fn gex_set_recompile_objects(kib: i32) {
    unsafe { RECOMPILE_OBJECTS = if kib <= 0 { 0 } else { kib as u64 } };
}

/// Statically recompile a whole mapped object from its own bytes, so the result
/// depends only on the library and where it loaded -- unlike the demand path,
/// whose output varies with which address the guest reached first.
fn recompile_object(map: &gex_kernel::CodeMap) -> usize {
    let span = map.end.saturating_sub(map.start);
    let cap = (unsafe { RECOMPILE_OBJECTS }).saturating_mul(1024).min(MAX_OBJECT_BYTES);
    if span == 0 || span > cap {
        return 0;
    }
    // The same guard the miss path has: until the guest module says how many
    // slots it uses, claiming from zero collides with the image's own blocks.
    if !with_jit_only(|j| j.described) {
        return 0;
    }
    // Has another process already paid for this library? Two processes map the
    // same libc at the same address, and the identity map lets one reuse the
    // other's slots; recompiling without asking translated it all again and
    // could reach V8's 10M-slot table ceiling. By whole pages, the identity
    // map's key; an object every page of which is answered is skipped.
    //
    // No kernel lock anywhere here (docs/kernel-locking.md §3.6, L16): the map
    // and the identities come from the process's published `Home` (`with_px`),
    // the slots and pages from the JIT state under its own lock.
    let (adopted, pages) = with_px(|px| with_jit_only(|j| {
        let first = gex_abi::dispatch_page(map.start);
        let last = gex_abi::dispatch_page(map.end.saturating_sub(1));
        let pages = (last - first + 1) as usize;
        let mut missing = 0usize;
        for page in first..=last {
            if j.page_table.lookup(page).is_some() {
                continue;
            }
            let Some(key) = page_identity(px, page * gex_abi::DISPATCH_PAGE) else {
                missing += 1;
                continue;
            };
            // Through `shared_slot`, which asks the global kernel before this
            // process's own mirror: the mirror holds only what this process
            // stored, so reading it directly re-translated libraries a sibling
            // had already published.
            match shared_slot(j, page, key) {
                Some(base) => {
                    j.page_table.assign(page, base);
                    // And the guest's directory, which `execve` cleared.
                    let mut gm = GuestMem::new(guest_bytes());
                    let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, base);
                }
                None => missing += 1,
            }
        }
        (missing, pages)
    }));
    if adopted == 0 {
        return 0;
    }
    // Not gated on how much of the object is unclaimed: when a process reaches
    // this, nearly every page really is, because apt's parallel Workers all
    // start with an empty store and all reach a library before any publishes.
    let _ = pages;
    // One process translates an object, not all: the store answers "has anybody
    // done this", `k_obj_translating` answers "is anybody doing it". A claim,
    // not a lock -- the loser returns to the miss path for the few blocks it
    // needs and picks the object up from the store once the winner publishes.
    let (obj, me) = with_px(|px| (object_identity(px, map.start), px.pid));
    let Some(obj) = obj else {
        return 0;
    };
    if unsafe { k_obj_translating(obj, me as u32) } == 0 {
        return 0;
    }
    let seeds = object_seeds(map);
    // The JIT state out, as a miss's lift takes it (`jitout`): until it is
    // back, nothing else numbers a page or adds to the translated set, so the
    // table the lift returns is the whole of the process's. (It used to be a
    // copy taken under the lock and written back over whatever a sibling's
    // miss had installed meanwhile.)
    let (exec, page_table, known, intrinsics, epoch) = with_px(|px| with_jit_only(|j| {
        let exec: Vec<(u64, u64)> = px
            .mm
            .lock()
            .vmas()
            .iter()
            .filter(|v| v.prot & gex_kernel::mem::prot::EXEC != 0)
            .map(|v| (v.start, v.end))
            .collect();
        let epoch = (
            jitout::EPOCH.load(std::sync::atomic::Ordering::Acquire),
            jitout::EXEC_EPOCH.load(std::sync::atomic::Ordering::Acquire),
        );
        let out = (
            exec,
            std::mem::take(&mut j.page_table),
            std::mem::take(&mut j.translated),
            std::mem::take(&mut j.intrinsics),
            epoch,
        );
        jitout::OUT.store(jit_me(), std::sync::atomic::Ordering::Release);
        out
    }));
    // Timed, like the block path: otherwise `blocks` and `nanos` disagree and
    // the object path's time goes unaccounted.
    let object_started = now_ns();
    phase(12);
    // The lift numbers pages in a copy and hands each module the table as it
    // left it, so the last one's is the result, and with none the table is as
    // it went in.
    let (modules, stats) = gex_xlate::translate_object(
        guest_bytes(),
        map.start,
        map.end,
        &seeds,
        &|a| exec.iter().any(|(s, e)| a >= *s && a + gex_abi::INST_ALIGN <= *e),
        &|a| known.contains(&a) || crate::cgen::provider_serves(a),
        page_table.clone(),
        Default::default(),
        MAX_OBJECT_BLOCKS,
        cfg!(feature = "shared-memory"),
        &intrinsics,
    );
    let object_nanos = now_ns().saturating_sub(object_started);
    let table = modules.last().map_or(page_table, |m| m.page_table.clone());
    *jitout::BOX.0.borrow_mut() = Some((table, known, intrinsics));
    jitout::OUT.store(jit_me() | jitout::DONE, std::sync::atomic::Ordering::Release);
    #[cfg(feature = "shared-memory")]
    // SAFETY: the address of a `static AtomicU32` in the shared memory.
    unsafe {
        warch::memory_atomic_notify(jitout::OUT.as_ptr() as *mut i32, u32::MAX);
    }
    let produced = with_px(|px| with_jit_only(|j| {
        // Retired or replaced while the state was out: as the miss does. An
        // exec drops the lot (the table is the new program's); a log that no
        // longer covers the lift drops it too, taking back the pages it
        // numbered (numbered with no entry written, a page loops on its miss
        // for ever: see the miss); otherwise it is installed and what was
        // retired meanwhile is retired again after. Nothing lifted is dropped
        // the same way.
        let retired = retired_since(epoch.0);
        let execd = jitout::EXEC_EPOCH.load(std::sync::atomic::Ordering::Acquire) != epoch.1;
        let barren = modules.iter().all(|m| m.exported_blocks.is_empty());
        if retired.is_none() || execd || barren {
            if !execd {
                for m in &modules {
                    for (p, b) in &m.dir_entries {
                        if j.page_table.lookup(*p) == Some(*b) {
                            j.page_table.forget(*p);
                        }
                    }
                }
            }
            if !barren {
                warn_line(&format!(
                    "[gex] recompiling {}: program replaced (or too much retired) during its translation, dropped\n",
                    map.path
                ));
            }
            return 0usize;
        }
        regview::publish_marks(&j.page_table);
        for m in &modules {
            for b in &m.exported_blocks {
                j.translated.insert(b.0);
                j.provider.insert(b.0, m.module_id);
            }
        }
        j.blocks += stats.blocks as u64;
        {
            let whose = map.path.rsplit('/').next().unwrap_or(&map.path).to_string();
            note_blocks(&mut j.by_object, whose, map.start, stats.blocks as u64);
        }
        j.nanos += object_nanos;
        let produced = modules.len();
        // And what this claimed, so the next process to map the same library
        // finds it. By `covered`, not allocated (as the miss path), or the next
        // process re-translates the whole library.
        for m in &modules {
            for (page, base) in &m.covered {
                if let Some(key) = page_identity(px, *page * gex_abi::DISPATCH_PAGE) {
                    // Into the global kernel too, not only this process's
                    // mirror, or the next process cannot find it.
                    record_shared_slot(j, px.pid, *page, key, *base);
                }
            }
        }
        for m in modules {
            push_pending(j, m.dir_entries, m.table_slots, m.wasm, 0);
        }
        publish_pending(j);
        if let Some(r) = retired.filter(|r| !r.is_empty()) {
            retire_ranges(j, &r);
        }
        produced
    }));
    // Released whether or not anything came of it (a barren translation must
    // not block the next process), and recorded *done* when it did, so the next
    // process serves the object from the store instead of re-lifting.
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
/// Called before either path translates anything, and outside `with_exec_lock`
/// because reading the object takes the host itself.
/// Whether a dispatch miss may translate. Off for an ejected project built
/// without a JIT fallback. See `gex_dispatch_miss`.
static JIT_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

#[no_mangle]
pub extern "C" fn gex_set_jit(on: i32) {
    JIT_ON.store(on != 0, std::sync::atomic::Ordering::Relaxed);
}

/// Whether run-time translation is allowed (`gex_set_jit`).
pub(crate) fn jit_on() -> bool {
    JIT_ON.load(std::sync::atomic::Ordering::Relaxed)
}

/// An ejected project's run (`gex_set_aot(1)`): what keeps the guest's
/// frames as an AOT-only runtime must (units and threads served inside the
/// call, a Worker catching up in place, a wild branch the guest's fault).
/// On in its training run too, where translation is allowed, so what that
/// run records is what the AOT-only run needs.
static AOT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[no_mangle]
pub extern "C" fn gex_set_aot(on: i32) {
    AOT.store(on != 0, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn aot() -> bool {
    AOT.load(std::sync::atomic::Ordering::Relaxed) || !jit_on()
}

/// Whether library substitution is on. See `gex_set_intrinsics`.
static mut INTRINSICS_ON: bool = true;

/// Per-family enable bits, for bisecting which substitution misbehaves.
/// bit0 = memcpy/memmove/mempcpy, bit1 = memset, bit2 = memcmp. All on by
/// default. See `gex_set_intrinsic_mask`.
static mut INTRINSICS_MASK: u32 = 0xffff_ffff;

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

/// Enable only some intrinsic families, to bisect which one corrupts memory.
/// bit0 = memcpy family, bit1 = memset, bit2 = memcmp. `0xffffffff` = all
/// (the default). Set before anything translates, alongside `gex_set_intrinsics`.
#[no_mangle]
pub extern "C" fn gex_set_intrinsic_mask(mask: u32) {
    unsafe { INTRINSICS_MASK = mask }
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
    let Some(mm) = named_mm() else { return 0 };
    let mm = mm.lock();
    let Some(v) = mm.vmas().get(i as usize) else {
        return 0;
    };
    let writable = v.prot & gex_kernel::mem::prot::WRITE != 0;
    (!writable && v.origin.is_some()) as i32
}

fn note_intrinsics(addr: u64) {
    if !unsafe { INTRINSICS_ON } {
        return;
    }
    // Not in a 32-bit (i386) process: the intrinsic block reads the 64-bit
    // register arguments and pops an 8-byte return address, and i386 cdecl
    // passes them on the stack.
    if compat_abi() {
        return;
    }
    // The map first, the JIT state after it and not under it (ExecLock, JIT,
    // mm: docs/kernel-locking.md §4.3), neither under the kernel lock.
    let Some((path, start)) = ({
        let mm = sigs::proc_home().mm().lock();
        mm.vmas()
            .iter()
            .find(|v| addr >= v.start && addr < v.end && v.prot & gex_kernel::mem::prot::EXEC != 0)
            .and_then(|v| v.origin.as_ref().map(|(p, _)| (p.clone(), v.start)))
    }) else {
        return;
    };
    // Retried rather than answered once, because the answer does not exist
    // yet the first time: `memcpy`'s slot is filled by the dynamic linker,
    // and a miss inside `ld.so` itself happens before that. Bounded, so an
    // object whose slots are never bound stops being re-parsed.
    if with_jit_only(|j| j.intrinsics_scanned.get(&path).copied().unwrap_or(0) >= 8) {
        return;
    }
    let (found, pending) = scan_intrinsics(&path, start);
    let added = with_jit_only(|j| {
        // A scan with nothing left outstanding need never run again.
        let n = if pending { 1 } else { 8 };
        *j.intrinsics_scanned.entry(path.clone()).or_insert(0) += n;
        let before = j.intrinsics.len();
        let mask = unsafe { INTRINSICS_MASK };
        for (a, k) in found {
            let bit = match k {
                gex_xlate::Intrinsic::MemCpy => 1,
                gex_xlate::Intrinsic::MemSet => 2,
                gex_xlate::Intrinsic::MemCmp => 4,
                gex_xlate::Intrinsic::MemPCpy => 8,
                gex_xlate::Intrinsic::MemMove => 16,
                // Never recognised by name, so never reaches this scan.
                gex_xlate::Intrinsic::Replace(_) | gex_xlate::Intrinsic::Override(_) => 0,
            };
            if mask & bit == 0 {
                continue;
            }
            j.intrinsics.insert(a, k);
        }
        j.intrinsics.len() - before
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

/// The path of the most recent `openat`, for the crash report: its own
/// lock, since `openat` takes no other (L14b).
static LAST_OPEN: gex_kernel::sync::Mutex<String> = gex_kernel::sync::Mutex::new(String::new());

/// How many bytes of it there are, written into the shim's own buffer.
#[no_mangle]
pub extern "C" fn gex_last_open(out: u32, max: u32) -> u32 {
    let s = LAST_OPEN.lock();
    let n = s.len().min(max as usize);
    shim_slice_mut(out, n as u32).copy_from_slice(&s.as_bytes()[..n]);
    n as u32
}

fn note_process_syscall(base: u64, pc: i64) {
    use gex_kernel::syscalls::{clone_flags, nr};
    let gm = GuestMem::new(guest_bytes());
    let g = |i: u8| gm.read_u64(base + ctx::x(i) as u64).unwrap_or(0);
    let n = g(8);
    // How much of a process is its dynamic linker: `ld.so` links in translated
    // code once per process, recomputing an answer a closure pins at build
    // time. Counting its syscalls says what avoiding it is worth.
    let pc = pc as u64;
    if pc >= gex_abi::INTERP_BASE && pc < gex_abi::INTERP_BASE + 0x0010_0000 {
        LOADER_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        LOADER_LAST.store(now_ns(), std::sync::atomic::Ordering::Relaxed);
    }
    // The last path this process asked to open, for the crash report: a fault
    // inside `openat` says nothing about which file, one bounded string read
    // away.
    if n == nr::OPENAT {
        if let Ok(b) = gm.read_cstr(g(1), 256) {
            // Its own lock: every Worker's openat writes it, and two at once
            // through a bare cell was a wild store (the browser's stack
            // smash).
            let path = String::from_utf8_lossy(&b).into_owned();
            *LAST_OPEN.lock() = path;
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
    let pid = my_pid();
    let what = if n == nr::EXECVE {
        // The program and its argv: "an execve happened" cannot tell the wrong
        // program from the right one with wrong arguments.
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
        // Flags named where possible: they are the whole content of a `clone`,
        // which by them makes a thread, a process, or something between.
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

/// Add `n` blocks lifted at `a` to what `whose` has cost so far.
/// One progress line: the translator's counters and the objects it lifted
/// most from, by wasm emitted. See `XLATE_REPORT`.
fn xlate_report(j: &Jit) {
    let x = |i: usize| XLATE_STAT[i].load(std::sync::atomic::Ordering::Relaxed);
    let mut v: Vec<(&String, &u64)> = j.obj_wasm.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1));
    let objs = v
        .iter()
        .take(6)
        .map(|(k, w)| {
            let n = j.by_object.get(*k).map_or(0, |e| e.0);
            format!("{}={}blk/{}KiB", if k.is_empty() { "(anon)" } else { k.as_str() }, n, **w >> 10)
        })
        .collect::<Vec<_>>()
        .join(" ");
    warn_line(&format!(
        "[gex] xlate: units={} insns={} wasm={}KiB smc={} retiredPages={} relift={}blk/{}KiB anon={}blk/{}KiB [{objs}]\n",
        x(2), x(0), x(1) >> 10, x(3), x(4), x(6), x(8) >> 10, x(7), x(9) >> 10,
    ));
}

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

/// This process's memory map, from its published `Home` (no kernel lock):
/// what the address-naming reports read. `None` before the process is
/// published.
fn named_mm() -> Option<&'static gex_kernel::mem::Mm> {
    sigs::home().map(|h| h.mm())
}

/// The object file at `a` (by file name) and `a`'s offset into its mapping.
pub(crate) fn object_file_off(a: u64) -> (String, u64) {
    let Some(mm) = named_mm() else { return Default::default() };
    mm.lock()
        .vmas()
        .iter()
        .find(|v| a >= v.start && a < v.end)
        .map(|v| (v.code.as_ref().map(|(p, _)| p.rsplit('/').next().unwrap_or(p).to_string()).unwrap_or_default(), a - v.start))
        .unwrap_or_default()
}

/// The short name of whatever object holds this address, with the
/// `+offset` into the object, so a return address on a crash stack can be
/// run through objdump to name the function. Reads the published map, so
/// it may be called with or without the kernel, but not with the map's own
/// lock held.
pub(crate) fn object_off_at(a: u64) -> String {
    let Some(mm) = named_mm() else { return String::new() };
    mm.lock()
        .vmas()
        .iter()
        .find(|v| a >= v.start && a < v.end)
        .map(|v| {
            let n = v.code.as_ref().map(|(p, _)| p.as_str()).unwrap_or(&v.name);
            let n = n.rsplit('/').next().unwrap_or(n);
            format!("{n} +{:#x}", a - v.start)
        })
        .unwrap_or_default()
}

fn object_name_at(p: &Px<'_>, a: u64) -> String {
    p.mm
        .lock()
        .vmas()
        .iter()
        .find(|v| a >= v.start && a < v.end)
        .map(|v| {
            // `code`, not `origin`: `origin` is `None` for anything the loader
            // built (every image and interpreter), which would report `[image]`
            // for all of them.
            let n = v.code.as_ref().map(|(p, _)| p.as_str()).unwrap_or(&v.name);
            let n = n.rsplit('/').next().unwrap_or(n);
            // A built region is named by address too: an executable and its
            // interpreter share a path and differ only by where they sit.
            if v.name == "[image]" {
                format!("{n}@{:#x}", v.start)
            } else {
                n.to_string()
            }
        })
        .unwrap_or_else(|| "unmapped".to_string())
}

/// Which object's executable mapping a guest address falls in, if any.
fn executable_object_at(a: u64) -> Option<String> {
    let mm = named_mm()?.lock();
    let v = mm.vmas().iter().find(|v| a >= v.start && a < v.end && v.prot & gex_kernel::mem::prot::EXEC != 0)?;
    v.origin.as_ref().map(|(p, _)| p.clone())
}

/// The library functions one object offers or calls, and whether any answer
/// is still outstanding.
///
/// Two sources: a plain `STT_FUNC` symbol names its own code, but glibc exports
/// `memcpy` and friends as `STT_GNU_IFUNC`, whose value is a *resolver* -- the
/// chosen implementation is written into the GOT at startup, so the address is
/// read from the slot the linker wrote in live memory, after it has run.
fn scan_intrinsics(path: &str, start: u64) -> (Vec<(u64, gex_xlate::Intrinsic)>, bool) {
    let Some(bytes) = sigs::proc_home().read_file(path) else {
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
        // An unbound slot points at this object's own PLT resolver stub (real
        // code, so "is this an address" cannot tell it from a real target).
        // Whose code it is does: the stub is in this object, the `memcpy` in
        // another. Binding the stub as `memcpy` makes every lazy call a wrong
        // memcpy -- how `ls` died inside `close`.
        match executable_object_at(target) {
            Some(obj) if obj != path => out.push((target, kind)),
            _ => pending = true,
        }
    }
    (out, pending)
}

fn object_seeds(map: &gex_kernel::CodeMap) -> Vec<u64> {
    // Through the process's published namespace, not the kernel (L16).
    let Some(bytes) = sigs::proc_home().read_file(&map.path) else {
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
            if v >= lo && v < hi && v % gex_abi::INST_ALIGN == 0 {
                seeds.push(v.wrapping_add(bias));
            }
        }
    }
    seeds.sort_unstable();
    seeds.dedup();
    seeds
}

/// The context the running thread is using, for a crash report: wild register
/// state usually means a wild context pointer, told apart by looking.
#[no_mangle]
pub extern "C" fn gex_current_ctx() -> i64 {
    // The remembered address first, then the kernel: this is a crash reporter's
    // question, and a trap leaves the kernel borrowed, so `with_exec_lock` would
    // panic on top of the crash and describe the reporter instead of the fault.
    let at = ctx_addr_get();
    if at != 0 {
        return at as i64;
    }
    my_ctx() as i64
}

#[no_mangle]
pub extern "C" fn gex_current_pc() -> i64 {
    // Without the kernel when there is a remembered context, because the
    // crash this most needs to describe is one that left the kernel borrowed.
    // See `CTX_ADDR`.
    let at = ctx_addr_get();
    if at != 0 {
        let gm = GuestMem::new(guest_bytes());
        return gm.read_u64(at + ctx::PC as u64).unwrap_or(0) as i64;
    }
    match my_ctx() {
        0 => 0,
        at => GuestMem::new(guest_bytes()).read_u64(at + ctx::PC as u64).unwrap_or(0) as i64,
    }
}

/// Link register at the same crash-safe context snapshot as `gex_current_pc`.
/// An indirect-branch trap leaves PC equal to the bad target; LR identifies
/// the call site that supplied it and is therefore the actionable address.
#[no_mangle]
pub extern "C" fn gex_current_lr() -> i64 {
    let read = |at: u64| {
        let gm = GuestMem::new(guest_bytes());
        gm.read_u64(at + 30 * 8).unwrap_or(0) as i64
    };
    let at = ctx_addr_get();
    if at != 0 {
        return read(at);
    }
    match my_ctx() {
        0 => 0,
        at => read(at),
    }
}

/// Which mapped object an address belongs to, written into `out` as bytes.
/// Returns the length, or zero when nothing is mapped there.
#[no_mangle]
pub extern "C" fn gex_object_at(addr: i64, out: u32, max: u32) -> u32 {
    // Not the kernel: this is a crash reporter, asked to name the pc of a
    // trap, and the trap it most needs to describe may have happened inside
    // the kernel (still borrowed). It reads the published map.
    let name = sigs::home().map(|home| {
        home.mm().lock()
            .vmas()
            .iter()
            .find(|v| (addr as u64) >= v.start && (addr as u64) < v.end)
            .map(|v| format!("{} +{:#x}", v.name, addr as u64 - v.start))
    })
    .flatten();
    let Some(name) = name else { return 0 };
    let n = name.len().min(max as usize);
    let dst = shim_slice_mut(out, n as u32);
    dst.copy_from_slice(&name.as_bytes()[..n]);
    n as u32
}

/// The highest slot total any queued module needs.
///
/// So the host grows its tables once for a batch, not once per module:
/// `Table.grow` reallocates, so many small grows are quadratic in table size.
/// Materialise a sorted snapshot of every block PC translated in this process so
/// far and return the count. The snapshot lives in the shared `Jit`, so `_ptr`
/// can hand JS its address in linear memory. Called from a sandbox run (e.g. at
/// first Wayland present) to dump the executed set as a profile seed for a
/// reachability-trimmed closure build.
#[no_mangle]
pub extern "C" fn gex_coverage_snapshot() -> u32 {
    with_jit_only(|j| {
        let mut v: Vec<u64> = j.translated.iter().copied().collect();
        v.sort_unstable();
        j.coverage_snap = v;
        j.coverage_snap.len() as u32
    })
}

/// Byte address in linear memory of the snapshot `gex_coverage_snapshot` built,
/// as `count` little-endian u64s. Valid until the next snapshot call.
#[no_mangle]
pub extern "C" fn gex_coverage_ptr() -> u32 {
    with_jit_only(|j| j.coverage_snap.as_ptr() as u32)
}

#[no_mangle]
pub extern "C" fn gex_jit_pending_slots() -> u32 {
    with_jit_only(|j| j.pending.iter().map(|p| p.slots).max().unwrap_or(0) as u32)
}

/// The module JS has to instantiate, or zero length if there is none.
/// A module's bytes at their exact size, before any Worker can see where they
/// are. The pending queue publishes the front module's address to every
/// Worker of the process (`PENDING`); `gex_jit_installed` used to shrink the
/// bytes as it kept them, which reallocates -- moving them out from under a
/// sibling still copying from the published address, into a block the next
/// translation's buffer took ("expected magic word" from WebAssembly.compile).
/// Shrunk here instead, the bytes never move once published.
fn exact(mut v: Vec<u8>) -> Vec<u8> {
    v.shrink_to_fit();
    v
}

#[no_mangle]
pub extern "C" fn gex_jit_pending_ptr() -> u32 {
    PENDING.with(|p| p[0].load(std::sync::atomic::Ordering::Relaxed))
}

#[no_mangle]
pub extern "C" fn gex_jit_pending_len() -> u32 {
    PENDING.with(|p| p[1].load(std::sync::atomic::Ordering::Relaxed))
}

/// How many slots the dispatch table must hold for the pending module.
#[no_mangle]
pub extern "C" fn gex_jit_slots() -> u32 {
    PENDING.with(|p| p[2].load(std::sync::atomic::Ordering::Relaxed))
}

/// Where a translated block belongs: table in the top eight bits, slot in the
/// rest, -1 when its page has no slots. Packed into one return (called once per
/// installed block) in the directory entry's own layout; see
/// `gex_abi::dispatch_entry`.
#[no_mangle]
pub extern "C" fn gex_dispatch_slot(addr: i64) -> i32 {
    with_jit_only(|j| {
        let page = gex_abi::dispatch_page(addr as u64);
        match j.page_table.lookup(page) {
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
/// Paired with [`gex_page_at`]. The page table (guest page -> slots) is Rust
/// data in this Worker's heap, so a forked child arrives without it though it
/// inherited the directory that points at those slots -- and would re-translate
/// pages the directory calls done, into fresh slots, then miss on the old ones.
#[no_mangle]
pub extern "C" fn gex_page_count() -> u32 {
    with_jit_only(|j| j.page_table.entries().len() as u32)
}

/// Write every page assignment to `out` as a `u64` page and a `u32` entry,
/// twelve bytes each, and return how many were written.
///
/// -1 if `cap` will not hold them; ask [`gex_page_count`] first. One call, not
/// one per page: the table is a `BTreeMap` and a per-page export was quadratic.
#[no_mangle]
pub extern "C" fn gex_page_dump(out: u32, cap: u32) -> i32 {
    with_jit_only(|j| {
        let entries = j.page_table.entries();
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

/// Take a page's slots from the process this one was forked from. The caller's
/// claim -- that the slots already hold this page's code -- holds for a fork by
/// construction, the child's text being a copy of the parent's at the same
/// addresses.
#[no_mangle]
pub extern "C" fn gex_adopt_page(page: i64, entry: u32) {
    phase(3);
    with_jit_only(|j| j.page_table.assign(page as u64, entry));
}

/// How many pages the identity map answers for.
///
/// The page table says where this process's translated pages are; the identity
/// map says which object's code those slots hold, and is the only one to
/// survive `execve`. See [`gex_shared_dump`] for why the child needs it.
#[no_mangle]
pub extern "C" fn gex_shared_count() -> u32 {
    with_jit_only(|j| j.shared.len() as u32)
}

/// Write the identity map to `out`: a `u64` page, a `u64` key and a `u32`
/// base, twenty bytes each, returning how many were written. -1 if `cap` will
/// not hold them.
///
/// `execve` throws the page table away (its addresses describe the old image),
/// but the identity map says the discarded slots still hold libc from the same
/// file and offset, so the new image's copy can reuse them -- without it every
/// `fork`+`execve` re-translated libc. Handed over as identities, not page
/// assignments, so it is safe for a process that is not a copy: the key names
/// the object, offset and size, and the miss path re-checks it against what the
/// receiver actually mapped.
#[no_mangle]
pub extern "C" fn gex_shared_dump(out: u32, cap: u32) -> i32 {
    with_jit_only(|j| {
        let n = j.shared.len();
        if (n * 20) as u32 > cap {
            return -1;
        }
        let dst = shim_slice_mut(out, (n * 20) as u32);
        for (i, ((page, key), base)) in j.shared.iter().enumerate() {
            dst[i * 20..i * 20 + 8].copy_from_slice(&page.to_le_bytes());
            dst[i * 20 + 8..i * 20 + 16].copy_from_slice(&key.to_le_bytes());
            dst[i * 20 + 16..i * 20 + 20].copy_from_slice(&base.to_le_bytes());
        }
        n as i32
    })
}

/// Take the identity map from the process this one was forked from.
///
/// `count` records in the layout [`gex_shared_dump`] writes. Recorded, not
/// overwritten: a page that already has an owner keeps it, since emitted code
/// names whichever base it was given.
#[no_mangle]
pub extern "C" fn gex_shared_seed(ptr: u32, count: u32) -> u32 {
    phase(4);
    let src = shim_slice(ptr, count.saturating_mul(20));
    with_jit_only(|j| {
        let mut n = 0u32;
        for r in src.chunks_exact(20) {
            let page = u64::from_le_bytes(r[0..8].try_into().unwrap());
            let key = u64::from_le_bytes(r[8..16].try_into().unwrap());
            let base = u32::from_le_bytes(r[16..20].try_into().unwrap());
            if let std::collections::hash_map::Entry::Vacant(v) =
                j.shared.entry((page, key))
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
/// Reported, not written down twice: it is V8's 10M cap rounded to whole
/// dispatch pages, and a glue disagreeing about where table 1 starts would put
/// blocks in the wrong table.
#[no_mangle]
pub extern "C" fn gex_slots_per_table() -> u32 {
    gex_xlate::PageTable::SLOTS_PER_TABLE as u32
}

/// What this process's address space went on, in MiB: 0 guest mappings, 1 the
/// runtime's own heap, 2 Worker stacks.
///
/// One memory per process, and it can only grow, so what a process spends here
/// is what the tab pays for it -- times the number of processes alive at once.
/// Reported per process because that product is the whole memory story and
/// neither half of it was visible.
#[no_mangle]
pub extern "C" fn gex_arena_spent_mib(which: u32) -> u32 {
    (ARENA_SPENT
        .get(which as usize)
        .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(0)
        >> 20) as u32
}

/// How many dispatch tables the emitter was built for.
#[no_mangle]
pub extern "C" fn gex_dispatch_tables() -> u32 {
    gex_abi::DISPATCH_TABLES as u32
}

/// Runtime dispatch slots the whole page has left, in thousands (a `u32` will
/// not hold the count). Reported per process because it is a budget the run
/// spends and never refills, and a process that spends a large piece of it is
/// invisible otherwise -- the symptom, tens of processes later, is a module
/// that names a dispatch table nobody has.
#[no_mangle]
pub extern "C" fn gex_slots_left_k() -> u32 {
    // SAFETY: takes nothing, returns a scalar, borrows nothing here.
    unsafe {
        if k_present() == 0 {
            return u32::MAX;
        }
        (k_slot_left() / 1000) as u32
    }
}

/// Publish the pending module's pages, now that its blocks are in the table.
///
/// Deliberately last: until the directory entry points at real slots, a guest
/// branch into one of these pages must keep reaching the miss stub.
#[no_mangle]
pub extern "C" fn gex_jit_installed() {
    with_jit_only(|j| {
        let Some(done) = j.pending.pop_front() else {
            // Empty, but this Worker's published view may still name a module a
            // sibling already took: the queue is shared between a process's
            // Workers, so another can empty it between this one's peek and pop.
            // Without re-publishing here, `gex_jit_pending_len` keeps returning
            // the stale length and the JS install loop
            // (`while (gex_jit_pending_len()) …`) spins forever on a module that
            // is gone -- the thread never resumes and the process wedges. Clear
            // the view (front is `None`, so this stores zero) and stop.
            publish_pending(j);
            return;
        };
        // Its slots point at its blocks (`dense`), before the directory
        // names its pages.
        if let Some(d) = &done.dense {
            dense::publish(d);
        }
        let mut gm = GuestMem::new(guest_bytes());
        for (page, base) in done.dirs {
            // A page has ONE base per process. A served module built elsewhere
            // names its own bases; where this process already numbered the page
            // (and wrote blocks at that base), pointing the directory at the
            // served base strands those blocks and dispatches through slots
            // that may hold anything -- the two-bases-per-page wild branch the
            // x86 pipeline runs hit (`ls | cat`: 4096 slots a page made it
            // common). The served blocks on such a page are simply unreachable;
            // a miss there lifts locally, at the base the process has.
            if gex_abi::X86 {
                match j.page_table.lookup(page) {
                    Some(have) if have != base => {
                        dir_note(page, have, "install-done-kept-local");
                        continue;
                    }
                    // Retired while this module waited to be installed (its
                    // page table had the page when it was queued): its blocks
                    // are of the code before the change, so the page stays
                    // retired and the next branch there lifts what is there
                    // now. Assigning the old base back is what ran stale code.
                    None if j.retired_pages.contains(&page) => {
                        dir_note(page, base, "install-done-retired");
                        continue;
                    }
                    None => j.page_table.assign(page, base),
                    _ => {}
                }
            }
            dir_note(page, base, "install-done");
            let _ = gm.write_u32(gex_abi::DISPATCH_DIR + page * 4, base);
        }
        // Kept only for a process that has run a second thread-Worker: the
        // registry exists solely so a joining Worker can instantiate every
        // translated module into its own tables to catch up. A single-threaded
        // process -- which every `apt` process is -- never gains one, so
        // keeping the bytes is pure loss against a fixed ~2 GiB heap.
        //
        // This is the regression the OOM was: before threads-as-Workers the
        // page loaded the plain build, where this push was `#[cfg]`'d out and
        // nothing was ever kept; the shared build kept it for *every* process,
        // and `apt-get update` under `?closure=0` filled the heap with ~1,640
        // one-MiB modules in a single process and aborted. A joining thread now
        // replays what was kept from the moment it spawned and translates the
        // rest on the ordinary miss path, which is exactly what a fresh process
        // does -- so a plain, single-threaded run keeps nothing, as it did
        // before sharing existed.
        //
        // Shrunk before keeping: `encode_with_custom` starts each module at a
        // 1 MiB capacity, and a batch averages ~26 KB, so a retained module
        // held a megabyte for twenty-six kilobytes.
        let ext = NEXT_EXTERNAL.with(|n| n.take());
        if j.ever_threaded {
            // Already in the JS-side store, and the same module (a sibling can
            // take the front of the shared queue between this Worker's read and
            // this pop, so the note is checked against what was popped): keep
            // only where it is.
            if let Some((off, len, key)) = ext.filter(|&(_, len, key)| len as usize == done.wasm.len() && key == content_key(&done.wasm)) {
                EXTERNAL_BYTES.fetch_add(len as u64, std::sync::atomic::Ordering::Relaxed);
                j.registry_sums.push(Vec::new());
                regview::publish_module(j.registry.len(), Vec::<u8>::new().as_ptr() as u32, len, Some((off, key)));
                j.registry.push(Vec::new());
                j.registry_ext.push(Some((off, len, key)));
                j.registry_dense.push(done.dense.as_ref().map_or((0, 0), |d| (d.base, d.entries.len() as u32)));
                j.store_grace.push_back(done.wasm);
                if j.store_grace.len() > STORE_GRACE {
                    j.store_grace.pop_front();
                }
                publish_pending(j);
                return;
            }
            let mut wasm = done.wasm;
            wasm.shrink_to_fit();
            // Counted for the arena report (which cannot borrow the host,
            // running in the allocator's refill path). Nothing trims `registry`.
            #[cfg(feature = "shared-memory")]
            REGISTRY_BYTES.fetch_add(wasm.len() as u64, std::sync::atomic::Ordering::Relaxed);
            j.registry_sums.push(chunk_sums(&wasm));
            regview::publish_module(j.registry.len(), wasm.as_ptr() as u32, wasm.len() as u32, None);
            j.registry.push(wasm);
            j.registry_ext.push(None);
            j.registry_dense.push(done.dense.as_ref().map_or((0, 0), |d| (d.base, d.entries.len() as u32)));
        }
        publish_pending(j);
    });
}

/// Say where the guest was when everything stopped.
///
/// A trap reaches JS as `RuntimeError` with no frames and no address, so the
/// guest's registers are the only other evidence: a wild pointer in `free` and
/// a stack off its mapping look identical from outside and different here.
///
/// Deliberately tolerant: this runs on the way out of a crash, so anything
/// it cannot read it prints as zero rather than failing a second time.
#[no_mangle]
pub extern "C" fn gex_dump_state() {
    if gex_has_host() == 0 {
        return;
    }
    (|| {
        let Some(home) = sigs::home() else { return Default::default() };
        let gm = GuestMem::new(guest_bytes());
        let c = my_ctx();
        let r = |i: u8| gm.read_u64(c + ctx::x(i) as u64).unwrap_or(0);
        let sp = gm.read_u64(c + ctx::SP as u64).unwrap_or(0);
        trace_line(&format!(
            "gex: pid {} thread {} sp={sp:#x} x0={:#x} x1={:#x} x2={:#x} x30={:#x} \
             x19={:#x} x20={:#x} x21={:#x} x22={:#x} x23={:#x} x24={:#x}\n",
            home.pid(),
            cur(),
            r(0),
            r(1),
            r(2),
            r(30),
            r(19),
            r(20),
            r(21),
            r(22),
            r(23),
            r(24),
        ));
        // Which mapping each of those addresses is in, if any. An address
        // with no vma is the whole diagnosis: the guest followed a pointer
        // into nothing, which on Linux is `SIGSEGV` and here is a trap that
        // takes the Worker with it.
        let where_is = |a: u64| -> String {
            match home.mm().lock().vmas().iter().find(|v| a >= v.start && a < v.end) {
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
        if gex_abi::X86 {
            // Every GPR: with variable-length instructions and no link
            // register, which value went wild is not guessable from a few.
            const NAMES: [&str; 16] = [
                "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11",
                "r12", "r13", "r14", "r15",
            ];
            for (i, name) in NAMES.iter().enumerate() {
                let a = r(i as u8);
                warn_line(&format!("gex:   {name}={a:#x} in {}\n", where_is(a)));
            }
            let fs = gm.read_u64(c + ctx::FS_BASE as u64).unwrap_or(0);
            let last = gm.read_u64(c + ctx::LAST_TARGET as u64).unwrap_or(0);
            warn_line(&format!("gex:   fs_base={fs:#x} in {}  last_target={last:#x} in {}\n", where_is(fs), where_is(last)));
            return;
        }
        for (name, a) in [
            ("sp", sp),
            ("x0", r(0)),
            ("x1", r(1)),
            ("x16", r(16)),
            ("x17", r(17)),
            ("x30", r(30)),
        ] {
            warn_line(&format!("gex:   {name}={a:#x} in {}\n", where_is(a)));
        }
    })();
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
    // No kernel lock: the end is claimed as `exit_process` claims it, and a
    // process already ending (or with a status) is left to that.
    let Some(home) = sigs::home() else { return };
    if home.exit().status().is_some() || !home.exit().claim() {
        return;
    }
    let pid = home.pid();
    trace_line(&format!("[proc] abandon {pid}: the Worker stopped without an exit\n"));
    // The descriptors, on the same terms as `exit_process`: a status alone
    // ends a `wait4` and nothing else. A parent reading a dead child's pipe
    // -- which is every `$(...)`, and every apt method -- is not waiting on
    // the status, it is waiting for the end of file that only the last close
    // of the write end produces. Without this the pipe keeps a writer that
    // no longer exists and the parent's `read` never returns: the run wedges
    // one process further along than the one that died, which is the shape
    // that made a lifter fault in a forked child look like a hang in `apt`.
    if let Some(fds) = fds_free() {
        fds.close_all();
    }
    // SAFETY: takes a scalar and borrows nothing here.
    unsafe {
        if k_present() != 0 {
            k_pipe_forget(pid);
            k_pty_forget(pid);
        }
    }
    publish_exit(pid, code);
}

/// Make this process's exit visible to its parent (`wait4`, `waitid`,
/// `SIGCHLD`), after everything it wrote has reached the host. Output from
/// different processes reaches the host by different routes (a Worker's
/// messages, which nothing orders against another Worker's), so a parent that
/// waited for this process could otherwise print before this process's last
/// output did.
fn publish_exit(pid: i32, status: i32) {
    // SAFETY: no arguments; the host only waits for its own deliveries.
    unsafe { drain_output() };
    if let Some(home) = sigs::home() {
        home.procs().exit(pid, status);
    }
}

/// Record a guest exit, and say so where a trap handler can see it.
fn set_exit(code: i32) {
    if let Some(home) = sigs::home() {
        home.exit().record(code);
    }
    set_exited(true);
    waitq::wake_process();
}

/// How a process ended, decided once and read with no lock (L24): the
/// status the Worker reports (`gex_exit_code`), and whether a thread has
/// claimed the end (`exit_process`). One per process, in its `Home`.
pub(crate) struct ExitState {
    /// The status, or [`ExitState::NONE`].
    status: std::sync::atomic::AtomicI64,
    /// Whether the end has been claimed: its teardown runs once.
    claimed: std::sync::atomic::AtomicBool,
}

impl ExitState {
    const NONE: i64 = i64::MIN;

    pub(crate) const fn new() -> Self {
        Self {
            status: std::sync::atomic::AtomicI64::new(Self::NONE),
            claimed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The status recorded, if any.
    pub(crate) fn status(&self) -> Option<i32> {
        let v = self.status.load(std::sync::atomic::Ordering::Acquire);
        (v != Self::NONE).then_some(v as i32)
    }

    /// Record the status (the latest recorded is the one reported).
    pub(crate) fn record(&self, code: i32) {
        self.status.store(code as i64, std::sync::atomic::Ordering::Release);
    }

    /// Claim the end: true for exactly one caller.
    pub(crate) fn claim(&self) -> bool {
        !self.claimed.swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    /// Whether the end is claimed, or a status recorded: someone is ending,
    /// or has ended, the process.
    pub(crate) fn over(&self) -> bool {
        self.claimed.load(std::sync::atomic::Ordering::Acquire) || self.status().is_some()
    }
}

/// This process's [`ExitState`]'s status, if it has one.
fn exit_status() -> Option<i32> {
    sigs::home().and_then(|h| h.exit().status())
}

/// Record that this process is gone, for one that crashed without saying so: a
/// wasm fault abandons the stack without running `exit_process`, so the status
/// is never published nor the descriptors released, and a dead child's held
/// pipe end deadlocks the parent's `read` and `wait4`.
///
/// Called from the Worker after `run` returns, while the shim can still reach
/// the global kernel. `status` is what the parent sees; a fault is reported as
/// a signal, not an exit code.
#[no_mangle]
pub extern "C" fn gex_process_gone(status: i32) -> u32 {
    if exited() {
        // `EXITED` is process-wide shared memory.  A sibling thread can set it
        // while the original process Worker is parked, then that Worker sees
        // the flag and leaves `gex.run` without having run `exit_process` in
        // *its* Host.  Republish from the process Worker unless this Host
        // already recorded the exit; the remote process table accepts this
        // idempotently and, crucially, wakes the parent's wait4.
        let pid = my_pid();
        if !sigs::home().is_some_and(|h| h.exit().over()) {
            publish_exit(pid, status);
        }
        return 0;
    }
    set_exited(true);
    waitq::wake_process();
    let pid = my_pid();
    let n = unsafe {
        if k_present() != 0 {
            let n = k_pipe_forget(pid);
            k_pty_forget(pid);
            n
        } else {
            0
        }
    };
    publish_exit(pid, status);
    warn_line(&format!(
        "[gex] pid {pid} crashed; {n} pipe ends released, status {status}\n"
    ));
    n
}

#[no_mangle]
pub extern "C" fn gex_exited() -> i32 {
    // Deliberately not `with_exec_lock`: this is asked on the way out of a trap,
    // which is exactly when the kernel may still be borrowed by whatever
    // trapped.
    exited() as i32
}

/// Set when a guest exit is recorded, so `gex_exited` need not borrow.
static EXITED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether this Worker's process has exited: its vfork child's, while one
/// runs here (vfork.rs), whose exit is not the parent's.
///
/// A Worker whose thread another thread's `execve` took away answers yes as
/// well: the process it ran is over for it, and every way out of the guest
/// that asks this ends the Worker without ending the process.
pub(crate) fn exited() -> bool {
    vfork::exited_here().unwrap_or_else(|| EXITED.load(std::sync::atomic::Ordering::Relaxed) || thread_gone())
}

/// Record the exit [`exited`] answers.
fn set_exited(v: bool) {
    if !vfork::set_exited_here(v) {
        EXITED.store(v, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The head of the pending queue, as three plain words read by JS between
/// handing the wasm stack back and taking it again, without touching the kernel
/// (which the reads cannot safely borrow).
///
/// Thread-local, not a shared static: `Jit::pending` lives in the per-Worker
/// `Host`, so each Worker has its own queue, and each export runs on the Worker
/// whose queue it asks about. A shared slot let two translating Workers
/// overwrite each other's `(ptr, len, slots)` between the JS length check and
/// the pointer read, reaching `WebAssembly.compile` as an empty buffer.
thread_local! {
    static PENDING: [std::sync::atomic::AtomicU32; 3] =
        const { [const { std::sync::atomic::AtomicU32::new(0) }; 3] };
}

/// Republish the head of the queue. Call after every push and pop.
fn publish_pending(j: &Jit) {
    // What the queue is holding, for the arena report -- a static because that
    // report runs in the allocator's refill path and cannot borrow the host,
    // and this is the one place with `h` every push and pop passes through.
    // The queue's front is all the host sees, so a slow installer piles whole
    // module images up behind it.
    PENDING_BYTES.store(
        j.pending.iter().map(|m| m.wasm.len() as u64).sum(),
        std::sync::atomic::Ordering::Relaxed,
    );
    PENDING_MAX.fetch_max(
        j.pending.len() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
    let (p, l, s) = j.pending.front().map_or((0, 0, 0), |m| {
        (m.wasm.as_ptr() as u32, m.wasm.len() as u32, m.slots as u32)
    });
    PENDING.with(|pending| {
        pending[0].store(p, std::sync::atomic::Ordering::Relaxed);
        pending[1].store(l, std::sync::atomic::Ordering::Relaxed);
        pending[2].store(s, std::sync::atomic::Ordering::Relaxed);
    });
}

thread_local! {
    /// The JS-side store slot (`offset, len, content key`) of the module this
    /// Worker is about to report installed: `gex_jit_note_external`.
    static NEXT_EXTERNAL: std::cell::Cell<Option<(u64, u32, u64)>> = const { std::cell::Cell::new(None) };
}

/// How many popped modules' bytes stay alive after going to the JS-side store.
/// A Worker's view of the queue (`gex_jit_pending_ptr`) is refreshed right
/// before it copies the bytes out, so only a pop in that instant can free them
/// under it, and it would take this many more pops to reuse the memory.
const STORE_GRACE: usize = 256;

/// Make this Worker's view of the pending queue (`gex_jit_pending_*`) the
/// queue's front now. The view is set only by this Worker's own pushes and
/// pops, so it may name a module a sibling installed long ago.
#[no_mangle]
pub extern "C" fn gex_jit_pending_refresh() {
    with_jit_only(|j| publish_pending(j));
}

/// Bytes of registry entries kept in the JS-side store instead of this heap.
static EXTERNAL_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `moduleContentKey` in gex.mjs, as one u64 (`a` high, `b` low): what a
/// module's store slot is checked against.
fn content_key(bytes: &[u8]) -> u64 {
    // A word at a time (little-endian), then the tail bytes: the same as
    // gex.mjs `moduleContentKey`, which hashes every module a Worker installs
    // and spent 0.65 s of a node start-up doing it a byte at a time.
    let mut a: u32 = 0x811c_9dc5;
    let mut b: u32 = 0x9e37_79b9;
    let mut step = |i: usize, x: u32| {
        a = (a ^ x).wrapping_mul(0x0100_0193);
        b = (b ^ x.wrapping_add(i as u32)).wrapping_mul(0x85eb_ca6b);
        b ^= b >> 13;
    };
    let words = bytes.len() / 4;
    for i in 0..words {
        step(i, u32::from_le_bytes(bytes[4 * i..4 * i + 4].try_into().unwrap()));
    }
    for (i, &x) in bytes[4 * words..].iter().enumerate() {
        step(words + i, x as u32);
    }
    ((a as u64) << 32) | b as u64
}

/// Whether the next `gex_jit_installed` keeps its module for joining Workers
/// (this process has threaded): only then is a JS-side store slot worth
/// writing.
#[no_mangle]
pub extern "C" fn gex_jit_keeps() -> u32 {
    regview::threaded() as u32
}

/// The module this Worker installs next is at `off` in the process's JS-side
/// module store, `len` bytes, with content key `key_hi:key_lo`
/// (`moduleContentKey`). `gex_jit_installed` then keeps the place, not the
/// bytes, if the module it pops matches.
#[no_mangle]
pub extern "C" fn gex_jit_note_external(off: u64, len: u32, key_hi: u32, key_lo: u32) {
    NEXT_EXTERNAL.with(|n| n.set(Some((off, len, ((key_hi as u64) << 32) | key_lo as u64))));
}

/// Where registry module `i` is in the JS-side store, plus one; 0 when its
/// bytes are in this module's memory (`gex_module_ptr`).
#[no_mangle]
pub extern "C" fn gex_module_ext(i: u32) -> u64 {
    regview::row(i).map_or(0, |r| r.ext.load(std::sync::atomic::Ordering::Relaxed))
}

/// Registry module `i`'s content key, high half (`which` 0) or low (1), when
/// it is in the JS-side store (whose writer gave it); 0 and 0 otherwise. So a
/// Worker catching up names the module without hashing its bytes.
#[no_mangle]
pub extern "C" fn gex_module_key(i: u32, which: u32) -> u32 {
    regview::row(i).map_or(0, |r| {
        let k = r.key.load(std::sync::atomic::Ordering::Relaxed);
        if which == 0 { (k >> 32) as u32 } else { k as u32 }
    })
}

/// MiB of registry modules kept in the JS-side store (1) and in this heap (0).
#[no_mangle]
pub extern "C" fn gex_registry_mib(external: u32) -> u32 {
    let b = if external != 0 { &EXTERNAL_BYTES } else { &REGISTRY_BYTES };
    (b.load(std::sync::atomic::Ordering::Relaxed) >> 20) as u32
}

/// How many translated modules this process has, for a Worker catching up.
#[no_mangle]
pub extern "C" fn gex_module_count() -> u32 {
    regview::count()
}

/// Where module `i`'s bytes are: a pointer, not a copy, since the registry
/// lives in the process memory a joining Worker already sees.
#[no_mangle]
pub extern "C" fn gex_module_ptr(i: u32) -> u32 {
    regview::row(i).map_or(0, |r| r.ptr.load(std::sync::atomic::Ordering::Relaxed))
}

/// Where registry module `i`'s row is in this process's memory, or 0: four
/// little-endian words -- pointer, length, store offset plus one (u64), key
/// (u64) -- all a Worker catching up asks about a module, in one call and
/// without the kernel's lock (`regview`).
#[no_mangle]
pub extern "C" fn gex_module_row(i: u32) -> u32 {
    regview::row(i).map_or(0, |r| r as *const regview::Row as u32)
}

/// Every slot this process has claimed, across every module.
///
/// What a Worker catching up must grow its tables to: `gex_jit_slots` reports
/// only the pending module's high-water, right for the installer but not for a
/// thread replaying the whole registry.
#[no_mangle]
pub extern "C" fn gex_jit_slots_total() -> u32 {
    match regview::slots() {
        Some(n) => n as u32,
        None => with_jit_only(|j| {
            regview::publish_marks(&j.page_table);
            j.page_table.slots() as u32
        }),
    }
}

/// How long module `i` is.
#[no_mangle]
pub extern "C" fn gex_module_len(i: u32) -> u32 {
    regview::row(i).map_or(0, |r| r.len.load(std::sync::atomic::Ordering::Relaxed))
}

/// The translation registry and the dispatch tables' lengths as a reader
/// holding no lock sees them (docs/kernel-locking.md, step 4).
///
/// JS asks about them all the time -- each table's need at every install,
/// each module's pointer, length, place and key while a Worker catches up --
/// and under the lock those questions were half of node -e 0's waiting. The
/// writers, who hold the lock, publish here as they go:
///
/// - each registry module's row, written before the count that makes it
///   visible is stored (release; readers acquire). Rows live in segments
///   that are allocated once and never freed or moved, so a row's address
///   stays good. The registry itself only grows; a new process (a forked
///   child copies these statics with its memory) starts it over with
///   `reset`.
/// - the page table's counters: every clone of a page table shares them, so
///   publishing is keeping the `Arc`s, done whenever `h.jit.page_table` is
///   assigned. That changes them only when a process starts and at each
///   `execve`; a replaced set is freed four replacements later, not at
///   once, because a reader may still hold it.
pub(crate) mod regview {
    use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
    use std::sync::Arc;

    #[repr(C)]
    pub struct Row {
        pub ptr: AtomicU32,
        pub len: AtomicU32,
        pub ext: AtomicU64,
        pub key: AtomicU64,
    }

    const SEG: usize = 1024;
    const SEGS: usize = 4096;
    static SEGMENTS: [AtomicPtr<Row>; SEGS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; SEGS];
    static COUNT: AtomicU32 = AtomicU32::new(0);

    pub fn count() -> u32 {
        COUNT.load(Ordering::Acquire)
    }

    pub fn row(i: u32) -> Option<&'static Row> {
        if i >= count() {
            return None;
        }
        let seg = SEGMENTS.get(i as usize / SEG)?.load(Ordering::Acquire);
        // SAFETY: a segment, once published, is SEG rows that are never freed
        // or moved, and row `i` was written before the count passed it.
        (!seg.is_null()).then(|| unsafe { &*seg.add(i as usize % SEG) })
    }

    /// Row `i` (the registry's length before the push), under the kernel's
    /// lock; visible to readers once written. `ext` is the JS-side store's
    /// offset and the content key, for a module kept there.
    pub fn publish_module(i: usize, ptr: u32, len: u32, ext: Option<(u64, u64)>) {
        let Some(slot) = SEGMENTS.get(i / SEG) else { return };
        let mut seg = slot.load(Ordering::Acquire);
        if seg.is_null() {
            let rows: Box<[Row]> = (0..SEG)
                .map(|_| Row { ptr: AtomicU32::new(0), len: AtomicU32::new(0), ext: AtomicU64::new(0), key: AtomicU64::new(0) })
                .collect();
            seg = Box::leak(rows).as_mut_ptr();
            slot.store(seg, Ordering::Release);
        }
        // SAFETY: as in `row`; only the lock holder writes.
        let r = unsafe { &*seg.add(i % SEG) };
        r.ptr.store(ptr, Ordering::Relaxed);
        r.len.store(len, Ordering::Relaxed);
        r.ext.store(ext.map_or(0, |(off, _)| off + 1), Ordering::Relaxed);
        r.key.store(ext.map_or(0, |(_, k)| k), Ordering::Relaxed);
        COUNT.store(i as u32 + 1, Ordering::Release);
    }

    /// A new, empty registry (a new process's host).
    pub fn reset() {
        COUNT.store(0, Ordering::Release);
        THREADED.store(false, Ordering::Release);
    }

    /// `Jit::ever_threaded`: whether the registry keeps modules at all.
    static THREADED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    pub fn set_threaded(on: bool) {
        THREADED.store(on, Ordering::Release);
    }

    pub fn threaded() -> bool {
        THREADED.load(Ordering::Acquire)
    }

    struct Marks {
        high: Arc<AtomicU64>,
        next: Arc<AtomicU64>,
        highs: Arc<Vec<AtomicU64>>,
    }
    static MARKS: AtomicPtr<Marks> = AtomicPtr::new(core::ptr::null_mut());
    /// Replaced mark sets, freed RETIRED_KEEP replacements later.
    const RETIRED_KEEP: usize = 4;
    static RETIRED: [AtomicPtr<Marks>; RETIRED_KEEP] = [const { AtomicPtr::new(core::ptr::null_mut()) }; RETIRED_KEEP];
    static RETIRED_N: AtomicU32 = AtomicU32::new(0);

    /// Keep `t`'s counters for lock-free readers, unless they are the ones
    /// already kept.
    pub fn publish_marks(t: &gex_xlate::PageTable) {
        let (high, next, highs) = t.marks();
        let had = MARKS.load(Ordering::Acquire);
        // SAFETY: a published `Marks` is never freed.
        if !had.is_null() && unsafe { Arc::ptr_eq(&(*had).highs, &highs) && Arc::ptr_eq(&(*had).high, &high) } {
            return;
        }
        MARKS.store(Box::leak(Box::new(Marks { high, next, highs })), Ordering::Release);
        // The set replaced is kept for readers that may still hold it, and
        // freed once RETIRED_KEEP later replacements have passed. Sets change
        // only when a process starts and at each `execve`, which leaves the
        // process one thread: a reader would have to sleep inside a getter
        // through that many execs. So at most RETIRED_KEEP + 1 sets exist
        // however long a process runs or however often it execs.
        if !had.is_null() {
            let n = RETIRED_N.fetch_add(1, Ordering::Relaxed) as usize;
            let old = RETIRED[n % RETIRED_KEEP].swap(had, Ordering::AcqRel);
            if !old.is_null() {
                // SAFETY: published by this function from a `Box`, unpublished
                // RETIRED_KEEP replacements ago, and in no slot any more.
                drop(unsafe { Box::from_raw(old) });
            }
        }
    }

    fn marks() -> Option<&'static Marks> {
        let m = MARKS.load(Ordering::Acquire);
        // SAFETY: as above.
        (!m.is_null()).then(|| unsafe { &*m })
    }

    /// `PageTable::table_need`, read lock-free.
    pub fn table_need(i: usize) -> Option<u64> {
        marks().map(|m| m.highs.get(i).map_or(0, |h| h.load(Ordering::SeqCst)))
    }

    /// `PageTable::slots`, read lock-free.
    pub fn slots() -> Option<u64> {
        marks().map(|m| m.high.load(Ordering::SeqCst).max(m.next.load(Ordering::SeqCst)))
    }
}

/// A direct-mapped table of back-edge counts, keyed by guest pc.
///
/// Direct-mapped and lossy on purpose (a shift, compare and add per safepoint):
/// collisions age each other out, costing accuracy only on a flat profile, not
/// the peaked one this exists to find.
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
/// Without `with_exec_lock`: read from inside the guest's own `write`, which already
/// holds it. Addresses are left raw (symbolising needs the VMA list and lock);
/// `gex_object_at` resolves them when the caller is somewhere safe.
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
/// Loud (a cold Mesa run makes eleven thousand), for one question: when a run
/// stops with the stack held and no other output, the last line is the block
/// the translator went into and did not return from.
static mut TRACE_MISSES: bool = false;

#[no_mangle]
pub extern "C" fn gex_set_trace_misses(on: i32) {
    unsafe { TRACE_MISSES = on != 0 };
}

/// Check the guest's stack pointer on entry to every block translated after
/// this, and stop at the first one that has a wild one.
///
/// The native side reads `GEX_CHECK_SP` from the environment; a page has none.
/// Only blocks translated afterwards carry the check, which keeps it off the
/// startup path and on the object under suspicion. For finding a bug, not
/// shipping.
#[no_mangle]
pub extern "C" fn gex_set_check_sp(on: i32) {
    gex_xlate::set_check_sp(on != 0);
}

/// Diagnostic: emit `memcpy` as a forward byte loop instead of `memory.copy`.
#[no_mangle]
pub extern "C" fn gex_set_copy_loop(on: i32) {
    gex_xlate::set_copy_loop(on != 0);
}

/// Experiment: emit guest scalar loads/stores as atomic-when-aligned so shared
/// reads are coherent (V8 cannot hoist them across guest fences). Default off.
#[no_mangle]
pub extern "C" fn gex_set_coherent_loads(on: i32) {
    gex_xlate::set_coherent_loads(on != 0);
}

/// Diagnostic: at each safepoint, check the running thread's SP against the
/// stacks it must NOT be in. A worker running with a stray SP that points into
/// the MAIN thread's stack writes into it and smashes a canary a quiet main
/// thread only detects much later -- the exact shape of the complex-Qt
/// (designer) stack smash. Off by default; loud when it fires. See
/// `gex_safepoint`.
static SP_GUARD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[no_mangle]
pub extern "C" fn gex_set_sp_guard(on: i32) {
    SP_GUARD.store(on != 0, std::sync::atomic::Ordering::Relaxed);
}

/// Plant a diagnostic breakpoint at guest pc `pc` (0 = off). See
/// `gex_xlate::set_trap_pc` and `tier1::emit_trap_pc`.
#[no_mangle]
pub extern "C" fn gex_set_trap_pc(pc: i64) {
    gex_xlate::set_trap_pc(pc as u64);
}

/// Ask for a heartbeat every `every` safepoints.
///
/// Its own switch, not a level of `gex_set_trace`: a syscall trace of a
/// Mesa-translating run is hundreds of megabytes and drowns the one line a
/// second that says whether the guest is moving.
#[no_mangle]
pub extern "C" fn gex_set_heartbeat(every: i64) {
    unsafe { HEARTBEAT = every.max(0) as u64 };
}

/// Blocks lifted, by object, as `name=count name=count` into `out`: the total
/// cannot tell a closure that is missing something from a program with its own
/// code to translate.
#[no_mangle]
pub extern "C" fn gex_jit_by_object(out: u32, max: u32) -> u32 {
    let text = with_jit_only(|j| {
        let mut v: Vec<(&String, &(u64, u64, u64))> = j.by_object.iter().collect();
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
    with_jit_only(|j| j.blocks as i64)
}

#[no_mangle]
pub extern "C" fn gex_jit_compiles() -> i64 {
    with_jit_only(|j| j.compiles as i64)
}

/// Blocks lifted on a page a closure library already claims.
///
/// A process lifting blocks of a library it holds whole is either failing to
/// ask for the fill or being told no -- different fixes -- and every one is a
/// block static recompilation already produced.
#[no_mangle]
pub extern "C" fn gex_lift_in_closure() -> i64 {
    with_jit_only(|j| j.lift_in_closure as i64)
}

/// How the shared-slot lookup went, packed so one call answers.
///
/// One field by index: four 16-bit fields packed into one word overflow for a
/// program that misses a hundred thousand times.
#[no_mangle]
pub extern "C" fn gex_reuse_stat(which: u32) -> i64 {
    with_jit_only(|j| {
        (match which {
            0 => j.reuse_have,
            1 => j.reuse_hit,
            2 => j.reuse_miss,
            4 => j.reuse_served,
            5 => j.store_ok,
            6 => j.store_full,
            7 => SERVE_NONE.with(|c| c.borrow().0),
            8 => SERVE_NONE.with(|c| c.borrow().1),
            9 => NOIDENT_WHY.with(|c| c.borrow().0),
            10 => NOIDENT_WHY.with(|c| c.borrow().1),
            _ => j.reuse_noident,
        }) as i64
    })
}

/// Milliseconds the shim itself spent translating.
#[no_mangle]
pub extern "C" fn gex_jit_millis() -> i64 {
    with_jit_only(|j| (j.nanos / 1_000_000) as i64)
}

/// `GEX_EXC_TRACE`-style narration of NtRaiseException's nesting decision
/// (set from JS with `gex_set_exc_trace`).
static EXC_TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub(crate) fn tracing_exc() -> bool {
    EXC_TRACE.load(std::sync::atomic::Ordering::Relaxed)
}
#[no_mangle]
pub extern "C" fn gex_set_exc_trace(on: i32) {
    EXC_TRACE.store(on != 0, std::sync::atomic::Ordering::Relaxed);
}

/// A line on the guest's standard error (delivered in order with its output).
pub(crate) fn stderr_line(s: &str) {
    unsafe { write(2, s.as_ptr() as usize, s.len() as u32) };
}
