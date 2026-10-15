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
  - `itimers`, `next_tid`, `next_ctx_slot`, `live_threads`,
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
