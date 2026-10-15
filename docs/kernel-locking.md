# Removing the kernel lock (plan, 2026-10-12)

Status: plan, no code yet. Backlog entry: "FULL REMOVAL OF THE GLOBAL KERNEL
LOCK (KLock)" (user, 2026-10-11). Owner: stream W.

The goal is that no call into a process's kernel takes a lock shared by all of
that process's threads. Each piece of state gets the narrowest
synchronisation that is still correct:

- nothing, for state one thread owns;
- atomics or snapshots, for state that is read often and written rarely;
- a lock per object or per subsystem, for the rest.

The browser shim and the native host share gex-kernel, so they share the
design. Both land step by step behind the usual gates.

## 1. What the lock is today

Two hosts, one structure: every guest thread of a process enters the
process's single `Kernel`, and only one thread is inside it at a time.

- **Browser, shared-memory build:**
  - `with_host` (shims/browser/src/lib.rs) takes `KLock` and borrows the
    static `HOST` (`Host { kernel: Kernel, threads, jit, wayland, root,
    proc_state, exit }`) in place.
  - `KLock` is a CAS word with `memory.atomic.wait32` (1 ms bound),
    re-entrant through a per-Worker `DEPTH`.
  - It lives in the process's own memory. So it is per process: it
    serialises the Workers that run one process's threads, not separate
    processes.
- **Browser, SP build (no SharedArrayBuffer):** `HOST` is a `RefCell`, and
  re-entry panics. There is one thread, so this build needs no locks and
  must stay lock-free at no cost (§4.0).
- **Native:** `HostState.kernel: Arc<Mutex<Kernel>>`, locked per syscall
  (shims/native/src/lib.rs, 10 `kernel.lock()` sites, plus the NT layer
  through `KernelCell`).
- **Cross-process state is already fine-grained.** It lives in the shared
  kernel module (shims/kernel) or behind `Arc`s with their own locks:
  - the module store, the stdin queue and the slot map each have their own
    `Mutex`;
  - futex generations are 16,384 atomic slots, notified per slot since
    28e9789a;
  - `ProcTable`, `FutexTable`, pipes, unix sockets, ptys, tmpfs, the net
    stack, eventfd and timerfd each lock internally (file.rs, pipe.rs,
    unix.rs, tmpfs.rs, net.rs: 4-26 locks each).

  This plan does not need to change any of it, only stop wrapping it in
  KLock as well.

What it costs today, with Undertale in the page on worldmachine at the
30 fps cap, busiest thread, ms per frame:

| Tree | KLock in syscalls | KLock elsewhere |
|---|---|---|
| tree C, before the W fixes | 37.3 | |
| units-ut2 WP | 7.0 | 1.7 |
| TW2N | 4.5 | 1.3 |

- The node -e 0 split (edger) shows the same: each of the four network
  Workers spent 2.2-2.5 s in KLock before TW2's parked network waits, and
  0.10-0.12 s after.

## 2. Inventory of takers

`with_host` call sites in shims/browser/src on master a2d4c964, grouped by
the state they touch. Each count is the number of sites, by enclosing
function.

| Group | Sites | Hot? | State touched |
|---|---|---|---|
| Syscall entry (`gex_syscall` 15, `count_syscall` 2, `note_process_syscall` 2) | 19 | every syscall | the whole `Kernel` through `syscall()`, the thread's `ThreadState` swapped in by `enter_thread`, `threads[cur]` |
| Dispatch miss and translation (`gex_dispatch_miss` 20, `recompile_object(s)` 7, `maybe_recompile_at` 2, `remiss`, `note_intrinsics` 3, `scan_intrinsics`, `object_seeds`, `executable_object_at`, `retire_unmapped_code`, fjit.rs 8, cgen.rs 7, cgen/libprov.rs 14, elfprov.rs 5, dylink.rs 4, wasmprog.rs 4, wasmdll.rs 3, dense.rs 1) | about 85 | every miss (Undertale ~9k/s units-only before 67912bc4) | `kernel.mem` (VMAs, permissions, file names), `jit` (page table, dirs, slots, dense sections, pending modules), unit and provider registries |
| Scheduling and waiting (`schedule` 10, `gex_park`, `gex_wake`, `wasm32_wait` 3, `park_until_ready` 3, `gex_paused_for_*` 3, `report_idle`) | 21 | every block/park | `threads` (state, blocked_on), `kernel.futexes`, signals pending, the compositor pump (`wayland`) |
| Traps and signals (`gex_safepoint` 6, `gex_breakpoint` 9, `gex_illegal` 2, `gex_cache_op` 4, `gex_sysreg_write`, `low_access`, nt.rs `raise_nested` 3, `set_resume_ctx_none_and_trap` 3) | 30 | rare, except safepoints | `kernel.signals`, the thread's context, `jit` for SMC |
| Process lifecycle (`gex_adopt_*` 8, `gex_process_gone` 2, `gex_ctx_exited`, `gex_abandon`, `gex_exit_code`, `gex_set_compat` 2, `gex_shared_seed/dump/count`) | about 17 | fork/exec/exit only | everything (adoption fills a fresh kernel) |
| Host devices and sockets (`gex_host_connect/bind/accept/send/recv`, `gex_set_winsize`, `gex_tty_lflag`, `gex_set_network`, `gex_get_file`, `gex_last_open`, `gex_cmdline`) | 11 | per host I/O | `kernel.socket_names`, `kernel.fds`, tty, `root` |
| Introspection and stats (`gex_vma_*` 5, `gex_page_*` 3, `gex_module_*` 5, `gex_jit_*` 9, `gex_dump_state`, `gex_explain_page`, `gex_pin_report`, `gex_coverage_*` 2, `gex_reuse_stat`, `object_off_str`, `object_file_off`, `check_host_magic`, nt.rs `report_stats`) | about 32 | cold (reports, xcache save) | read-only views of `mem`, `jit` and the module lists |

Fields of `Kernel` (crates/gex-kernel/src/syscalls.rs), by who writes them:

- **Per thread, swapped in under the lock** (`enter_thread`/`ThreadState`
  and `switch_signals`): `ctx_addr`, `tid`, `clear_child_tid`,
  `retry_accept`, `retry_recvmsg`, `retry_transfer`, `restore_sigmask`,
  `write_blocked`, and Signals' `blocked`, `alt_stack`, `in_handler` and
  the frame-layout flag.
- **Per process, written by syscalls:**
  - `fds` (`FdTable`: a `Vec<Option<Arc<OpenFile>>>`, cloexec, generation);
  - `mem` (`MemoryMap`: vmas, brk, mmap cursor, arena: no internal locking);
  - Signals' `actions` and process-directed `pending`/`sources`;
  - `itimers`, `next_ctx_slot`, `live_threads`,
    `unimplemented`;
  - `exe_path`, `argv`, `envp`, uid/gid;
  - `fd_activity`, `compat`, `sigreturn_legacy`.
- **Per process, constant after start:** `pid`, `ppid`, `started`,
  `clock`, `trace`.
- **Shared and already synchronised:**
  - `futexes`, `procs`, `pipes`, `unix_remote`, `ptys`, `net`,
    `resolver`, `vfs`;
  - every `OpenFile`'s object.

`Host` fields:

- `threads: Vec<Thread>`: per-thread run state, pc, waiting, blocked_on,
  page_table, pending wasm and dirs;
- `jit`: per-process translation state;
- `wayland`: an `Arc` device;
- `root`, `proc_state`: `Arc`s;
- `exit`.

## 3. Target structure

```
Process (Arc, one per guest process; shared by its threads)
  creds, pid, ppid, exe/argv/env      constants, or an RCU snapshot for exec
  fds: FdTable                        epoch snapshot to read, Mutex to write (§3.4)
  mm: Mm                              VMA snapshot to read, Mutex to write (§3.5)
  sig: ProcSignals                    actions behind an RwLock; pending as an AtomicU64 + sources Mutex
  itimers, tids, live_threads         atomics, or one small Mutex
  xlate: Translation                  lock-free lookups, CAS publication, a Mutex only to compile (§3.6)
  threads: ThreadTable                fixed slots with atomic state (§3.7)
ThreadCtx (owned by its Worker or OS thread; never shared)
  ctx_addr, tid, clear_child_tid, retry_*, restore_sigmask, write_blocked,
  blocked/alt_stack/in_handler, pending (AtomicU64, readable by senders)
```

Syscall handlers become `fn(&Process, &mut ThreadCtx, &mut GuestMem, args)`.
The `enter_thread`/`switch_signals` swap disappears, along with the reason
it needed the lock: the per-thread fields are no longer in the shared
struct.

### 3.1 Calls that need nothing shared

These read constants or the calling thread's own state:

- clock_gettime, gettimeofday, getrusage and sched_yield (already done, in
  `clock_call`, 3d9814ec);
- getpid, getppid, gettid, getuid/geteuid/getgid/getegid;
- sigprocmask/rt_sigprocmask and sigaltstack (thread-local, plus a pending
  check against the atomic pending words);
- set_tid_address, set_robust_list, and the per-thread parts of prctl;
- arch_prctl/set_thread_area;
- uname and the read-only sysinfo fields.

They are answered from `ThreadCtx` and `Process` constants with no lock,
the way `clock_call` answers its calls today. The difference is that they
run in `syscall()` itself, so the separate fast-path dispatcher goes away.

### 3.2 Futexes

The kernel module already has sharded generation slots with a per-slot
notify. What remains is the per-process `FutexTable` (crates/gex-kernel
futex.rs, 4 Mutexes): its waiter lists and PI owners.

- Shard it by the same hash, 256 shards, each with its own lock.
- FUTEX_WAIT checks the word and enqueues under one shard lock only.
- FUTEX_WAKE and REQUEUE take at most two shards, in address order.
- PI futexes (LOCK_PI/UNLOCK_PI) keep their owner per shard.

As landed (L1, branch l/klock): `futex` is in the free class
(`syscalls::free_call`), answered by `syscalls::futex_call`,
`futex_block` and `futex_pi_park`, the same functions the kernel's own
`sys_futex` calls, through a `FutexCx` (the process's table, a way to ask
whether the word is mapped, the caller's tid, the clock).

- `FutexTable`'s queues are 256 shards (`futex::shard`), each its own
  mutex; a queue's generation has its own. The cross-Worker relay
  (`woken`) has an `any_woken` flag, so a call with nothing to relay takes
  no lock.
- PI: the waiter count and the waiters bit change together under the
  table's `pi_lock`. `FUTEX_LOCK_PI` that finds the lock held sets the bit
  (a compare-and-swap even when it is on) and counts itself; `UNLOCK_PI`
  reads the count and stores the word. Apart, an unlock could read no
  waiters, a waiter set the bit and count itself, and the unlock then
  clear the word to a bare 0, which user space takes and releases without
  a wake: the waiter slept on (pi-mutex hung in 5 of 8 runs).
- Whether the word is mapped: the page asks the process's `MapView`
  (`mem::MapView`, the VMA ranges published through `rcu::Published`
  after every map change, §3.5's reader half); the kernel's own path and
  the SP build ask the map itself.
- The park (`park_free`) takes the thread's own `Run` lock only; the
  deadlock watch reads every thread's published run state
  (`all_futex_blocked`) without the kernel lock.

### 3.3 Wakes

TW2 replaced the global `k_wake` after every syscall with targeted wakes:

- the futex slot's own notify;
- `#netWait` for network waits;
- wakes only after calls that can end someone's wait (15cf6c46).

What is left is to finish it:

- every `BlockOp` gets a wake word: its futex slot, its fd's readiness
  sequence, or its pipe or socket;
- `schedule()` waits on that word alone;
- no path bumps the global wake word except process exit and signals to
  a whole process.

### 3.4 File descriptors

- **Reads:** most syscalls only look an fd up (read, write, recv, send,
  poll, fstat, lseek). Lookup reads an `Arc<[Option<Arc<OpenFile>>]>`
  snapshot published through an atomic pointer, with epoch-based
  reclamation (§4.2). It takes no lock.
- **Writes:** open, close, dup, dup2/3, fcntl F_DUPFD, socket, pipe and
  accept change the table. They take a per-process fd `Mutex`, copy, and
  publish.
- **Objects:** `OpenFile` and its object already lock themselves; offsets
  become `AtomicU64` where they are not already.
- **Close races:** a read on an fd that another thread closes is safe,
  because the reader holds an `Arc` to the `OpenFile` (Linux does the
  same).

### 3.5 Memory map

- **Writes:** `MemoryMap` gets a per-process mm `Mutex` for writers: mmap,
  munmap, mprotect, mremap, brk, madvise(DONTNEED) and fork's copy.
- **Reads:** readers take an immutable snapshot of the VMA list
  (`Arc<VmaSnapshot>`, published by generation, which `MemoryMap::generation`
  already counts). The readers are:
  - dispatch_miss permission and file-name lookups;
  - fault classification (`low_access`, safepoints);
  - `/proc/self/maps` and the introspection exports.
- **Holes stay holes.** A reader on an old snapshot can see a VMA a
  concurrent munmap removed. That is the same window Linux allows between
  a lookup and the access. Guest accesses still fault on the real
  protection, which the shim maintains separately.

### 3.6 Translation (dispatch_miss, fjit, units)

About 85 sites, and the hottest path after syscalls. The plan:

- **Lookups are lock-free.** The dispatch table, closure and owner pages
  and the dense directory are shared words already. A miss reads them
  with acquire loads.
- **Compile without a lock.** Lift, emit and compile run on the missing
  thread with no lock held; they read code through the mm snapshot.
- **Publish with CAS.** Installing a module claims its slots and pages
  with CAS on the directory words:
  - the first writer wins;
  - a loser drops its module and retries the dispatch;
  - pending modules (`threads[i].wasm`/`dirs`) are already per thread.
- **Registries.** Units and providers (libprov, elfprov, wasmdll, dylink,
  wasmprog) keep their registries behind one `Mutex` per registry. They
  change only when an image is mapped.
- **Self-modifying code.** SMC retirement (`gex_cache_op`,
  `retire_unmapped_code`, page generations) bumps the page generation
  atomically. Stale modules are then refused at their next entry check,
  as fjit's `page_gen` does now.

### 3.7 Threads and scheduling

- `Host.threads: Vec<Thread>` becomes a fixed-capacity table of slots.
  Each slot holds:
  - an atomic run state;
  - the owning Worker;
  - its `ThreadCtx` behind a `Mutex` that only its owner takes, plus a
    try-lock for a sender inspecting it.
- Thread creation claims a free slot with CAS.
- `schedule()`:
  - reads only its own slot;
  - pumps the compositor through the `Arc` device, which has its own lock
    (today the pump runs inside `with_host` only to reach the mm, and with
    §3.5 that becomes a snapshot read).

### 3.8 Signals

- `sigaction` writes `actions` under an `RwLock`; delivery reads it.
- **Pending signals:**
  - process-directed pending is an `AtomicU64`, plus a `Mutex` for the
    sources;
  - thread-directed pending is the target `ThreadCtx`'s `AtomicU64`;
  - kill and tgkill set bits and wake the target's wake word.
- **Delivery** happens on the receiving thread, from its own context and a
  snapshot of the actions.
- **The kernel module.** `signal_pending_for` already runs there for the
  cross-process case, and stays.

### 3.9 Process lifecycle

- **fork** takes the mm and fd mutexes, and copies under them. The other
  threads keep running: Linux does not stop them either, and the copy is
  of the forking thread's view.
- **exec** is single-threaded by the time it replaces the image: the other
  threads are killed first.
- **exit and wait** go through `ProcTable`, which already locks itself.
- **Adoption** (`gex_adopt_*`) fills a fresh process before any of its
  threads runs, so it needs no lock.

### 3.10 Introspection, host devices, NT

- **Introspection** exports read snapshots (mm, fds, translation
  counters). The few that need a consistent whole view, such as
  `gex_dump_state` and the xcache save, take a per-process debug
  `RwLock` for writing. Writers of the structures they read hold it for
  reading, which is uncontended, so a single atomic add.
- **Host sockets and tty** use `socket_names`, which gets its own `Mutex`,
  and the fd table (§3.4).
- **The NT layer.** `KernelCell::lock_kernel` hands out the whole
  `Kernel`. It becomes accessors for what NT actually uses (vfs, mm, fds,
  devices). NT's own `nt::Process` state keeps its own locks.

### 3.11 Native

- The native host's `Arc<Mutex<Kernel>>` becomes `Arc<Process>`, plus a
  `ThreadCtx` per OS thread.
- Each step below changes gex-kernel for both hosts at once. The native
  run is the debugging oracle for each step: thread sanitizer and stress
  tests run there.

### 3.12 What still takes the lock, and what replaces it (survey, 2026-10-15)

On 31a21cad (after L18) there are 113 lock sites in shims/browser/src:
105 `with_host`, 4 `with_host_free`, 2 `with_host_free_in`, and 2
`KLock::take`. The two `KLock::take` calls are `with_host` itself and NT's
`lock_kernel`. `with_jit` has no callers left. Each group below lists its
call sites, the state they actually touch, and what replaces the lock.

- **A. The locked syscall path.**
  - Sites: 2, both in `gex_syscall`: its `None` arm, and the trace's
    caller name.
  - Calls still taken this way, as canonical numbers:
    - lifecycle: clone, clone3, execve, exit, exit_group, wait4, reboot;
    - signals: kill, tkill, tgkill, rt_sigtimedwait, rt_sigsuspend,
      rt_sigreturn;
    - credentials and sessions: set*uid/gid, setgroups, getgroups,
      getres*, setsid, setpgid, getpgid, getsid, and getppid when IDS
      holds none;
    - descriptor objects: eventfd2, epoll_create1, timerfd_*,
      memfd_create, dup2, fallocate, sendfile, splice, copy_file_range,
      fsync, fdatasync, sync;
    - thread-own and constant: set_tid_address, set/get_robust_list, rseq,
      prctl, arch_prctl, uname, sched_*, getcpu, membarrier, times, time;
    - namespace: mount, umount2, unshare.
  - Work the locked section does around every call: the
    `enter_thread`/`leave_thread` swap, publishing IDS, the pin report,
    refreshing the proc snapshot, and delivering signals at syscall end.
  - Replacements:
    - thread-own and constant calls: nothing; they use the thread's own
      state.
    - descriptor objects: `FdIo`.
    - credentials: a per-process `Creds`, shared by its threads, copied
      by a fork and read at each use: by `PathIo`'s `stat_owned`, by a
      signal's sender uid and by `ident_call`. Each set call changes one
      id, so it is two atomics rather than a `Published` snapshot. There
      is no kill permission check to read it (no privilege model). pgid
      and sid are already `ProcTable`'s (L22c).
    - kill, tkill, tgkill, rt_sigtimedwait and rt_sigsuspend: `ProcSig`,
      `ThreadSig` and `ProcTable`.
    - rt_sigreturn: the thread's own state, through `sigs::sigreturn`.
    - mount, umount2 and unshare: the namespace's writer.
    - IDS: gone. `ident_call` reads the pid set at load and fork, the
      tid bound at thread start (`bind_mine`) and the `Creds`, instead of
      a copy refreshed at every locked call (L22c).
    - lifecycle: groups F and G below.
  - Tests:
    - setuid on one thread is seen by another thread's `stat` and by its
      kill permission checks.
- **B. Translation and file reads.**
  - Sites: 21.
    - fjit.rs ×5;
    - dylink.rs ×3;
    - wasmdll.rs ×2;
    - wasmprog.rs ×3;
    - cgen.rs ×4;
    - `gex_get_file` and `gex_pin_report`;
    - sigs.rs ×2. These two are the SP build's borrow, but they still go
      through `Host`.
  - State: the vfs, the mm (which already has its own lock), envp, the
    trace flag, and `retired_code`.
  - Replacement: reads through the published `Home` (`proc_home`, `Px`).
    This is identity: no new lock.
- **C. Introspection and diagnostics.**
  - Sites: 31.
    - Everything that names an address: `object_off_at`, `object_off_str`,
      `object_file_off`, `executable_object_at`, `livelock_note`, fjit's
      two names, `gex_breakpoint`'s lines, `gex_dispatch_miss`'s wild,
      no-meta and eject reports, `gex_illegal`, and NT `fault`'s map line.
    - `gex_vma_*`, `gex_cmdline`, `gex_dump_state`, `gex_jit_by_object`,
      `gex_shared_*`, the `gex_jit_*` counters, `gex_reuse_stat`, and
      `gex_dense_owner`.
  - State: the vmas, the namespace, argv, and `Host.jit` (`JitHere`'s
    counters, `shared` and `registry_dense`).
  - Replacements:
    - names: the mm through `Home`;
    - counters: atomics;
    - `shared` and `registry_dense`: into the JIT box (`with_jit_only`);
    - `gex_dump_state`: reads snapshots.
- **D. The thread table** (`Host.threads`).
  - Sites: 7 (`gex_ctx_exited`, the wake, idle and safepoint censuses,
    `report_deadlock`, and NT's thread exit and spawn), plus vfork's in F.
  - Replacement: §3.7's fixed slot table. A CAS claims a slot, run state is
    atomic, and each slot's `ThreadCtx` is behind its own mutex. The
    censuses read atomics: a racy view is fine for a diagnostic.
- **E. Process end.**
  - Sites: 18.
    - `set_exit` and `exit_process` from `gex_breakpoint` ×5,
      `gex_illegal`, `gex_sysreg_write`, `low_access`, `gex_dispatch_miss`
      ×2, `report_deadlock`, NT `end_process` and its x87 fault, and
      dylink's and wasmprog's 127;
    - `exit_here`, `gex_abandon`, `gex_process_gone` ×2, `gex_exit_code`.
  - State: `Host.exit`, the descriptor table's teardown, and the
    `ProcTable` publication.
  - Replacement: exit as a once.
    - A CAS on an atomic exit status decides the winner. The winner sets
      `halt_code`, and every free-path call checks `halt_code` at entry and
      at its park's end.
    - Teardown happens once no thread holds a descriptor reference: it
      drops the table's published snapshot as an RCU writer, and the
      descriptions close when the last reader's `Arc` goes. A thread parked
      in a read, an accept or a futex wait is woken by the halt and leaves
      without touching the table again.
    - `ProcTable` publishes the status last, so a parent's `wait4` sees the
      descriptors already closed.
  - Tests:
    - `exit_group` from one thread while others are parked in a blocking
      read, an accept and a futex wait.
- **F. Adoption and vfork.**
  - Sites: 9 (`gex_adopt_*`, `adopt_fds_text`, `gex_vfork_begin` ×2,
    `gex_vfork_end`, `gex_adopted_exec`).
  - Replacement: nothing.
    - A process has a single owner until it is published, so its fields
      are written directly.
    - A vfork child already has its own host.
- **G. Host devices and settings.**
  - Sites: 11.
    - `gex_set_network`;
    - `gex_host_connect`, `send`, `recv`, `bind` and `accept`;
    - `gex_tty_lflag` and `gex_set_winsize`;
    - `gex_fd_pipe_id`;
    - `gex_set_compat` ×2.
  - Replacements:
    - the network: a published slot, so `Home` follows a late
      `gex_set_network`;
    - host sockets: the socket names' own lock;
    - the terminal: its own lock;
    - the descriptor: `FdIo`;
    - compat and start time: process constants, set before the first
      guest instruction.
- **H. NT.**
  - Sites: 4 in nt.rs (the code retire, `end_process`, and `fault` ×2),
    plus `lock_kernel`, which hands the whole `Kernel` to 16 callers in
    gex-kernel's nt (file ×6, section ×2, thread ×2, vm ×2, object, opengl,
    sysinfo, gdi/font). Those callers use the vfs, the mm and the
    retire log.
  - Replacement: `KernelCell` becomes accessors for those. This is code
    removal only, with no new NT-layer behaviour.

Order, one subsystem per commit, each with its concurrency test and none
with a fallback that takes the lock. The commits are handed over two or
three at a time.

| Step | Groups |
|---|---|
| L19 | B, and C's names |
| L20 | `JitHere` into the JIT box and atomics |
| L21 | G |
| L22 | A's calls, in its groups: L22a the thread-own and constant calls, times and time; L22b the descriptor objects; L22c credentials and sessions; L22d signals; L22e mounts and the rest |
| L23 | D |
| L24 | E |
| L25 | lifecycle (clone, exec, wait, exit) and F |
| L26 | H |

After L26 the `HOST` static and `KLock` are deleted. The native
`Mutex<Kernel>` (§3.11) goes last.

## 4. Mechanisms specific to the page

### 4.0 One lock type, three builds

- `gex_kernel::sync::{Mutex, RwLock}` wraps `std::sync` natively.
- In the shared-memory shim it is a CAS word with
  `memory.atomic.wait32/notify`, as `KLock` is.
- In the SP build it is a no-op cell, with a debug-build re-entry check.
- Every new lock records its holder for the deadlock reporter, as `KLock`
  does with its "spun Ns, held by W" line.

### 4.1 Traps leave locks held

The shim leaves `with_host` by trapping when control goes back to JS. A
trap does not unwind, so a guard held across it never drops.

- **The rule:** no lock is held across anything that can trap back to JS:
  guest memory faults, a guest callback, or a park. Lock regions are leaf
  computations.
- **Debug check:** a per-Worker held-lock count is checked at every
  trap-out site, and is non-zero only on a bug.
- **Process end:** a trap that ends the process still releases nothing.
  That is fine, as today.

### 4.2 Snapshots and reclamation

- **Publication:** fds, the mm and signal actions are published as
  `Arc<T>` through an `AtomicPtr`.
- **Reclamation:** the shared-memory shim has no global quiescent point.
  So readers announce an epoch:
  - each Worker has a slot in a per-process epoch array;
  - a reader stores the global epoch in its slot on entry, and 0 on exit;
  - a writer retires the old `Arc` and frees it once every non-zero slot
    has moved past the retire epoch.
- **Cost:** two relaxed stores and one acquire load per syscall.
- **Natively,** the same code runs with `std`.

### 4.3 Lock order

Three locks nest. Every path takes them in this order, and gives none back
out of order:

    KLock (rank 1)  ->  JIT state (rank 2)  ->  memory map (rank 3)

- **KLock,** the rest of the old kernel lock (`with_host`): outermost.
- **The JIT state** (`JIT` in shims/browser/src/lib.rs, step L13): the page
  table, translated set, intrinsics, registry and pending queue of a
  process, in a box of its own. A kernel call takes it on its first touch
  of `Host::jit` (`JitHere`) and gives it back just before KLock
  (`KLock`'s release; the end of an SP `with_host`). What touches only the
  JIT state takes it alone, with no KLock (`with_jit_only`); its closure
  may not enter the kernel. The translator still takes the page table,
  translated set and intrinsics out (`jitout`) and lifts holding neither
  lock; a waiter for them outside the kernel waits with the JIT lock given
  back.
- **The memory map** (`Mm`, step L12a): innermost. Nothing is taken under
  it; a path that holds an `Mm` guard and then needs the JIT state drops
  the guard first.
- **Leaf locks** (a pipe's, a futex shard's, the descriptor table's, the
  epoch lists) take nothing under them and are not ranked.

**The check:** `gex_kernel::sync::rank` numbers the three, and
`sync::rank_take` panics in a debug build (`debug_assertions`, or the
`lock-ranks` feature) when a thread takes a ranked lock while holding one
of the same or a higher rank: an order broken, or a re-entry that would
hang. KLock calls it at its
outermost take; `sync::Mutex::ranked` locks call it in `lock()`. A
deadlock between two Workers then shows as a panic on one, naming the
ranks it held. `check_trap_out` clears the record with the held count.
Release builds skip it unless built with the `lock-ranks` feature
(`GEX_LOCK_RANKS=1 shims/browser/build.sh`): the page shim cannot be built
with debug assertions whole, since core's own checks reject its slices of
guest memory at address 0. Tests: `ranked_locks_nest_in_rank_order`,
`a_ranked_lock_under_a_higher_one_panics`,
`a_ranked_lock_taken_again_panics_rather_than_hangs` (gex-kernel sync).

## 5. Order of landing, each step with its gate leg

Every step lands alone, in a gate round. The gate runs:

- gex-kernel's tests for both architectures;
- the native suites;
- the page legs (busybox, native-debian, W3 echo, the Undertale page with
  units, node -e 0);
- the SP smoke.

Each step also states its measured effect against the step before, on the
same box.

**Removal, not fast paths** (the user, 2026-10-12). Each remaining step
deletes KLock from one subsystem for good: that subsystem's state becomes
concurrent by design (atomics, per-object locks and wait queues, RCU-style
publication), and its `with_host` uses are deleted. A lock-free shortcut that
falls back to KLock (a snapshot re-checked "every Nth turn", a hint that
sends the call down the locked path) does not count as a step. Such a
fallback may exist only inside one branch, before it lands.

**Progress metric**, in every report: the call sites in shims/browser/src
that take the kernel lock (`with_host`, `with_host_free`, `with_jit`; lines
that are only comments excluded), and how many the step deleted. Baseline:
232 on master f3689d12 (a495e23c: 236). Plus the per-syscall census of the
calls that still take it (diag/klock-6e's `[klock-calls]`).

### 5.1 Landed

Steps 0 to 5, and 6a (getpid, gettid and the uid/gid calls from immutable
per-thread ids, fd2ac628), with the compositor wake fixes on step/klock-6
(b638c279, 1c658a54, 6cf33b9c). Steps 3 to 5 took work out from under the
lock or made it cheaper; they deleted no sites (232 to 219 lock-taking
sites on step/klock-6 at 6cf33b9c, most of it 6a and the idle loop).

| Step | Change | New gate leg / check | Measure |
|---|---|---|---|
| 0 | Census: per `with_host` site, time held and time waited (`GEX_KLOCKDIAG=1`, a report every 10 s, as fbdiag does). Diagnostic, not landed. | none | where the remaining 4.5+1.3 ms/frame of Undertale and the network Workers' 0.1 s go |
| 1 | `gex_kernel::sync` lock types (§4.0); the trap-out held-lock check (§4.1); the epoch array (§4.2). No behaviour change. | unit tests for the three lock flavours; a held-across-trap test that must fail without the check | none expected |
| 2 | Split `Kernel` into `Process` and `ThreadCtx`; remove `enter_thread`. Mechanical, still under KLock. | all existing tests; the W3 page echo; the native thread suites | no change expected |
| 3 | The idle loop (§3.7, its part) and the cheap half of targeted wakes (§3.3), on w/ut-page-tw2's targeted wake. `wasm32_wait`/`schedule`/`gex_park` take the lock once a pass, not seven times. A safepoint takes it only on a hint of a signal (the global or this process's signal counter, an armed timer, a sweep every 64th), and asks about *its own* thread, which fixed `pthread_kill` to a spinning thread. A read or write wakes nobody unless its descriptor is a stream, and a read of a pipe with room wakes nobody. | `tests/browser-thread-signal.sh` (a64, x86); `check-futex-wake.mjs`; the browser pipe and fork legs; the W3 legs | Undertale takes and wait per frame per process |
| 4 | Translation registry reads without KLock (§3.6, the read half): `gex_table_need` and the `gex_module_*` getters read published state, and JS asks once a batch, not once an index or field. | the fjit legs; units-only Undertale; node -e 0 | node -e 0 wait at those sites |
| 5 | Translation outside KLock (§3.6, the write half). As built: a dispatch miss decides what to lift under KLock, takes the JIT state (page table, translated set, intrinsics) out of `Host` (`jitout::OUT`), gives the lock back, lifts, leaves the result marked done, and queues the unit under the lock again. Everything that reads that state goes through `with_jit`, which puts a finished result back and otherwise waits for it: with the lock given back from outside the kernel, holding it from inside (`retire_code`, `do_exec`, `do_fork`, nested calls), which ends because the lift needs nothing under the lock. Code retired during a lift is retired again once its unit is queued (the order one lock gave); an exec drops the unit. | the fjit legs (W3 echo with fjit, Undertale with fjit); the page SMC programs (`smc-race`, `smc-threads`, `smc-direct-jump`, `mem64-smc`, `copy-loop`); a Muck census | node -e 0 dispatch_miss held time; Unity HK/Muck presents/s |

### 5.2 Remaining, ordered by measured wait

Measured on worldmachine, Undertale in the page, units-ut3, `&fjit=0`,
diag/klock-6e census (2026-10-12). The game's process takes the lock for
these syscalls per 10 s: clock_gettime 55k, pselect6 9.4k, getrusage 7k,
futex 4.8k, sched_yield 3.5k, read 3.5k, rt_sigprocmask 3k, write 2k,
epoll_pwait 1.6k, writev, recvmsg, sendmsg, ppoll and ioctl under 1k each.
On top of the syscalls, every idle Worker of every process takes the lock
in the scheduler pick after each wake of the global WAKE word: about
44k takes and 650 ms of wait per 10 s in the game's process, and
15-22k takes in each idle Wine process.

1. **Clocks: lock deleted.** clock_gettime, clock_getres, gettimeofday,
   getrusage and sched_yield never take KLock, with no fallback:
   - the time base (`MONOTONIC_BASE`, `k_boot_secs`), the process's ABI and
     its start time are immutable process constants, set at load, exec and
     fork, before the process's first syscall;
   - EFAULT is answered in place;
   - pending signals are delivered at the next safepoint (which has its own
     lock-free signal check) rather than at this syscall's boundary;
   - a provider collection's stop protocol (cgen/libprov.rs) parks the
     thread at syscall exit through libprov's own lock.
   - Deletes: `clock_fast_path`'s `FAST_SIG`, `FAST_RUN` and
     `any_registered` fallbacks and the locked path for these numbers.
   - Leg: each call answered while another Worker holds KLock for 5 s.
2. **Waits: the global WAKE re-check deleted (§3.3).** Linux's wait-queue
   model:
   - every thread owns one wait slot (a word in the global kernel's
     memory it parks on);
   - every waitable object (pipe end, unix and TCP socket, eventfd,
     timerfd, the terminal, a child-status table entry, a futex) owns a
     waiter list: a small per-object-locked list of thread slots;
   - a blocking read, write, accept, connect or wait4 registers the
     thread's slot on its object, re-checks readiness, parks on its slot,
     and unregisters on return;
   - poll, ppoll, select and pselect register the slot on every watched
     object (no first-fd-plus-bound polling);
   - epoll keeps its interest list on the epoll object; each watched
     object notifies the epoll object, which notifies its own waiters;
   - a writer, closer, connect, exit or signal notifies the slots on its
     object's list and nothing else; a signal notifies its target
     thread's slot;
   - the scheduler pick asks only its own thread and never takes KLock
     for an idle Worker: `wasm32_wait`/`schedule`/`gex_park` park on the
     thread slot with the thread's own deadline.
   - Deletes: `k_wake` after syscalls, `wakes_waiters`, the pick's
     `with_host`, `PARK_PLAN`'s re-reads, `idle_look`'s locked survey.
   - Legs: a syscall in one process wakes no Worker in another; a poll on
     two pipes wakes within 1 ms of a write to the second; 43-thread
     mspaint click latency (P's edger-lat) and its lock-wait trace.
3. **fd table: lock deleted (§3.4).** The fd table becomes an
   `Arc<[Option<Arc<OpenFile>>]>` published through an atomic pointer
   with epoch reclamation (§4.2); open, close, dup, fcntl(F_DUPFD), pipe,
   socket and accept take a per-process fd mutex, copy and publish.
   read, write, readv, writev, pread, pwrite, recvmsg, sendmsg, poll,
   select and epoll_pwait then run without KLock; offsets are atomics
   in `OpenFile`. Leg: close-while-reading; pipe and socket stress in
   both hosts.
   - sendmsg and recvmsg (L5): in the free class, as `FdIo::sendmsg`,
     `recvmsg` and `recvmsg_park` (the kernel's `sys_sendmsg`/`sys_recvmsg`
     call the same). A socket is recovered from its descriptor's handle
     (`FileHandle::as_remote_unix`, `as_inet_socket`), so the kernel's two
     tables of sockets by handle address are deleted. A descriptor arriving
     from another process is rebuilt through the process's published
     namespace (`Vfs::view`, a `Namespace` republished on every mount,
     unmount and `chdir`; a thread with its own working directory has it
     only in its own calls), and the remote socket hooks take no kernel.
     A blocking `recvmsg` parks until the socket is readable and is made
     again (`Owed::reissue`). Legs: `sendmsg-rights` and `scm-fork` (a
     file and a pipe passed to a forked child) in the page tier;
     `free_socket_messages_answer_as_the_kernel_does`.
   - ioctl (L6): in the free class, as `FdIo::ioctl` (the kernel's
     `sys_ioctl` calls the same), whole: the device's own requests, the
     descriptor's (FIOCLEX, FIONCLEX, FIONBIO, FIONREAD) and the
     terminal's. Nothing it touches is the process's alone: the process
     table, the terminal (`Kernel::tty`, one for the process's life, kept
     in the page's published process state) and every device handle
     synchronise themselves already, because other processes reach them
     through their own kernels. The page's GL device keeps its state per
     Worker and submits from the calling Worker, so a submit or present no
     longer waits behind the lock or defers itself (`gl::DEFER` stays for
     the locked path the PowerPC page still takes). A request that signals
     the caller's own group (TIOCSIG, TIOCSWINSZ) collects it before the
     call's end. In the Noita page it was the last per-frame taker (one
     present a frame). Legs: `ioctl-threads` (FIONREAD on a pipe being
     filled and drained, close-on-exec flipped and terminal requests from
     three more threads) in the page tier;
     `free_ioctl_answers_as_the_kernel_does`.
   - The 32-bit positioned calls (L9): i386 `pread64`, `pwrite64`,
     `lseek` and `_llseek` are in the free class too. The 64-bit ones
     already were; the 32-bit ones took the lock only because their
     arguments come split in register pairs. `i386::translate` joins them
     as the locked path does, and its `Post` writes `_llseek`'s result to
     the caller's `loff_t` and answers 0. In the Noita page (i386) they
     were the second-busiest takers at startup (`lseek` 76 a second,
     `pread64` 20 in pid 2). Legs: the i386 builds of
     `close-while-reading`, `kernel-contend` and `ioctl-threads`, page
     tier and SP; `free_i386_positioned_calls_answer_as_the_kernel_does`.
   - The descriptor table's own calls (L10): `close`, `close_range`,
     `dup`, `dup2`, `dup3`, `fcntl` (i386's `fcntl64` too) and `flock`
     are in the free class, as `FdIo` methods the kernel's calls use too.
     The table publishes itself and the `flock` table locks itself. What
     the kernel lock had been making safe was the "last descriptor" test
     (`Arc::strong_count == 1` after the close). Without the lock it is
     wrong both ways: two threads closing the last two copies at once each
     still hold one, and a replaced copy of the table holds one until its
     readers move on. So the lock and an `O_TMPFILE`'s name now go when
     the description itself does (`OpenFile`'s `Drop`, Linux's last
     `fput`). A `close` of such a description waits for the table's
     replaced copies (`FdTable::settle`, `Published::settle`), so the lock
     is gone when the call returns. For that, `Published` now retires a
     replaced value under its writers' mutex, and counts the values being
     dropped. A memfd is found from its handle (`FileHandle::as_memfd`),
     not a per-kernel map. Legs: `fdtable-threads` (the dup family, flags
     and close racing on one table while a writer feeds a pipe through
     copies; flock released by the last close and not before) in the page
     tier, x86-64 and i386, and SP;
     `free_fd_table_calls_answer_as_the_kernel_does`, its i386 twin,
     `a_tmpfile_name_goes_with_its_last_descriptor`, and gex-atomic's
     `settle_drops_every_value_replaced_before_it`.
4. **Futexes: lock deleted (§3.2), L1.** `FutexTable` sharded, 256
   shards with their own locks; the PI count and bit under one table
   lock; `futex` answered in the free class with no fallback. Legs:
   `check-futex-wake.mjs`; `kernel-contend` (mutex, condvar, PI, map
   churn, idents from 4 threads) in the page tier; pi-mutex; the
   gex-kernel tests `free_futex_answers_as_the_kernel_does`,
   `futex_calls_from_many_threads_keep_a_mutex` and
   `a_map_view_is_read_while_the_map_changes`.
5. **Sleeps: lock deleted, L2.** nanosleep and clock_nanosleep are in the
   free class: `syscalls::sleep_block` and `nanosleep_answer` are the
   kernel's own code, with the clocks from process constants
   (`clock_nanos_at`: the clock, the start time); the page parks the sleep
   on its thread's own run state (`park_free`, `Waiting::Sleep`). They were
   the largest taker left in every Noita-page process (about 61 a second
   each). Legs: `sleep-threads` in the page tier;
   `free_sleep_answers_as_the_kernel_does`.
6. **Signal masks and per-thread state: lock deleted.** rt_sigprocmask,
   sigaltstack, set_tid_address, set_robust_list, arch_prctl on a
   per-thread state only its owner writes; pending words atomic (§3.8,
   the per-thread half). Leg: thread-signals in the page tier, which
   fails today (§5.3, thread-signals).
7. **Memory map: lock deleted (§3.5).** mm mutex for writers and a
   published VMA snapshot for readers (dispatch_miss, faults,
   /proc/self/maps).
   - The mm lock (L12a): `Kernel::mem` is an `Mm`, the process's
     `MemoryMap` behind its own `gex_kernel::sync::Mutex`, with the
     published view (`MapView`) beside it, read without the lock. Every
     reader and writer takes it for a leaf computation (`k.mem.lock()`),
     never across a trap; a copy (`fork`, a vfork child) is a copy of the
     map with its own lock and view, and `Mm::shared` is the same map for
     a holder outside the kernel.
   - The mapping calls without the kernel lock (L12b): `mmap`, `munmap`,
     `mprotect`, `brk`, `mremap`, `madvise` and `msync` are in the free
     class (`syscalls::MapIo`, with i386's `mmap2` and old `mmap`). What
     they note for the host moved from `Kernel` into the map, under its
     lock (`mem::MapLog`: code mapped and retired, the pin and placement
     armed by `openat`, where pins landed, shared file mappings); the pins
     and the placement are process constants, published with the map in
     the page's `sigs::Home`. A call that can take code away (`mmap`,
     `munmap`, `mprotect`, `mremap`) is made and its code retired under
     the JIT lock (`free_syscall`, `map_drain`): JIT then mm, so nothing
     translates or runs the old code between the two and a freed address
     is not mapped again with new code before its translations are gone.
     `mremap` copies with the mm lock held, so another thread's mapping
     cannot land in the range it vacates before its bytes are out.
     The log is drained only under the JIT lock, held until what was taken
     is retired (`take_logged`), and every call that makes code executable
     drains it before it returns: the free path's, the locked path's
     (`retire_unmapped_code_in`) and the NT layer's (its calls still run
     under the kernel lock, their drain a section after the call). So
     whichever path unmapped code, a thread that maps new code at that
     address either retires the old translations itself or waits for whoever
     took them to finish; none runs the old code there. `mprotect` merges
     the neighbours it makes alike again, as Linux does, so a range made
     read-only and writable again is one mapping for `mremap`. Still under
     the kernel lock: the NT layer's mapping calls themselves. Legs:
     `free_mapping_calls_answer_as_the_kernel_does`,
     `free_i386_mapping_calls_answer_as_the_kernel_does`,
     `protect_back_merges_and_mremap_takes_the_whole`; `mmap-threads`
     (x86-64, i386, aarch64) in the page tier; W3 `vmreuse` (NT
     VirtualAlloc/VirtualProtect/VirtualFree with code at reused
     addresses, from four threads).
   - The mapped-code drains without the kernel lock (L12c): what runs after
     a call that mapped code -- the recompile queue (`recompile_objects`,
     and `maybe_recompile_at` on a miss) under the JIT lock alone
     (`with_jit_only`); export providers, replaced libraries and units
     (`elfprov::maps`, `libprov::maps`, `elf_unit_maps`, the forked child's
     `rescan_if_new`, `gex_libprov_guest_sym`/`guest_stack_end`, the
     provider's own low mappings) and the intrinsics scan (`note_intrinsics`)
     read files through the process's published namespace and the map
     through `Mm::shared`, both in `sigs::Home` (`Home::read_file`,
     `Home::code_maps`, `sigs::proc_home`). `note_intrinsics` had taken the
     JIT lock under mm (against §4.3; fixed on its own first).
     `recompile_object`, the bulk translator, followed in L16 (item 9).
     Leg: `codemap-threads` (x86-64, i386; page tier, also with
     `GEX_ELF_UNITS=all` and `GEX_RECOMPILE_OBJECTS=1`): copies of the
     program mapped executable
     from a file and called from three threads while two churn anonymous
     mappings.
   - `/proc` without a refresh (L14a): the page refreshed a snapshot of the
     process (`ProcState`: the map rendered, the descriptors listed, the
     image) under the lock before every syscall that might open a `/proc`
     file. `/proc/self` now reads the process's own published state live
     (`proclive::ProcCell`): the map through `Mm::shared`, the descriptor
     table (now an `Arc<FdTable>` in `Kernel`) through its published copy,
     the directory through the published namespace and the relations
     through the process table. The image (name, argv, environment) is
     published at a process's start, a vfork child's start and an exec
     (`Kernel::publish_proc`). An exec that kept the program's path left the
     old command line in `/proc/self/cmdline`; the image is now compared by
     argv. Leg: `proc-threads` (x86-64, i386, aarch64; page tier and SP)
     and `proc_reads_the_process_live`.
   - The path lookups and opens without the kernel lock (L14b): `openat`,
     `openat2`, `getdents64`, `fstat`, `newfstatat`, `statx`,
     `faccessat(2)`, `readlinkat` and `getcwd` (and their i386, x86-64 and
     aarch64 older forms, translated to these) run on the free path through
     `PathIo`: the published namespace, the descriptor table, the map (for
     the user buffers) and the calling thread's working directory. The
     working directory belongs to the fs_struct, not to the thread: every
     thread made with `CLONE_FS` (every pthread) shares its creator's
     (`ThreadFs`, an `FsCwd` per context, the same `Arc` for the threads
     that share it); `clone` without `CLONE_FS` copies it and
     `unshare(CLONE_FS)` makes the caller's own. A `chdir` in one pthread
     is seen by its siblings, as on Linux. The namespace's publication keeps
     the process's directory while a call runs in a thread's own
     (`Vfs::enter_cwd`/`leave_cwd`). The tmpfile counter is an atomic and
     the page's record of the last open (`LAST_OPEN`) its own lock, so
     `note_process_syscall` no longer takes KLock on every `openat`. Legs: `free_path_calls_answer_as_the_kernel_does`,
     `free_i386_path_calls_answer_as_the_kernel_does`; `fs-threads`
     (x86-64, i386, aarch64; page tier and SP): a pthread's `chdir` seen by
     its siblings and main, `unshare(CLONE_FS)` shared with the threads made
     after it and not by the rest, and `openat`/`fstatat` through a
     directory descriptor while another thread renames files in it.
   - The path writers without the kernel lock (L14c): `mkdirat`,
     `unlinkat`, `renameat(2)`, `linkat`, `symlinkat`, `utimensat`,
     `fchmod(at)` and `fchown(at)` (and their older forms) are `PathIo`'s
     too. The umask they apply is an atomic. `RENAME_NOREPLACE`'s check
     and the move are one step under the filesystem's own lock
     (`FileSystem::rename2`, also over the remote filesystem), where the
     kernel lock had made them one. Three Linux behaviours that were
     missing, fixed with it:
     - A renamed directory takes everything under it. The tmpfs is a map
       of whole paths and moved only the directory's own name, so its
       contents were lost (dpkg, apt, cargo and git all rename
       directories). Linux's refusals are kept: a directory into itself
       (`EINVAL`), over a file (`ENOTDIR`) or over a directory that is not
       empty (`ENOTEMPTY`), a file over a directory (`EISDIR`).
     - A directory descriptor resolves by the directory, not by the path it
       was opened at (`FileHandle::moved`, `OpenFile::dir_path`; tmpfs by
       inode, the remote filesystem through `k_fs_moved`, hostfs through
       the host's descriptor). `openat(dirfd, ...)`, `fchdir`, `getdents64`
       and `fstat` on it follow a rename of the directory or of one above
       it. Finding the directory and then the name is two steps, so a call
       through a descriptor that misses while its directory moved (a
       per-directory move count) is made again, as Linux's walk retries on
       its rename seqlock (L14d: then falls back to the rename lock, below).
       `fchmod` follows a renamed file the same way.
     - `execve` keeps the calling thread's fs_struct and a `fork` copies
       it. A thread with a directory of its own (`unshare(CLONE_FS)`) execs
       into that directory, not the process's; the directory is taken from
       the call itself (`Kernel::call_fs`, L14d), since by the time the
       host acts another thread's call may have run.
     Legs:
     `free_path_writers_answer_as_the_kernel_does`, the writers' older
     forms in `free_i386_path_calls_answer_as_the_kernel_does`,
     `a_renamed_directory_takes_its_subtree`,
     `an_open_directory_follows_its_renames`; `fs-threads` (x86-64, i386,
     aarch64; page tier, SP and gexc). It now also renames a directory with
     a file in it back and forth, and renames a descriptor's own directory
     back and forth while threads open through it; a child execs from a
     thread with its own fs_struct.
   - `chdir`, `fchdir` and `umask` without the kernel lock, and the
     working directory by identity (L14d). They are `PathIo`'s: each
     changes the calling thread's fs_struct (`FsCwd`: the directory, held
     open, and the umask), which the process's (`Kernel::proc_fs`) and
     each thread's own (`ThreadFs`) are. The namespace's directory now
     only follows the process's, for what is resolved outside a call. The
     working directory is held through its open directory, as Linux holds
     a dentry: renamed, `getcwd`, `/proc/self/cwd` and relative lookups
     follow it; removed, `getcwd` says `ENOENT` and `/proc/self/cwd` reads
     `/ (deleted)`. Its path is found again only when the filesystem's
     count of directory renames and removals has changed
     (`FileHandle::rename_epoch`, read without a lock). A relative lookup
     that misses while the working directory moved is made again, as one
     through a descriptor is. After eight such tries the call is made once
     more with the filesystems' directory renames held off
     (`FileHandle::hold_renames`): the filesystem's own rename lock
     (`vfs::RenameGate`, per tmpfs and hostfs mount; the global kernel's
     for the remote filesystem through `k_fs_hold_renames`), taken
     shared, not the kernel's. A miss for a rename is then impossible,
     not just unlikely: Linux's walk falls back to `rename_lock` the same
     way after its optimistic tries. A directory rename waits while
     another thread holds; the holder's own renames do not, and a holder
     whose process has ended holds nothing (`vfs::set_owner_alive`).
     hostfs counts only the renames made through it: the host's own, and
     other host processes', are outside that lock. Legs:
     `a_working_directory_follows_its_renames`,
     `a_hold_keeps_directory_renames_off`; `fs-threads` now also renames
     the working directory and checks `getcwd` and a relative lookup, and
     races renames of the working directory back and forth with threads
     opening, stating and `getcwd`ing relative to it.
8. **Signals and the thread table: lock deleted (§3.7, §3.8).**
   - The scheduler's turn (L7): `schedule` takes no lock. Every turn is
     `pick_mine` on the thread's own run state, the first turn of a
     kernel-parked call included (it was made under the call's take, and
     a turn with no registered wait took one of its own: the last per-turn
     taker in every Noita-page process, 60 a second in Noita's and 16 in
     each idle Wine process's). `pick_turn`, `idle_look` and `any_signal`
     are deleted. The last thread out ends its process under its own exit
     call's take (`end_if_all_exited`), and a vfork parent's thread is
     bound back to its Worker when the child is done. The compositor's
     pump still takes the lock, for the pump alone and only when it has
     work (`pump_compositor`); that is §3.10's. Legs: the page tier's
     thread and signal programs (`main-thread-exit`, `thread-churn`,
     `thread-signals`, `kernel-contend`) in the shared, memory64 and
     single-thread builds.
   - The calls on the process's and thread's own signal state, and
     `getrandom`, without the kernel lock (L15): `rt_sigaction` (the
     dispositions, `ProcSig`'s `RwLock`), `rt_sigpending`, `sigaltstack`
     (the thread's `ThreadSig`), `setitimer`, `getitimer` and `alarm` (the
     process's timers, behind their own lock, with the earliest deadline
     an atomic) run on the free path (`is_own_state`; the same functions
     the kernel's arms call). After an arming the page publishes the next
     deadline itself (`waitq::publish_timer`), as the locked call's end
     did. `getrandom` reads the namespace's `/dev/urandom` there: in the
     Undertale census it was the longest single hold left (2 calls, 49 ms).
     Fixed with it: an i386 `setitimer` now reports the old value and
     `getitimer` is answered (it was `ENOSYS`), both narrowed from the
     64-bit itimerval (`Post::Itimerspec32`'s four words); musl's i386
     `alarm` is `setitimer`, so `alarm` there always answered 0. `kill`,
     `tgkill` and `rt_sigtimedwait` follow, on top of W's cross-process
     `tgkill` (801a8221). Legs: `free_own_state_calls_answer_as_the_kernel_does`,
     the i386 forms in `free_i386_path_calls_answer_as_the_kernel_does`;
     `sig-state-threads` (x86-64, i386, aarch64; page tier, SP and gexc):
     threads set and read back their own signals' handlers, keep their
     own alternate stacks, see a blocked raised signal pending and draw
     random bytes while an interval timer fires; then `getitimer`, the old
     value of a disarm, and `alarm`'s seconds left.
   - The small descriptor and process calls without the kernel lock (L17):
     `epoll_ctl`, `pipe2` (and `pipe`), `ftruncate` (i386's
     `ftruncate64`), `statfs`/`fstatfs`, `getpriority`/`setpriority`,
     `sysinfo` and `prlimit64` (with x86-64's `getrlimit`/`setrlimit` and
     i386's `ugetrlimit`) run on the free path (`is_fd_misc`): the
     descriptor table and the global kernel's pipes (`FdIo`), constant
     answers, and the resource limits behind their own lock (`Rlimits`,
     read, checked and set at once; RLIMIT_NOFILE's soft limit is the
     descriptor table's, as before). Legs:
     `free_fd_misc_calls_answer_as_the_kernel_does`,
     `free_i386_fd_misc_calls_answer_as_the_kernel_does`; `fdmisc-threads`
     (x86-64, i386; page tier, SP and gexc): pipes made and used, one epoll
     set added to, waited on and deleted from by two threads, files
     truncated and read back, and the descriptor limit set and read while
     other threads open descriptors.
   - Task ids (L15b, before `kill`/`tgkill` leave the lock): threads are
     numbered with the processes, by the process table
     (`ProcTable::new_tid`, the global kernel's `k_tid_new` in the page),
     as Linux's tids and pids are one numbering. A per-process count
     (`Kernel::next_tid`, from pid + 1) gave a forked child's thread the
     number of the next process forked, so `tkill(n)`, `kill(n)` and
     `/proc/<n>` could mean either. The table keeps each thread's process
     until it ends (`end_tid`: `exit`, `execve`'s other threads, the
     process's exit); `kill` of a thread's id is its process's signal,
     `tkill`/`tgkill` of another process's thread reach that process (and
     `tgkill` checks the tgid), and `/proc/<tid>` is its process's
     directory (`ProcSource::task_owner`). Legs:
     `thread_ids_are_numbered_with_the_processes`; `task-ids` (x86-64,
     i386; page tier, SP and gexc).
   - The socket calls without the kernel lock (L18): `socket`,
     `socketpair`, `bind`, `listen`, `accept`/`accept4`, `connect`,
     `sendto`, `recvfrom`, `sendmmsg`, `recvmmsg`, `getsockname`,
     `getpeername`, `setsockopt`, `getsockopt` and `shutdown` run on the
     free path (`is_socket_call`, `NetIo`): the descriptor table (`FdIo`),
     the network, resolver and ports, the global kernel's sockets and the
     socket name tables, all shared and locking themselves. A unix
     socket's relative name resolves against the calling thread's
     fs_struct. A blocking `connect` parks until the connection is up or
     refused (`BlockOp::Connect`, now also a class call's park); a blocking
     `accept` or `recvfrom` with nothing there parks until there is and is
     made again (`AcceptWait`). Legs:
     `free_socket_calls_answer_as_the_kernel_does`; `sock-threads`
     (x86-64, i386; page tier, SP and gexc): a listener serving two
     connecting threads through a blocking accept, the options and names of
     what they made, shutdown seen as the end, socketpairs with
     `sendmmsg`/`recvmmsg`, and internet sockets made and asked their kind.
9. **Lifecycle, devices, introspection, NT; then KLock itself deleted
   (§3.9, §3.10).**
   - The compositor's pump (L8): no lock. What it asked of the kernel is
     published: the shared file mappings (`SharedMaps`, a list republished
     on every `mmap(MAP_SHARED)` and every unmap that drops one, read as
     `SharedMapView`, which is where a client's `wl_shm` pool is), and the
     process's program and X socket names (`wl::ProcCx`, set at a
     process's start, a fork, a vfork's return and an exec). Pumps from
     several Workers are serialised by the device's own sessions lock.
     After L7 it was the last per-frame taker in the Noita page (60 a
     second in Noita's process, 16 in each Wine process's). Legs:
     `the_shared_map_view_follows_the_list`; the Noita page and the core
     gate's browser and gears legs, which pump.
   - The miss path before a translation (L11, §3.6's first part): no
     lock.
     - A handler's return through `SIGRETURN_TRAMPOLINE` (every handler
       on aarch64, where musl sets no restorer) restores the frame
       through the thread's own signal state (`sigs::sigreturn`).
     - The x86 check that a branch target is executable reads the map's
       published view (`MapView::prot_at`; the map itself in SP).
     - The fill's record of the spans it asked about and the pages nobody
       could fill has its own lock (`FILL_ASKED`, `gex_kernel::sync::Mutex`).
     A miss a module already in the process answers (a fill) took the
     lock twice, and now takes none. A miss that translates still takes
     it in `with_jit` (§3.6's lift and install), as do the wild-branch
     report and the eject stop, which end the process. Legs: the page
     tier's signal programs (`signal-storm`, `thread-signals`,
     `sa-restart`, `pwait-sigmask`) built for aarch64 as well as x86.
   - The JIT state's own lock (L13, §4.3): the page table, translated
     set, intrinsics, registry and pending queue left `Host` for a
     per-process box (`JIT`), ranked between KLock and the mm. Kernel
     calls take it on first touch and give it back before KLock; the 24
     `with_jit` callers that touch nothing else (a fill's directory
     catch-up, the self-modifying-code retire, the unit and closure
     directory writes, the coverage and slot getters) take it alone, with
     no KLock (`with_jit_only`). The miss's lift and install still go
     through `with_jit` (they read the thread table and the object
     identities); the mapping calls (L12b) are made and retire their code
     under this lock.
     Legs: the page tier's translation programs (`retire-during-lift`,
     `smc-race`, `smc-threads`, `smc-direct-jump`, `page-cross-entry`)
     and the rest of the page tier with the order checked
     (`GEX_LOCK_RANKS=1`).
   - The miss's lift and install without the kernel lock (L16, the rest of
     §3.6's write half): what a translation reads of its process besides
     the JIT state -- the map, the namespace (object identities and names)
     and the pid -- comes from the published `Home` (`Px`, `with_px`), and
     the rest is the JIT state under its own lock (`with_jit_only`). The
     closure and serve maps the miss consults (`IMAGE_PAGES`,
     `CLOSURE_OWNER`, `LIB_PROGRAM`, `IMAGE_TABLE`, `CLOSURE_TABLE`,
     `LIB_LAZY`, `IMAGE_HIGH`, `CLOSURE_DIRS`, `LAZY_OWNER`, `LIVE_BASE`,
     `SERVED_PAGE`) moved under the JIT lock with it, writers
     (`gex_add_closure_module`, `gex_add_lazy_module`, `gex_claim_page`,
     `do_exec`) and readers (`gex_closure_owner(_at)`, `gex_page_table`,
     `gex_lib_lazy`, `gex_claim_floor`, `gex_lazy_owner`) alike; as do the
     install (`gex_jit_installed`) and the pending-queue getters.
     `recompile_object` takes the JIT state out as a miss does (`jitout`)
     rather than lifting from a copy and writing the copy back over what a
     sibling's miss installed meanwhile, and like the miss drops its result
     on an exec or a retire the log no longer covers, and retires again
     what was retired during its lift; its seeds read the object through
     the published namespace. `with_jit`, the kernel-lock form, had no
     caller left and is gone. Still under the kernel lock on the miss path:
     the wild-branch report, the eject stop and a miss with nothing to
     translate, which end the process; the function JIT's file reads
     (fjit.rs). Legs: `lift-threads` (page tier, gexc): four threads lift
     fresh chains of code at once and unmap them while two make kernel
     calls; the translation programs above; `codemap-threads` with
     `GEX_RECOMPILE_OBJECTS=1`.
10. **Native: `Mutex<Kernel>` gone (§3.11).**


### 5.3 Measurements and known failures

- Each §5.2 step deletes KLock from one subsystem while every other still
  takes it. So any step can stop without leaving a half-locked structure:
  whatever is not yet converted stays under KLock as now.
- Steps 0 to 5 were ordered by the step 0 census (2026-10-11):
  - **Undertale** (worldmachine, units-ut3, 600 s): every Wine process's
    idle Workers pass through `wasm32_wait`, `schedule` and `gex_park` about
    550 times a frame, taking KLock 7 times a pass: 80-130k takes a second
    per process, against about 520 real syscalls a frame in the game's
    process. Each syscall bumps the global wake word and wakes them all.
    Hence step 3 first.
  - **Step 3, measured** (worldmachine, Undertale page on a master base
    without function-tier units, so below the cap; 200 s, last 120 s):
    the game's process went from 80.1k to 22.7k takes a second and from
    827 to 288 ms a second waited (about 3,870 to 1,040 takes a frame);
    idle Wine processes from 18-26k to 3.3-4.8k takes a second; 20.3-20.7
    to 21.9 fps. Of the remaining global wakes all came from the game's
    process, about 700 a second, most of them reads and writes on the
    wineserver's pipes; the read filter takes the reads out. Each wake left
    costs an idle Worker one take of its own process's lock, so the
    per-object slots that would end them save idle CPU rather than the
    game's lock wait, and wait for step 6 (6b). To be rerun on units-ut3
    once the units are re-keyed.
  - **node -e 0** (edger, warm run, 14 processes; 1.71 M takes, 10.5 s
    waited, 11.8 s held): registry reads from JS (`gex_table_need`, 498k
    takes, and `gex_module_*`, 357k) waited 7.2 s; `gex_dispatch_miss`
    held 5.35 s over 7,200 takes; syscalls held 2.6 s. Hence steps 4 and 5
    next.
  - **Step 5, measured** (edger, node -e 0, census builds, two runs each,
    A = step 4, B = step 5): the cold process's KLock held time went from
    3.7 and 6.2 s to 0.76 and 1.25 s; the miss now holds it 0.3-0.4 s for
    4.6-7.3k misses (decide and queue), against 3.3 and 5.5 s for 4.9-7.2k.
    Wait over all processes went from 1.1 and 2.4 s to 0.4 and 0.7 s. The
    warm run's wall time did not move (13.25 and 13.50 s for A, 13.29 and
    13.65 s for B): the cold process's lifting was never the critical path's
    lock wait, only its own. Undertale tier-1 on worldmachine, 200 s, last
    120 s: 21.9-22.9 fps against 22.3-22.5 for step 4.
  - **Step 8, the thread-signals "failure"** was the test's: it slept
    100 ms for its two threads to reach their condition-variable wait, and
    a page starts a thread's Worker later than that, so the blocker took the
    signal meant for it before it had blocked anything. It now waits for
    both under the mutex, and is in `PAGE_PROGRAMS` with `signal-storm`.
  - **Step 8, signal words (as built):** `Signals` is two `Arc`s, the
    process's `ProcSig` (dispositions behind an `RwLock`, the shared
    pending word, the interval timers with their earliest deadline as an
    atomic) and the running thread's `ThreadSig` (pending and mask as
    atomic words, sources and handler state behind small mutexes). Which
    thread runs is a pointer swap, not a copy in and out. Each page Worker
    holds its own thread's `ThreadSig` (`sigs`), so a safepoint asks
    whether a signal would do something, collects the inbox (when the
    proc table's signal sequence moved, which is now bumped after the bit
    is set) and fires due timers, and builds the handler frame, with no
    kernel lock and no hints: the global and local signal counters, the
    timer flag and the every-64th sweep are gone. A delivery claims its
    signal with one atomic `fetch_and` before building the frame, so two
    threads never take one process-directed signal; collecting raises each
    signal once, with its sender, for the same reason. The thread table
    (a resume after a handler frame) and process exit stay the kernel's
    until §3.7 and step 9.
  - **Step 8, the thread's run state (§3.7, as built):** each thread-table
    entry keeps its pc, its wait, the call it parked in and what that call
    still owes (`Run`: `rt_sigsuspend`'s mask, a write's `SIGPIPE`, a call
    to make again) behind an `Arc<Mutex>` of its own, and its Worker holds
    that `Arc` (`MY_RUN`). The kernel's per-call leftovers (`restore_sigmask`,
    `write_blocked`, the `retry_*` of `accept`, `recvmsg` and transfers) are
    taken out of it as the call parks. An idle turn that finds its wait
    over (`waitq::Turn::Ready`) finishes the call with no kernel lock
    (`finish_mine`, `finish_wait`): the answer into its context, the signal
    half through `sigs::finish`, and an `accept`, `recvmsg` or transfer is
    made again from its instruction (in place, for a wasm32 program or a
    provider) instead of being redone inside the kernel. A safepoint's
    handler frame resumes without `schedule`. The locked turn stays for a
    blocking call's first look (under its own take), the compositor's pump
    and the deadlock watch; process exit stays the kernel's. The context
    address and tid a Worker runs (`MY_CTX`, `MY_TID`) replaced twenty
    sites that took the lock to read them.
  - **The lock-free syscall class (as built):** `read`, `write`, `readv`,
    `writev`, `pread64`, `pwrite64`, `lseek` and `rt_sigprocmask` are a
    class decided by number (`syscalls::free_class`; the 32-bit ABI's
    positioned calls and `_llseek` are not in it). A call in the class never
    takes KLock, blocking included. The page makes the whole call from its
    Worker (`free_syscall`, `sigs::free_call`). The descriptor half
    (`FdIo`: the call, the park before it and the park after an `EAGAIN`)
    and `sigprocmask` are the same code the kernel runs. Before delivering,
    the call collects signals sent from elsewhere and fires due timers
    (`sigs::gather`). Its `SIGPIPE` is raised on the thread's own signal
    state.

    A call that waits:
    - puts its wait into the thread's run state;
    - makes the scheduler's first turn itself (`pick_mine`): it finishes
      the call if the stream is ready, or registers the slot on the
      stream's queue and says why nothing runs.

    That "why" (`wait_why`) is now the Worker's own thread's alone, asked
    without the kernel. The process's network is `HAVE_NET`, which the
    kernel's `net` is made from. A global-kernel socket says so itself
    (`FileHandle::is_global_socket`). Registering a wait and publishing the
    process's next timer (`waitq::enter`, `publish_timer`) read the
    process's signal state, not the kernel.

    The kernel still takes over when the process ends: a signal's default
    action or a halted machine (`killed`, `exit_process`). The compositor's
    pump stays a locked scheduler turn when it has work.
  - **Known failure, native, not KLock's:** `gexc run --fjit` livelocks
    ("retranslation livelock") once a page of a long direct-jump chain is
    retired under it, as `tests/x86-programs/retire-during-lift` does
    (2026-10-12). Pre-existing; it is why that test is not named `*smc*`
    (run.sh would run it under `--fjit`). Tier-1 native and the page pass.
  - **Step 5, the drop freeze** (5892193e): a unit dropped across a retire
    left the pages it had numbered in the page table with no directory
    entry, and a block running onto one of them failed its page check for
    ever (Undertale frozen at its first frame). A dropped unit now forgets
    those pages; `retire-during-lift` puts a block across every page
    boundary and hung 2 of 2 on a build dropping on every retire without
    the fix. Steps 6 and 7 must keep the rule: a page given a base is
    either published or forgotten.
  - Registry reads touch neither fds nor mm, so step 4 needs neither first;
    step 5's write half publishes what steps 6 and 7 then stop guarding.

## 6. Risks

- **Re-entrancy.** Today's code assumes it is alone in the kernel. A
  handler that calls back into another subsystem (`execve` closing
  cloexec fds, `exit_group` tearing down mm and fds) must take locks in
  one global order: fd, then mm, then signals, then threads, then the
  translation registries.
  - The lock types assert the order in debug builds.
- **Lost wakes.** Each removed lock was implicitly ordering a check and a
  park. Every park keeps the existing pattern: read the wake word, check
  the condition, park only if the word has not changed (`WAKE_SEEN` in
  `schedule`).
- **The SP build** must not pay for any of it (§4.0). Its smoke leg is in
  every round.
- **Memory.** The epoch array and the snapshots cost a few KB per process.
  Snapshot churn on mmap-heavy guests (Unity's GC) is bounded by
  publishing at most once per syscall.
