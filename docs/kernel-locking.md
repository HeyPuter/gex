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

| Step | Change | New gate leg / check | Measure |
|---|---|---|---|
| 0 | Census: per `with_host` site, time held and time waited (`GEX_KLOCKDIAG=1`, a report every 10 s, as fbdiag does). Diagnostic, not landed. | none | where the remaining 4.5+1.3 ms/frame of Undertale and the network Workers' 0.1 s go |
| 1 | `gex_kernel::sync` lock types (§4.0); the trap-out held-lock check (§4.1); the epoch array (§4.2). No behaviour change. | unit tests for the three lock flavours; a held-across-trap test that must fail without the check | none expected |
| 2 | Split `Kernel` into `Process` and `ThreadCtx`; remove `enter_thread`. Mechanical, still under KLock. | all existing tests; the W3 page echo; the native thread suites | no change expected |
| 3 | The idle loop (§3.7, its part) and the cheap half of targeted wakes (§3.3), on w/ut-page-tw2's targeted wake. `wasm32_wait`/`schedule`/`gex_park` take the lock once a pass, not seven times. A safepoint takes it only on a hint of a signal (the global or this process's signal counter, an armed timer, a sweep every 64th), and asks about *its own* thread, which fixed `pthread_kill` to a spinning thread. A read or write wakes nobody unless its descriptor is a stream, and a read of a pipe with room wakes nobody. | `tests/browser-thread-signal.sh` (a64, x86); `check-futex-wake.mjs`; the browser pipe and fork legs; the W3 legs | Undertale takes and wait per frame per process |
| 6b | Per-object wake slots (§3.3, the rest): a wait on one pipe or socket parks on that object's slot, bumped by its other end; multi-fd polls keep the global word. After step 6, whose per-object fd state makes the slot natural. | a test that a syscall in one process wakes no idle Worker in another | idle Workers' wakes per second in every Wine process |
| 4 | Translation registry reads without KLock (§3.6, the read half): `gex_table_need` and the `gex_module_*` getters read published state, and JS asks once a batch, not once an index or field. | the fjit legs; units-only Undertale; node -e 0 | node -e 0 wait at those sites |
| 5 | Translation outside KLock (§3.6, the write half): lock-free lookups, CAS publication, per-registry mutexes. | the fjit legs (W3 echo with fjit, Undertale with fjit); an SMC test with 2 threads; a Muck census | node -e 0 dispatch_miss held time; Unity HK/Muck presents/s |
| 6 | Syscalls: the nothing-shared ones (§3.1, `clock_call` folded in), then the sharded `FutexTable` (§3.2), then the fd snapshot and fd mutex (§3.4). | a test that each nothing-shared call is answered while another Worker holds KLock; `check-futex-wake.mjs` at 8 Workers on 64 futexes with requeue; a close-while-reading test; pipe and socket stress in both hosts | Undertale syscall wait per frame; node -e 0 network Workers' KLock |
| 7 | mm mutex and VMA snapshot (§3.5). | an mmap/munmap storm against translation on 4 threads, natively and in the page; the fork tests | dispatch_miss time per frame |
| 8 | Signals (§3.8) and the rest of the thread table (§3.7). | the signal suites; the nullcall fault leg; a tgkill storm; W3 legs (Wine uses SIGUSR and SIGALRM heavily) | idle Workers' CPU |
| 9 | Lifecycle, devices, introspection and NT (§3.9, §3.10). `with_host` and `KLock` deleted. | fork/exec/wait stress in the page; the xcache save; the whole gate | Undertale KLock: 0; the full table again |
| 10 | Native: `Mutex<Kernel>` gone (steps 2-9 already changed gex-kernel, so this is the host glue). | native suites under TSan, where they build | native Noita/HK, as a check only |

- Steps 3 to 8 each remove KLock from a class of calls while every other
  call still takes it. So any step can stop without leaving a half-locked
  structure: whatever is not yet converted stays under KLock as now.
- The order is by the step 0 census (2026-10-11):
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
