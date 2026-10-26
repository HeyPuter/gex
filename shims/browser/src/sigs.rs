//! Signals without the kernel lock.
//!
//! A process's signal state is two kinds of shared object
//! ([`gex_kernel::signal`]): the process's `ProcSig` (dispositions, the
//! process-directed pending word, the interval timers) and each thread's
//! `ThreadSig` (its own pending word and mask). Both are reference-counted
//! and synchronise themselves, so a Worker asks whether its own thread has a
//! signal to take, and builds the handler frame for it, without the kernel:
//! what a safepoint does a thousand back edges at a time.

use crate::{k_present, k_signal_seq};
use gex_kernel::proc::ProcTable;
use gex_kernel::signal::{self, Delivery, ProcSig, Signals, ThreadSig};
use gex_kernel::futex::FutexTable;
use gex_kernel::mem::MapView;
use gex_kernel::syscalls::{self, ClockFn, FdIo, Free, FreeCall, FreeOutcome, FutexCx, MapIo, PathIo};
use gex_kernel::{GuestMem, Kernel};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;

/// What this process's signals need that is not in the signal state: where
/// other processes leave theirs, and the clock its timers run on. Constant
/// for the process's life.
pub(crate) struct Home {
    pub sig: Arc<ProcSig>,
    procs: Arc<ProcTable>,
    pub pid: i32,
    /// Its user and group ids (`Kernel::creds`, the same ones): a set
    /// under the lock or without it is every thread's next read.
    creds: Arc<gex_kernel::syscalls::Creds>,
    /// Its threads' signal states (`Kernel::thread_signals`), for a
    /// `tgkill` to one.
    threads: Arc<gex_kernel::signal::ThreadSigs>,
    clock: ClockFn,
    /// Its futexes and its memory map's published view, for `futex`
    /// without the kernel.
    pub futexes: Arc<FutexTable>,
    map: Arc<MapView>,
    /// Its namespace's published view, for a descriptor rebuilt from its
    /// name (`recvmsg`'s `SCM_RIGHTS`).
    vfs: Arc<gex_kernel::vfs::VfsView>,
    /// Its terminal, for `ioctl`.
    tty: Arc<gex_kernel::tty::TtyRef>,
    /// `flock`'s locks, released by the last `close` of a description.
    file_locks: Arc<gex_kernel::unix::FileLocks>,
    /// The memory map itself (`Mm::shared`), for the mapping calls, with
    /// the placement they ask (`Kernel::pins`, `Kernel::place`).
    mm: gex_kernel::mem::Mm,
    pins: Arc<std::collections::HashMap<String, u64>>,
    place: Option<Arc<dyn gex_kernel::syscalls::ObjectPlace>>,
    /// For the path calls: the threads' own fs_structs, the group, the
    /// terminals and `O_TMPFILE`'s serial (`Kernel::path_io`).
    thread_fs: Arc<gex_kernel::syscalls::ThreadFs>,
    ptys: Option<gex_kernel::pty::Remote>,
    tmpfiles: Arc<std::sync::atomic::AtomicU64>,
    /// The process's first fs_struct (its directory and umask).
    proc_fs: Arc<gex_kernel::syscalls::FsCwd>,
    /// The global kernel's pipes, for `pipe2`, and the resource limits,
    /// for `prlimit64`.
    pipes: Option<gex_kernel::pipe::Remote>,
    rlimits: Arc<gex_kernel::syscalls::Rlimits>,
    /// For the socket calls: the resolver and the ports, the global
    /// kernel's sockets, and the socket name tables (the network is the
    /// page's, `crate::page_net`).
    resolver: Arc<gex_kernel::net::Resolver>,
    ports: Arc<gex_kernel::net::Ports>,
    unix_remote: Option<gex_kernel::unix::Remote>,
    socket_names: Arc<gex_kernel::unix::SocketNames>,
    host_names: Arc<gex_kernel::unix::SocketNames>,
    /// Its threads, their contexts and run states (`Host::threads`).
    table: Arc<crate::Threads>,
    /// Its live thread count (`Kernel::live_threads`), for `exit`.
    live: Arc<std::sync::atomic::AtomicU32>,
    /// Its next free register-context slot (`Kernel::next_ctx_slot`), for
    /// `clone`.
    slots: Arc<std::sync::atomic::AtomicU32>,
    /// How it ended, decided once.
    exit: crate::ExitState,
    /// Its image, command line and environment as `/proc` shows them
    /// (`Kernel::publish_proc`, at its start and each exec).
    image: Arc<gex_kernel::proclive::ProcCell>,
}

/// This process's [`Home`], published by `set_host`. A new one replaces it
/// only when a new process takes over the memory, which happens once per
/// process; the old one is left, a few words, rather than freed under a
/// reader.
static HOME: AtomicPtr<Home> = AtomicPtr::new(std::ptr::null_mut());

thread_local! {
    /// This Worker's thread's signal state, and its tid.
    static MINE: RefCell<Option<(i32, Arc<ThreadSig>)>> = const { RefCell::new(None) };
    /// `k_signal_seq` (or the table's own sequence) as this Worker last
    /// collected at.
    static SEQ_SEEN: Cell<u32> = const { Cell::new(0) };
    /// [`view`] as last built, and the `Home` it was built from: this
    /// Worker's own, so a call takes no count on the signal state that
    /// every Worker of the process shares. Forgotten when the thread or the
    /// process changes (`bind`, `bind_free`, `publish_here`).
    static VIEW: RefCell<Option<(*const Home, Rc<Signals>)>> = const { RefCell::new(None) };
    /// The namespace this Worker's calls last read (`Home::vfs`), from which
    /// view and at which epoch ([`namespace`]): kept while the view's epoch
    /// stays, so a call takes no count on it. Forgotten with the process
    /// (`publish_here`).
    static NS: RefCell<Option<(*const gex_kernel::vfs::VfsView, u64, Rc<Arc<gex_kernel::vfs::Namespace>>)>> = const { RefCell::new(None) };
}

/// Publish `k`'s process as this memory's (`set_host`).
pub(crate) fn publish(k: &Kernel, table: &Arc<crate::Threads>, image: &Arc<gex_kernel::proclive::ProcCell>) {
    HOME.store(Box::into_raw(Box::new(home_of(k, table, image))), Ordering::Release);
}

fn home_of(k: &Kernel, table: &Arc<crate::Threads>, image: &Arc<gex_kernel::proclive::ProcCell>) -> Home {
    Home {
        sig: Arc::clone(k.signals.process()),
        procs: Arc::clone(&k.procs),
        pid: k.pid,
        creds: Arc::clone(&k.creds),
        threads: Arc::clone(&k.thread_signals),
        clock: k.clock,
        futexes: Arc::clone(&k.futexes),
        map: Arc::clone(k.mem.view()),
        vfs: Arc::clone(k.vfs.view()),
        tty: Arc::clone(&k.tty),
        file_locks: Arc::clone(&k.file_locks),
        mm: k.mem.shared(),
        pins: Arc::clone(&k.pins),
        place: k.place.clone(),
        thread_fs: Arc::clone(&k.thread_fs),
        ptys: k.ptys,
        tmpfiles: Arc::clone(&k.tmpfiles),
        proc_fs: Arc::clone(&k.proc_fs),
        pipes: k.pipes,
        rlimits: Arc::clone(&k.rlimits),
        resolver: Arc::clone(&k.resolver),
        ports: Arc::clone(&k.ports),
        unix_remote: k.unix_remote,
        socket_names: Arc::clone(&k.socket_names),
        host_names: Arc::clone(&k.host_names),
        table: Arc::clone(table),
        live: Arc::clone(&k.live_threads),
        slots: Arc::clone(&k.next_ctx_slot),
        exit: crate::ExitState::new(),
        image: Arc::clone(image),
    }
}

impl Home {
    /// The process's memory map, the kernel's own (`Mm::shared`).
    pub(crate) fn mm(&self) -> &gex_kernel::mem::Mm {
        &self.mm
    }

    /// Its namespace as published now.
    pub(crate) fn ns(&self) -> Arc<gex_kernel::vfs::Namespace> {
        self.vfs.get()
    }

    pub(crate) fn pid(&self) -> i32 {
        self.pid
    }

    /// Its threads (`Host::threads`), which lock themselves.
    pub(crate) fn table(&self) -> &crate::Threads {
        &self.table
    }

    /// Its image's path, as last published.
    pub(crate) fn exe(&self) -> String {
        self.image.exe()
    }

    /// Its argument vector, as last published.
    pub(crate) fn argv(&self) -> Vec<String> {
        self.image.argv()
    }

    /// Its environment, as last published.
    pub(crate) fn envp(&self) -> Vec<String> {
        self.image.envp()
    }

    /// How it ended (`exit_process`), which needs no lock.
    pub(crate) fn exit(&self) -> &crate::ExitState {
        &self.exit
    }

    /// Its user and group ids, which lock themselves.
    pub(crate) fn creds(&self) -> &gex_kernel::syscalls::Creds {
        &self.creds
    }

    /// Its terminal, which locks itself.
    pub(crate) fn tty(&self) -> &gex_kernel::tty::TtyRef {
        &self.tty
    }

    /// The process table, which locks itself.
    pub(crate) fn procs(&self) -> &ProcTable {
        &self.procs
    }

    /// The socket name table, which locks itself.
    pub(crate) fn socket_names(&self) -> &gex_kernel::unix::SocketNames {
        &self.socket_names
    }

    /// Up to `out.len()` bytes of `path` from `offset`, through the
    /// published namespace (no kernel lock); bytes filled.
    pub(crate) fn read_file_at(&self, path: &str, offset: u64, out: &mut [u8]) -> Option<usize> {
        let f = self.vfs.get().open(path, 0, 0).ok()?;
        let mut done = 0usize;
        while done < out.len() {
            let n = f.read_at(offset + done as u64, &mut out[done..]).ok()? as usize;
            if n == 0 {
                break;
            }
            done += n;
        }
        Some(done)
    }

    /// The whole of `path` (at most 512 MiB), as [`Home::read_file_at`].
    pub(crate) fn read_file(&self, path: &str) -> Option<Vec<u8>> {
        let f = self.vfs.get().open(path, 0, 0).ok()?;
        let mut out = Vec::new();
        let mut buf = vec![0u8; 65536];
        loop {
            let n = f.read_at(out.len() as u64, &mut buf).ok()? as usize;
            if n == 0 {
                return Some(out);
            }
            out.extend_from_slice(&buf[..n]);
            if out.len() > 512 * 1024 * 1024 {
                return None;
            }
        }
    }

    /// The executable mappings of files, as the log names new ones.
    pub(crate) fn code_maps(&self) -> Vec<gex_kernel::CodeMap> {
        use gex_kernel::mem::prot;
        self.mm
            .lock()
            .vmas()
            .iter()
            .filter(|v| v.prot & prot::EXEC != 0)
            .filter_map(|v| v.code.as_ref().map(|(p, off)| gex_kernel::CodeMap { start: v.start, end: v.end, path: p.clone(), offset: *off }))
            .collect()
    }
}

/// This Worker's process, for the work after a syscall that runs without
/// the kernel lock (the mapped-code drains).
pub(crate) fn proc_home() -> &'static Home {
    home().expect("mapped code before its process was published")
}

thread_local! {
    /// A vfork child's [`Home`] while it runs in this Worker of its parent
    /// (`publish_here`): this Worker's process is the child's, the memory's
    /// other Workers the parent's.
    static HERE: Cell<*mut Home> = const { Cell::new(std::ptr::null_mut()) };
}

/// Publish a vfork child's process for this Worker alone, or (`None`) go
/// back to the memory's (vfork.rs).
pub(crate) fn publish_here(k: Option<(&Kernel, &Arc<crate::Threads>, &Arc<gex_kernel::proclive::ProcCell>)>) {
    let new = k.map_or(std::ptr::null_mut(), |(k, table, image)| Box::into_raw(Box::new(home_of(k, table, image))));
    let old = HERE.with(|c| c.replace(new));
    if !old.is_null() {
        // SAFETY: made above by an earlier call, and only this Worker reads
        // it, never across a call to here.
        drop(unsafe { Box::from_raw(old) });
    }
    // The next collection is the new process's, and so are the view and
    // the namespace (the old `Home` is gone, and its address may come back).
    SEQ_SEEN.with(|c| c.set(0));
    VIEW.with(|v| v.borrow_mut().take());
    NS.with(|n| n.borrow_mut().take());
}

pub(crate) fn home() -> Option<&'static Home> {
    let here = HERE.with(|c| c.get());
    if !here.is_null() {
        // SAFETY: freed only by `publish_here`, on this Worker, between calls.
        return unsafe { here.as_ref() };
    }
    // SAFETY: a published `Home` is never freed (see `HOME`).
    unsafe { HOME.load(Ordering::Acquire).as_ref() }
}

/// Make thread `tid` this Worker's, under the kernel lock (`set_current`).
pub(crate) fn bind(k: &mut Kernel, tid: i32) {
    MINE.with(|m| {
        let mut m = m.borrow_mut();
        if m.as_ref().is_some_and(|(t, _)| *t == tid) {
            return;
        }
        *m = Some((tid, k.thread_sig(tid)));
        VIEW.with(|v| v.borrow_mut().take());
    });
}

/// Make thread `tid`, one the process already has, this Worker's, with no
/// kernel lock (a thread's adoption): its signal state from the
/// process's table, where its `clone` put it; a thread made otherwise (an
/// NT thread) gets one of its own, blocking nothing.
pub(crate) fn bind_free(home: &Home, tid: i32) {
    MINE.with(|m| {
        let mut m = m.borrow_mut();
        if m.as_ref().is_some_and(|(t, _)| *t == tid) {
            return;
        }
        let own = home.threads.get_or_insert_with(tid, || Arc::new(ThreadSig::new_thread(0)));
        *m = Some((tid, own));
        VIEW.with(|v| v.borrow_mut().take());
    });
}

/// Bring in what other processes have sent this one, when anyone has sent
/// anything since this Worker last looked, and what its timers have raised.
fn gather(home: &Home) {
    let seq = if unsafe { k_present() } != 0 {
        unsafe { k_signal_seq() as u32 }
    } else {
        gex_kernel::proc::signal_seq()
    };
    if SEQ_SEEN.with(|c| c.replace(seq)) != seq {
        home.sig.collect(&home.procs, home.pid, home.creds.uid());
        // Another process's `tgkill` to this Worker's thread: its own, so
        // taken here (the kernel's `collect_signals` takes the rest, and
        // drops those for threads gone).
        MINE.with(|m| {
            if let Some((tid, t)) = m.borrow().as_ref() {
                for (_, signo, from) in home.procs.take_thread_signals(home.pid, *tid) {
                    t.raise_from(signo, signal::Source { code: signal::SI_TKILL, pid: from, uid: home.creds.uid() });
                }
            }
        });
    }
    if home.sig.timers_armed() {
        let (secs, nanos) = (home.clock)();
        home.sig.fire_timers(secs as u128 * 1_000_000_000 + nanos as u128);
    }
}

/// Whether a signal would do something to this Worker's thread: a handler,
/// or a default that is not "ignore". No kernel lock.
pub(crate) fn ready() -> bool {
    let Some(home) = home() else { return false };
    gather(home);
    MINE.with(|m| m.borrow().as_ref().is_some_and(|(_, t)| t.ready(&home.sig)))
}

/// Deliver the signal [`ready`] found to this Worker's thread, whose
/// context is at `ctx_addr`: build its handler frame, or say the process
/// ends. No kernel lock.
pub(crate) fn deliver(gm: &mut GuestMem<'_>, ctx_addr: u64) -> Delivery {
    let Some(home) = home() else { return Delivery::None };
    let Some(view) = view(home) else { return Delivery::None };
    dispatch(&view, gm, ctx_addr)
}

/// A synchronous fault against this Worker's thread (the kernel's
/// `raise_fault`, without it): the handler's frame on the context at
/// `ctx_addr`, entered at `pc`, or the process ends when there is none, it
/// is ignored, or the signal is blocked. No kernel lock.
pub(crate) fn fault(gm: &mut GuestMem<'_>, ctx_addr: u64, signo: u32, pc: u64, addr: u64) -> Delivery {
    let end = Delivery::Terminate { signal: signo, core: true };
    let Some(home) = home() else { return end };
    let Some(view) = view(home) else { return end };
    let action = view.action(signo);
    let blocked = view.blocked() & (1u64 << (signo - 1)) != 0;
    if action.handler == signal::SIG_DFL || action.handler == signal::SIG_IGN || blocked {
        return end;
    }
    if gm.write_u64(ctx_addr + gex_abi::ctx::PC as u64, pc).is_err() {
        return end;
    }
    view.forget_source(signo);
    let compat = crate::CLOCK_ABI.load(Ordering::Acquire) == 1;
    match signal::setup_frame(&view, gm, ctx_addr, signo, addr, compat) {
        Ok(target) => Delivery::Resume(target),
        Err(_) => end,
    }
}

/// This Worker's thread's signal state as one `Signals`.
fn view(home: &Home) -> Option<Rc<Signals>> {
    let at = home as *const Home;
    if let Some(v) = VIEW.with(|v| v.borrow().as_ref().filter(|(h, _)| *h == at).map(|(_, s)| Rc::clone(s))) {
        return Some(v);
    }
    let v = Rc::new(MINE.with(|m| m.borrow().as_ref().map(|(_, t)| Signals::view(Arc::clone(&home.sig), Arc::clone(t))))?);
    VIEW.with(|c| *c.borrow_mut() = Some((at, Rc::clone(&v))));
    Some(v)
}

/// The process's namespace as published (`Home::vfs`), as this Worker last
/// read it while that is still the current one (`NS`).
fn namespace(home: &Home) -> Rc<Arc<gex_kernel::vfs::Namespace>> {
    let at = Arc::as_ptr(&home.vfs);
    let epoch = home.vfs.epoch();
    if let Some(ns) = NS.with(|n| n.borrow().as_ref().filter(|(v, e, _)| *v == at && *e == epoch).map(|(_, _, ns)| Rc::clone(ns))) {
        return ns;
    }
    // The epoch read before the namespace: one published between them is
    // read again next time.
    let ns = Rc::new(home.vfs.get());
    NS.with(|n| *n.borrow_mut() = Some((at, epoch, Rc::clone(&ns))));
    ns
}

fn dispatch(view: &Signals, gm: &mut GuestMem<'_>, ctx_addr: u64) -> Delivery {
    // The ABI the frame is in: an i386 process's, or the native one's
    // (`clock_constants` publishes it at load and exec).
    let compat = crate::CLOCK_ABI.load(Ordering::Acquire) == 1;
    signal::dispatch(view, gm, ctx_addr, compat).unwrap_or(Delivery::None)
}

/// The signal half of finishing this Worker's thread's parked call, whose
/// answer is already in its context at `ctx_addr` (what the kernel's
/// `finish_blocking` does, without it): a write that met a closed pipe
/// raises `SIGPIPE` (`sigpipe`), `rt_sigsuspend`'s mask goes back through
/// the handler's frame or, with no frame, here (`suspended`), and a pending
/// signal is delivered. No kernel lock.
pub(crate) fn finish(gm: &mut GuestMem<'_>, ctx_addr: u64, suspended: Option<u64>, sigpipe: bool) -> Delivery {
    let Some(home) = home() else { return Delivery::None };
    let Some(view) = view(home) else { return Delivery::None };
    gather(home);
    if sigpipe {
        // Linux sends it as the writer itself sending it (SI_USER).
        view.raise_from(
            signal::sig::SIGPIPE,
            signal::Source { code: signal::SI_USER, pid: home.pid, uid: home.creds.uid() },
        );
    }
    if suspended.is_some() {
        view.set_suspend_restore(suspended);
    }
    let delivery = if view.has_pending() { dispatch(&view, gm, ctx_addr) } else { Delivery::None };
    // Nothing was delivered, so no frame took the mask: put it back here.
    if let Some(mask) = view.take_suspend_restore() {
        view.set_blocked(mask);
    }
    delivery
}

/// Whether this Worker's thread's parked call of restart class `class`,
/// ended by a signal, starts again ([`Signals::restarts`]). No kernel lock.
pub(crate) fn restarts(class: signal::Restart) -> bool {
    let Some(home) = home() else { return false };
    let Some(view) = view(home) else { return false };
    gather(home);
    view.restarts(class)
}

/// The call of the lock-free class `call` (`syscalls::free_class`) for this
/// Worker's thread, whose context is at `ctx_addr`: the whole call, its park
/// and its signal included, without the kernel.
pub(crate) fn free_call(gm: &mut GuestMem<'_>, ctx_addr: u64, call: FreeCall, compat: bool, pc: u64) -> FreeOutcome {
    let home = home().expect("a syscall before its process was published");
    let view = view(home).expect("a syscall from a Worker with no thread bound");
    let fds = crate::fds_free().expect("a syscall before its descriptor table was published");
    gather(home);
    // Whether a futex word is mapped: the map's published view where other
    // Workers may be changing the map; the map itself in the single-threaded
    // build, which keeps no view.
    #[cfg(not(feature = "shared-memory"))]
    let by_kernel = |addr: u64, len: u64| home.mm.lock().vma_at(addr).is_some_and(|v| addr + len <= v.end);
    let ns = namespace(home);
    let futex = FutexCx {
        table: &home.futexes,
        #[cfg(feature = "shared-memory")]
        map: Some(&*home.map),
        #[cfg(not(feature = "shared-memory"))]
        map: None,
        #[cfg(feature = "shared-memory")]
        mapped_by: None,
        #[cfg(not(feature = "shared-memory"))]
        mapped_by: Some(&by_kernel),
        tid: crate::my_tid() as u32,
        clock: home.clock,
    };
    // This thread's fs_struct: its own, or the process's (borrowed: no
    // count taken on what every thread shares).
    let fs = match home.thread_fs.of(ctx_addr) {
        Some(own) => std::borrow::Cow::Owned(own),
        None => std::borrow::Cow::Borrowed(&home.proc_fs),
    };
    let map = || MapIo { mem: &home.mm, fds, pins: &home.pins, place: home.place.as_ref(), pc };
    let f = Free {
        io: FdIo { fds, compat, ns: &ns, pid: home.pid, file_locks: &home.file_locks, pipes: home.pipes, shared: Some(home.mm.shared_maps()) },
        map: map(),
        path: PathIo {
            ns: &ns,
            view: &home.vfs,
            map: map(),
            compat,
            pid: home.pid,
            creds: &home.creds,
            procs: &home.procs,
            ptys: home.ptys,
            tmpfiles: &home.tmpfiles,
            fs,
            clock: home.clock,
        },
        signals: &view,
        procs: &home.procs,
        live: &home.live,
        slots: &home.slots,
        tty: &home.tty,
        pid: home.pid,
        creds: &home.creds,
        threads: &home.threads,
        wasm32: crate::WASM32.load(Ordering::Acquire),
        futex,
        started: crate::clock_started(),
        rlimits: &home.rlimits,
        net: syscalls::NetIo {
            net: crate::page_net(),
            resolver: &home.resolver,
            ports: &home.ports,
            unix_remote: home.unix_remote,
            socket_names: &home.socket_names,
            host_names: &home.host_names,
        },
        thread_fs: &home.thread_fs,
    };
    gex_kernel::phase::mark(4);
    syscalls::free_call(&f, gm, ctx_addr, call)
}

/// Return from a signal handler for this Worker's thread, whose context is
/// at `ctx_addr`: the frame's context and mask back (`signal::sigreturn`).
/// The pc to go on at. No kernel lock: the thread's signal state is its own.
pub(crate) fn sigreturn(gm: &mut GuestMem<'_>, ctx_addr: u64) -> Result<u64, gex_kernel::Errno> {
    let home = home().ok_or(gex_kernel::Errno::EFAULT)?;
    let view = view(home).ok_or(gex_kernel::Errno::EFAULT)?;
    let compat = crate::CLOCK_ABI.load(Ordering::Acquire) == 1;
    signal::sigreturn(&view, gm, ctx_addr, compat, false)
}

/// Whether `pc` is in an executable mapping of this process: the map's
/// published view where other Workers may be changing the map; the map
/// itself in the single-threaded build, which keeps no view.
pub(crate) fn executable(pc: u64) -> bool {
    #[cfg(feature = "shared-memory")]
    {
        home().and_then(|h| h.map.prot_at(pc)).is_some_and(|p| p & gex_kernel::mem::prot::EXEC != 0)
    }
    #[cfg(not(feature = "shared-memory"))]
    {
        home().is_some_and(|h| h.mm.lock().vma_at(pc).is_some_and(|v| v.prot & gex_kernel::mem::prot::EXEC != 0))
    }
}

/// How long until this process's next interval timer fires, by its own
/// clock; `None` with none armed. No kernel lock (`Kernel::timer_in_ns`).
pub(crate) fn timer_in_ns() -> Option<u64> {
    let home = home()?;
    if !home.sig.timers_armed() {
        return None;
    }
    let (secs, nanos) = (home.clock)();
    home.sig.timer_in_ns(secs as u128 * 1_000_000_000 + nanos as u128)
}
