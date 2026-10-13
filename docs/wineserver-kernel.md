# wineserver in the gex kernel (design, 2026-10-12; stream K)

Status: design for review. Nothing here is built yet. Gex branch
`wskern/main` (off master 3d1de752); Wine branch `wskern/wine` (off
`mspaint/wine` b4ad6a68bb7, which holds all of W3's `wasm32-w3`
4a16fbb8aca).

## 1. Why

Under W3, wineserver is a thread of the first Windows process
(wine-native.md §3.5, `server/wasm32_inproc.h`). Every server call
from any process, the first one included, still goes through Wine's own
transport:

1. the client writes the request to its request pipe;
2. the server thread wakes in `epoll_pwait`, reads it and runs the
   handler;
3. the server writes the reply to the reply pipe;
4. the client wakes in `read`.

P measured mspaint in the page on worldmachine (master + klock-6
prototype, `scratch/P/sc-diag`):

| Click | Requests | Median round trip | Summed wait | Time to first publish |
|---|---|---|---|---|
| Tool | 93 | 0.29 ms | 28 ms | 44-52 ms |
| Ribbon tab Home | 641 | 0.21 ms | 152 ms | 258 ms |

A round trip is two cross-Worker wake hops of about 0.10-0.14 ms each,
plus about 0.015 ms of the server's own work (the handler and its
`epoll_pwait`, `read` and `writev`). So wake latency, not server CPU, is
what this design buys down. Most of the Home requests come from
WH_CALLWNDPROC hooks (3 requests per hooked message), `get_window_property`
and 266 opens of uxtheme's ThemeLoadedEvent: all pure in-memory handlers.

Three further costs come from the server living in one process's memory:

- **The session-read patch.** win32u in any other process reads the
  session (desktop, queues, windows) through `read_session_data`
  requests into a local mirror that stays good for 16 ms
  (winstation.c, Wine fork 92bf095922a, 86708150604, 4a16fbb8aca). Other
  processes' changes arrive up to 16 ms late, and each refresh is a
  round trip.
- **The in-process-only USD mapping.** The server maps the user shared
  data at the client's own 0x7ffe0000 (mapping.c), which only the first
  process can see. Children keep their own clocks (af062452d87,
  bf5af1fc23e).
- **Asymmetry.** The process hosting the server reads the session for
  free and the others pay a request; a game started by a launcher is
  slower than the same game started directly (§3.17: a full 256-key poll
  is 1.3 ms a frame in a child, 0.2 ms in the server's process).

## 2. Constraints

- **The browser is the target.** Each gex process has its own
  `WebAssembly.Memory` (a SharedArrayBuffer in the MP build). Memories
  cannot alias a page. A module can import several memories
  (multi-memory) and copy between them with `memory.copy`: that is how
  the global kernel module (`shims/kernel`, gex-kmem) reaches a guest's
  buffers.
- **Native gexc is for debugging.** It runs every guest process in one
  host process, each with its own `wasmtime::SharedMemory`. Whatever the
  page does, native does the same way, so native stays the oracle.
- **No new KLock dependence** (kernel-locking.md). W's step 2
  (step/klock-7 d26f0d41) gives waits per-object queues: wait slots
  (`k_ws_self`, `k_ws_alloc`, `k_ws_seq`, `k_ws_park`) and wait queues
  (`k_wq_new`, `k_wq_add`, `k_wq_notify`, `k_wq_chain`). There is no
  global wake word any more. The server must use these, not a wake of
  its own, and must not park while holding a lock that the kernel's
  park path takes.
- **Wine's semantics stay.** Requests and replies, fd passing, waits,
  APCs, async I/O, and the select path that blocks clients. The server
  stays single-threaded in its logic: one request runs at a time.
- **The user rejected a same-process direct-call shortcut.** The design
  must treat every process equally, the first one included.
- **Eject ships no cross-process shared memory** (§9).
- **SP build (no SAB):** every process shares one thread through the JSPI
  scheduler. The design must work there with no locks.

## 3. Options

The two questions are where the server's code and state live, and how a
client's thread gets a request served.

### 3.1 Where the server lives

**A. Link wineserver into the global kernel module.** The server's C
objects would be linked into `shims/kernel` and its state would sit in
the kernel's memory.

- The kernel module is Rust with no POSIX libc. Wineserver needs one:
  malloc, stdio for the registry, `open`, `mmap`, `epoll`, `sendmsg`,
  signals. All of that would need a libc built on kernel internals,
  inside the kernel.
- A file descriptor table lives per process, in each process's shim
  (`Kernel` in `HOST`), not in the global kernel. So the server would
  need a second fd-table implementation of its own.
- Wine-specific code would grow the module that every Worker of every
  program instantiates, Linux programs included. That module's static
  memory is already sized by hand (`STACK_COUNT`).
- Rejected: most of the work is a second libc and a second fd layer.

**B. A kernel-hosted server module on a memory of its own, with no
process.** This keeps the separate libc problem. Its syscalls still need
an fd table, a memory map and file access, which today only a process
has.

**C. A kernel-hosted server process on a runtime-owned server memory
(chosen).** wineserver becomes its own gex process again, as it is
upstream: `wineserver.wasm`, a native wasm32 program built with our musl
and started by `start_server`.

- **The server memory (SM)** is a `WebAssembly.Memory` (natively a
  `wasmtime::SharedMemory`) that the runtime creates and owns, with the
  kernel module's lifecycle: made when the global kernel is, per Wine
  session (prefix), and dropped with the kernel or at the session's end.
  It is not a guest mapping:
  - no guest `mmap` can name it;
  - it appears in no process's address space;
  - only the server module and the door glue import it.

  So it is runtime memory, as the kernel module's is, and never a
  guest-visible `MAP_SHARED` region (coordinator, 2026-10-12). Whether a
  build shares it across processes is then a build option (§9).
- **P_s.** The kernel marks the server process as the session's server.
  P_s is instantiated on SM as its linear memory, in place of a fresh
  memory. P_s's own shim, the code it translates and its mappings live
  in a process memory of their own as usual; SM holds only the wasm32
  server image (data, heap, stacks). P_s exiting does not free SM; the
  next P_s of the same session would start on a fresh SM.
- Every Worker of every Windows process in the session can instantiate
  the server's module on SM, as a thread of P_s would.
- P_s has a normal pid, fd table and memory map, so the server's
  syscalls need nothing new.
- C is B, built from machinery that already exists: a process and its
  per-Worker instances, on a memory the runtime provides.

### 3.2 How a request is served

**H. Handoff to a server Worker through a shared slot.**

1. The client copies the request into a slot in SM.
2. It bumps the slot's wait queue.
3. P_s's thread serves it and bumps the client's slot.

- With both sides parked, this is still two wake hops: the 0.2 ms we
  have now, minus only the pipe syscalls.
- It only wins if both sides spin. A spinning server burns a core all
  session, or for an adaptive window after each request. A spinning
  client burns its own core.
- P is prototyping spin-before-park on today's pipes, which measures
  the best H can do.

**D. Door call: the client's thread runs the request in the server
(chosen).** This is thread migration, as in Solaris doors or LRPC.

1. The client's Worker calls into its own instance of the server
   module, on SM.
2. It takes the **server lock**, a word in SM.
3. It copies the request into SM, runs Wine's `call_req_handler` as the
   requesting thread, and copies the reply back.
4. It releases the lock.

- There are no wakes, no pipes and no syscalls on the client's side.
- What is left is a lock, two copies and the handler (§8).
- Wineserver's handlers never block: every reply is synchronous, and a
  wait is a `STATUS_PENDING` reply plus a later wakeup. So a handler run
  to completion on the caller's thread is the server's own model.

D is the design. H is kept as a measurement point in the prototype
(`WINEGEX_DOOR=handoff`), because the brief asks for both to be
measured, and as the place where P's spin work lands if D falls short.

## 4. The chosen design

```
 client process (own memory)            server memory SM (runtime-owned)
 ┌──────────────────────────┐           ┌──────────────────────────────┐
 │ ntdll.so server_call ────┼─ door ───▶│ server lock │ door buffer     │
 │ win32u shared reads ─────┼─ copy ───▶│ session block (16 MiB)        │
 │ wait on own slot ◀───────┼─ k_wq ────│ per-thread wake rings         │
 └──────────────────────────┘           │ objects, handles, queues, ... │
                                        └──────────────────────────────┘
                                          ▲ P_s main thread: the pump
```

### 4.1 The server process P_s

- **Build.** Wine's server builds as its own wasm32 program again.
  `wasm32_inproc.h` and its renames go: there is no shared loader and no
  `wineserver_main`. Two new exports:
  - `ws_door_dispatch(thread, len, reply_max)`, which runs one request
    from the door buffer;
  - `ws_door_area()`, which gives the addresses of the lock, the door
    buffer and the door stack.
- **Start.** ntdll's `start_server` forks and execs `wineserver -f`, as
  upstream does.
  - In the page, P_s is a process with one Worker (its main thread).
    The exec of a program marked as a door server (a GEXWASM header flag,
    as `isW3Program` reads the i386 flag) asks the runtime for the
    session's SM and instantiates the wasm32 image on it.
  - Once its master socket is up, P_s publishes itself as the door
    server with `gex_door_publish()`: a call into the shim, which tells
    the runtime that SM now holds a live server and which module it
    runs.
  - The runtime already holds SM; it records the server's compiled
    `Module` beside it, per session (per prefix). It hands both to every
    Worker it creates from then on, in the Worker's init message.
  - A Worker created earlier (pid 1's, which started the server) gets
    them as a message at its next trip to JS. Until then it uses the
    pipes (§4.6).
- **The pump.** P_s's main thread runs Wine's `main_loop` (fd.c) with
  two changes:
  - it holds the server lock except while in `epoll_wait`;
  - a **kick pipe** (a global-kernel pipe, `k_pipe_*`) is in its epoll
    set.

  The pump does what is not a request: timeouts, fd events (client
  sockets closing, async I/O on files, sockets and named pipes, inotify),
  and the proxied syscalls (§4.4).
- **Lifetime.** As upstream: P_s exits when the last client is gone and
  the master socket's linger timer expires. The page's session keeper
  (wine-native.md §3.17, "Session reuse") keeps one client alive across
  programs.

### 4.2 The door call

Client side (ntdll.so `server_call_unlocked`, wasm32):

```c
if (data->door_thread && door_fits( req ))
    return __wine_door_call( data->door_thread, req );   /* import */
/* else Wine's pipes, unchanged */
```

`__wine_door_call` is an import of the Wine module, resolved to an
export of the **door glue**. The glue is a small wasm module per Worker.
It imports the client's memory, SM and the kernel module's memory, so
the call is wasm to wasm with no JS on the path. Its steps:

1. **Lock.** The server lock is a robust futex-style word in SM: the
   owner's wait slot (`k_ws_self()`) and a waiter count. CAS it to this
   Worker's slot. If it is taken, spin about 2 µs, then wait as a
   step-2 waiter on the lock's queue (`k_wq_add`, re-check,
   `k_ws_park`), and loop.
   - `k_ws_park` is the same instruction as a raw `memory.atomic.wait32`
     and never traps, so rule 4.1 holds (W, 2026-10-12). In the SP build
     the same `k_ws_park` suspends through JSPI (`sched.parkSlot`); a raw
     wait32 there would deadlock the one realm.
   - The pump holds the lock only while it handles events, never across
     `epoll_wait`.
   - The page's main thread never makes door calls (it runs no guest).
2. **Request in.** `memory.copy` of the request header (64 bytes) and its
   data vectors from client memory into the **door buffer** in SM. There
   is one door buffer, since the lock admits one caller; it is 64 KiB
   and grows under the lock. The reply area is `reply_max` long after
   it.
3. **Dispatch.** Point the server instance's `__stack_pointer` at the
   **door stack** (one, in SM, for the same reason) and its TLS base at
   the server's main TLS block. Then call
   `ws_door_dispatch(thread, ...)`, which:
   - checks the `struct thread *` token (§4.3);
   - sets `current`, as `read_request` does, and runs `call_req_handler`;
   - has `send_reply` write into the door buffer instead of
     `thread->reply_fd`;
   - processes, at the end, the timeouts that have expired, and the
     wakeups and signals queued during the handler (§4.4, §4.5).
4. **Kick.** If the handler armed a timeout earlier than the pump's
   deadline, or changed the pump's fd set (§4.4), write one byte to the
   kick pipe. The pump's deadline and fd-change count are words in SM.
5. **Reply out.** Copy the reply header and data into client memory.
6. **Unlock.** Release, and `notify` if there are waiters.

Signals: `wine_server_call` already blocks `server_block_set` around the
call, and the door call never enters the client's shim. So no gex signal
reaches the caller mid-call.

**Process death mid-door.** A Worker killed asynchronously
(`worker.terminate()` on SIGKILL or process exit) while inside a door
would leave the lock held and the server half-updated. So the glue keeps
a per-Worker **in-door word** in the kernel's memory. Termination of a
process sets a pending flag and waits for that word to clear, with a
bound; the glue checks the flag at door exit and ends its thread itself.
The check goes where a process actually ends (W): the shim's
`exit_process` and `set_exit` paths, the kernel's `k_exit`, and
host.mjs where it calls `worker.terminate`. W's lifecycle steps 8-9 will
call the same check rather than add another.

A bounded wait still leaves a Worker that trapped or crashed inside the
door. The lock word records its owner's slot, robust-futex style. The
exit path, or the next locker that finds the owner's Worker gone (its
slot freed), takes the lock over and marks the server poisoned (§11):
a handler cut off midway cannot be repaired, so the session ends as it
does today when wineserver dies. Natively, threads are not killed
asynchronously, so only the page needs the in-door word.

**What `door_fits` excludes:**

- requests whose data is over 1 MiB in either direction (they take the
  pipe; rare);
- the start-up requests before `init_first_thread` / `init_thread` has
  returned a door token.

Everything else goes through the door: `select` and every request that
sends or receives an fd included (§5).

### 4.3 Thread identity

- Today the server identifies the caller by which request pipe a request
  arrived on.
- A door caller presents a token instead: the address in SM of its
  server-side `struct thread`.
  - `init_first_thread` and `init_thread` return it in a new reply field,
    `door_thread`.
  - `ws_door_dispatch` checks a magic word and the thread's state before
    using it.
  - It is not a security boundary, no more than today's pipes are.
- The server keeps a thread's pipes as well, for fallback and for
  shutdown detection, so a door thread's teardown is upstream's.

### 4.4 Syscalls the server makes inside a door

The server instance in a client's Worker has no shim of P_s's behind it:
the client Worker runs the client's shim. Its musl `syscall` import is
bound by the glue:

- **Served in place, with no process state:**
  - `clock_gettime` (the global clock base, `k_boot_secs`);
  - `getpid` (P_s's pid, a constant in SM);
  - `gettid` (the pump's tid);
  - `kill` and `tgkill` to other processes: `k_kill` in the global
    kernel, which is how signals cross processes today.
- **Deferred to the pump:** `epoll_ctl` (from `set_fd_events`). The glue
  queues the change in SM (fd, events) and kicks; the pump applies the
  queue before its next `epoll_wait`. Wine already tolerates late
  application: set_fd_events only states what the next poll should
  report.
- **Proxied to the pump: everything else.** Examples are `open`, `read`,
  `pread`, `mmap` of a file mapping, `sendmsg` and `recvmsg` with fds,
  `ftruncate` and `fstat`. The call proceeds in four steps:
  1. The glue writes the call (number, arguments, and data copied into
     an SM buffer) into a proxy slot, notifies the pump's wait slot and
     kicks.
  2. It waits on its own wait slot (`k_ws_park`) for the result. The
     server lock stays held: the handler is mid-flight on the one door
     stack, so it cannot be given up and re-taken the way a condvar
     would.
  3. The pump makes the syscall as P_s on its own shim, writes the
     result and notifies the caller's slot.
  4. The glue copies the results back.

  **No deadlock: the pump serves proxies without the server lock, from
  every place it can wait** (W pointed out that a pump waiting for the
  lock would otherwise deadlock against a caller waiting for the pump).
  Outside the lock the pump is only ever in one of two waits:
  - `epoll_wait`, which the kick pipe ends;
  - acquiring the server lock. Here the pump waits on its own slot,
    which both an unlock and a proxy post notify; it serves any posted
    proxy and then tries the lock again.

  A proxied syscall itself never needs the server lock: it is P_s's own
  syscall on P_s's shim. The server's fds are non-blocking, so it cannot
  wait on a client either. No client needs the pump to finish a door
  call except through the proxy, which the pump serves from either
  wait.

  A proxy costs about two wake hops, today's round trip, so it must stay
  rare on hot paths. `GEX_DOOR_STATS=1` counts proxies per request code
  and syscall. The prototype's first job is that census on P's mspaint
  clicks and on Undertale (§10).
- **Parking under the server lock.** The proxy wait is the only place
  a door caller parks with the server lock held. It never traps and
  never takes KLock, and the pump never needs the lock to end it (above).

### 4.5 Waits, wakeups and APCs

Wine's wait is:

1. a `select` request;
2. if the reply is `STATUS_PENDING`, a blocking read of the thread's
   wait pipe for a `wake_up_reply {cookie, signaled}`
   (`wait_select_reply`);
3. on the server side, `send_thread_wakeup` writes that pipe.

The design keeps the shape and replaces the pipe:

- **A wake ring per thread, in SM.** Eight `{cookie, signaled}` entries
  with head and tail. `send_thread_wakeup` appends to it and notes the
  thread's queue for notifying; the queue is notified after the handler,
  at door exit or at the end of the pump's turn. The ring keeps Wine's
  "stole another reply" case: the waiter takes the entry whose cookie is
  its own, and the others stay.
- **The client's wait** uses W's step-2 protocol (§2):
  1. `seq = k_ws_seq(k_ws_self())`;
  2. `k_wq_add(thread_q, slot)`;
  3. check the ring;
  4. `k_ws_park(slot, seq, -1)`;
  5. `k_wq_del` when done.

  It is a gex wait like any other: a signal to the thread (SIGUSR1 for a
  suspend, SIGQUIT, SIGKILL) interrupts it as it interrupts a futex
  wait, and the client loops as `wait_select_reply` does on EINTR. This
  goes through a new syscall, `gex_nr::SERVER_WAIT(ring, queue)`, so
  the shim's park, signal and timeout paths are reused unchanged.
- **The thread's queue** is a `k_wq_new` queue made at `init_thread`;
  `wake_thread` notifies it. There is no global wake, and no Worker is
  woken that is not waiting on this thread.
- **APCs.** A system APC to a waiting thread is a wakeup with
  `STATUS_KERNEL_APC` (unchanged). To a running thread, the server
  sends SIGUSR1 (`send_thread_signal`, `tgkill`), served in place
  (§4.4). User APCs are unchanged.
- **Timeouts.** The client passes absolute monotonic deadlines (the
  shared `k_boot_secs` base). The server's timeout list fires them from
  the pump; the pump's sleep deadline is the list's head. A door call
  that arms an earlier one kicks (§4.2, step 4).
- **ntsync.** W3.6 (ntsync in gex-kernel) is orthogonal: it would take
  waits on events, mutexes and semaphores out of the server altogether.
  The door then serves what is left.

### 4.6 Fallback and coexistence

- Every thread keeps its request, reply and wait pipes.
- The pump keeps serving pipe requests (under the server lock, as one
  request at a time), so a Worker without the door, an over-size request
  or `WINEGEX_DOOR=0` all work.
- This also makes A/B measurement a switch, not a rebuild.

## 5. File descriptors

- **Client to server** (`wine_server_send_fd`, SCM_RIGHTS on the client's
  Unix socket, received by the server's `receive_fd`): unchanged.
  - The client's `sendmsg` happens before the request, in the client's
    own shim.
  - Inside the door, the server's `receive_fd` is a `recvmsg` on P_s's
    end of that socket, which is proxied (§4.4).
  - The socket pair and the descriptor tokens are global-kernel objects
    already (`k_sock_write_fds`, `k_sock_read_fds`, `scm_rebuild`).
- **Server to client** (`send_client_fd`, then `wine_server_receive_fd`
  after the reply): the server's `sendmsg` is proxied; the client's
  `recvmsg` is its own.
- **Cost.** Each fd transfer is a proxy round trip, the same as today.
  Opening files is not on the click paths P measured. If the census
  says otherwise, the next step is an in-kernel handover that skips the
  socket: `k_fd_give(from_pid, fd, to_pid)`, built on the existing
  tokens, served in place.

## 6. Session memory and the user shared data

### 6.1 The session

- **Where.** The session mapping is one 16 MiB block in SM (mapping.c's
  wasm32 size stays: one block, never grown).
- **Reading it.** win32u in every process (no special case for any
  pid) keeps its 16 MiB **mirror**, the same layout as today's
  `session_mirror`. It reads through two door-glue imports:
  - `ws_shm_copy(dst, offset, size)`: one `memory.copy` from SM into
    the mirror, done in `shared_object_acquire_seqlock` and the existing
    refresh sites (winstation.c, window.c);
  - `ws_shm_load64(offset)`: in `shared_object_release_seqlock`, which
    compares the object's *live* `seq` against the one copied.
- **Coherence.** That is an ordinary seqlock over a snapshot, and the
  copy is coherent:
  - there is no TTL and no `__wine_session_writes` counter;
  - there are no `read_session_data` requests;
  - another process's change is seen at the next read.
- **Removed:** the session-read patch's request path and its TTL, and
  `wineserver_session_data`.
- **Cost.** A copy of `sizeof(object) + 512` bytes is about 0.1-0.3 µs.
  A full 256-key poll costs about 50-80 µs a frame in every process.
  Today that is 1.3 ms in a child and 0.2 ms in the server's own
  process, which now costs about the same as a child. The copy can be
  sized per object type later if a game's poll shows up.

### 6.2 The user shared data

- **The problem.** KUSER_SHARED_DATA at 0x7ffe0000 is read by i386
  program code with plain loads, so it must be in the client's own
  memory.
- **The fix.** Every process keeps its own page:
  - filled from the server's USD mapping when ntdll maps it, as a copy
    (`ws_shm_copy` of the mapping, which lives in SM);
  - its clocks ticked by ntdll every 16 ms, as children do today
    (bf5af1fc23e).
- **Removed:** the server's `MAP_FIXED` at 0x7ffe0000 (mapping.c) and the
  "only pid 1 sees the server's USD" rule.
- **Other USD fields.** The server writes them once at start (processor
  features, XState, time-zone fields at start-up). A time-zone change is
  a request and rare; ntdll refreshes its copy on the reply of
  `set_timezone`-type requests.

### 6.3 Other mappings

- `mapping.c`'s other wasm32 branch, "a client's view aliases the
  server's mapping", is only for the session and goes with it.
- Ordinary section views (file mappings, anonymous shared sections
  between processes) are the existing per-process `MAP_SHARED`
  question (wine-native.md §3.5, "Known gap"). They are not in scope
  here. The design does not make them worse.

## 7. KLock interaction (agreed with W, 2026-10-12)

- **A door call takes no KLock.** The client's side is not a syscall: it
  never enters the client's shim. The server side runs on the server
  lock, which lives in SM and is not KLock, any process's.
- **The server lock** is a robust word (owner slot, waiter count) waited
  on through step 2's slots and a queue of its own:
  - MP page and native: `k_ws_park`, which never traps (rule 4.1 holds);
  - SP: the same calls, which suspend through JSPI. Uncontended, the lock
    is one CAS and costs SP nothing else; the smoke leg (busybox, gears)
    must keep passing.
- **Proxied syscalls** run on the pump as P_s's own syscalls. They take
  whatever P_s's shim takes, so they inherit each KLock step as it lands.
  The pump serves them from both of its waits, never under the server
  lock (§4.4), which is W's deadlock condition.
- **Waits** use step 2's wait slots and per-object queues: one queue per
  server thread, notified at door exit and at the end of the pump's turn.
  The pump's epoll is a gex wait like any other; its kick pipe is a
  global pipe with its own queue (step 6b's per-object slots once
  landed).
- **Termination.** The in-door word is checked at the shim's
  `exit_process`/`set_exit`, `k_exit` and host.mjs's `worker.terminate`;
  W's steps 8-9 call the same check (§4.2).

## 8. Expected cost per request

These are estimates. The prototype measures them on worldmachine and
edger.

| Step | Today (pipes) | Door |
|---|---|---|
| client `writev` + `read` through the shim and global pipes | several µs of syscalls, plus wait | none |
| wake the server Worker | 100-140 µs | none |
| server `epoll_pwait`, `read`, `writev` | part of the 15 µs | none |
| handler | about 2-10 µs (part of the 15 µs) | the same |
| wake the client Worker | 100-140 µs | none |
| lock, two `memory.copy`, glue | none | about 0.2-1 µs uncontended |
| **round trip** | **210-290 µs** | **about 3-12 µs** |

So, per click:

| Click | Requests | Wait today | Expected with the door |
|---|---|---|---|
| Tool | 93 | 28 ms | about 0.3-1.1 ms |
| Ribbon tab Home | 641 | 152 ms | about 2-8 ms |

These hold if none of those requests needs a proxied syscall. The
census in step 2 of §10 checks that before the numbers are trusted.

- **Contention.** Callers serialize on the server lock, as upstream's
  single-threaded server serializes them. A caller that finds it held
  spins about 2 µs, then waits.
- **Other per-request costs:**
  - handoff H with both sides spinning: about 2-5 µs plus a core each;
  - H parked: no better than today.

## 9. Eject

**gex-eject ships no cross-process shared memory** (user, 2026-10-07).
SM is runtime-owned memory with the kernel module's lifecycle, never a
guest-visible `MAP_SHARED` region (§3.1). So whether it is shared is a
build option, not a design fork (coordinator, 2026-10-12).

**Decided (user, 2026-10-12): allowed.** In eject, SM may be shared
across a program's Windows processes as a runtime-owned memory, like the
kernel module's memory. It is not guest-visible shared memory, so the
"no cross-process shm" rule does not apply to it. The two build options
stay:

- **Private SM (the build option eject can always take).**
  - The process's Workers alone import SM.
  - Every request is a door call, with no pump Worker. The pump's work
    (timeouts, fd events, proxies) runs at door entry and at the
    program's own waits, as in the SP build.
  - This fits a one-process eject today: the web eject runtime is one
    process and one thread (eject.md, "Limits of the web build"), and a
    Wine program cannot be ejected yet at all.
- **Shared SM (the page and native default, and eject's for several
  Windows processes).**
  - SM is imported by the Workers of every process in the session, as
    the kernel module's memory is.
  - An eject build of several Windows processes uses this (allowed
    above).

The code is the same in both; the option chooses which Workers receive
SM and whether a pump Worker exists.

## 10. Plan

1. **This document.** It goes to the coordinator before major
   implementation.
2. **Prototype, request/reply only** (on `wskern/main` and `wskern/wine`):
   - **Wine:**
     - wineserver as its own wasm32 program;
     - the door export;
     - the server lock in `main_loop`;
     - the `door_thread` reply field;
     - `server_call_unlocked`'s door branch;
     - `GEX_DOOR_STATS`.
   - **gex:**
     - `gex_door_publish`;
     - the door glue (page and native);
     - the proxy slot and the pump's kick pipe;
     - the in-door word.
   - **Not yet changed:** waits still use the wait pipes, and the session
     read is unchanged.
   - **Measure:**
     - a nullcall microbenchmark (a door `get_process_info` against the
       pipe), natively for correctness and on edger for time;
     - the proxy census;
     - P's mspaint per-click server wait (`scratch/P/sc-diag`, `probe.sh`,
       worldmachine);
     - Undertale on the bench boxes through bench.sh.
3. **Full migration:**
   - waits on the wake rings and step 2's queues (§4.5);
   - the session mirror over `ws_shm_copy` (§6.1);
   - the per-process USD (§6.2);
   - the session keeper (wine-native.md §3.17) on P_s;
   - removal of `wasm32_inproc.h`.
   - **Legs:**
     - the W3 page echo;
     - nullcall;
     - the Undertale page leg;
     - P's uiribbon leg;
     - the nt tests;
     - the SP smoke.

### 10.1 Prototype status (2026-10-12)

Built on `wskern/main` (gex) and `wskern/wine` (Wine, off `mspaint/wine`).
Request and reply go through doors natively (gexc) and on the page (MP
build). How it differs from §4:

- **The server memory is P_s's own linear memory**, held by the runtime
  for the door's lifetime: the page broadcasts it (`doorPublish`) and
  every Worker instantiates the server program on it; natively every
  process sees it. A runtime-owned memory made before P_s exists is the
  next step; nothing in the door ABI depends on which it is.
- **The first process.** The Worker of the process that started the
  server never sees the broadcast: a busy Worker takes no messages. That
  process runs its program again as a child, which starts with the door,
  and passes its exit on (`wineserver_relaunch`, `gex.door_missing`). The
  server signals "ready" only after the door is published.
- **The clock.** The door host passes the time and CLOCK_BOOTTIME into
  `__wine_door_dispatch`, so `set_current_time` makes no system calls
  out of the door.
- **No blocked signals.** `wine_server_call` runs a door call without its
  two `sigprocmask` calls: one host call cannot be interrupted by a
  signal handler that reuses the request pipe.
- **The page's door instance** gets its own function table, sized from
  the program's `dylink.0` and its binding list (`X` slots, GOT), and it
  starts under the door lock (the start runs on the one door stack).
- **Not yet done** (at the prototype; see §10.2 for what followed):
  - waits still go through the wait pipes;
  - session reads are still `read_session_data` requests, now through
    the door;
  - no in-door word: a trap releases the lock without poisoning the
    server;
  - the SP build keeps the pipes (`door_missing` returns 0 there).
- **Census tools:** `?doorstats=MS` or `GEX_DOOR_STATS=MS` prints one line
  per Worker every MS. It gives per-call copy-in, dispatch and copy-out
  times, the in-door system calls, and the proxied calls with their wait.

Measurements (edger, bench.sh; numbers are per request unless marked):

| | pipe | door |
|---|---|---|
| nullcall (`get_thread_info`), page under Node, first door cut | 26.1-38.0 us | 5.9-8.3 us |
| nullcall, Chromium, with the clock passed in | 28.2-37.8 us | 4.6-6.4 us |
| nullcall, Chromium, also without sigprocmask | 29.0-36.0 us | 2.1-3.7 us |
| mspaint click-to-present, tool-fill median | 103-122 ms | 58-77 ms |
| mspaint tool-pencil | 117-118 ms | 57-62 ms |
| mspaint tab-view | 359-374 ms | 211-212 ms |
| mspaint tab-home | 652-674 ms | 480-697 ms (noisy) |

On the same box a request-free unix call (`NtQueryPerformanceCounter`)
costs 0.55-1.5 us, so a door request is about 1.5-2.5 us above the
floor. Census during the mspaint clicks (Chromium, 1 s lines): the UI
thread makes about 1300-4100 door calls a second at about 7 us each (copy
in 1.9-3.4 us, dispatch 2.2-5.3 us, copy out 0.9-2.7 us), and 1-19
proxied calls a second, which wait 0.2-3.1 ms in total. The proxies that
dominate boot (readlink from musl's `realpath` in `open_fd`, pread,
statx, open and close) are rare once the program runs.

**Undertale** (Chromium, `scratch/K/ut/run-ut.sh 300`, W's w3d dist with
K's loader and server over it, two runs each, alternating):

| | pipes (`WINEGEX_DOOR=0`) | doors |
|---|---|---|
| fps over the last 120 s | 21.6, 22.9 | 24.3, 22.8 |
| frames in 300 s | 2595, 2748 | 2906, 2737 |
| first frame | 34.3 s, 33.8 s | 36.0 s, 34.9 s |

Within run-to-run noise, Undertale's frame rate does not change. Once
the game runs it makes few server requests: its time goes to drawing,
not to waiting on the server. (W's own stack measured 25.1 fps in the
same harness.)

Undertale with doors first needed a shim fix that has nothing to do with
doors (`173a2c30`). When a Wine thread exits under its start routine's
call-out, the run loop entered a stale resume at the call-out's return
pad. That breakpoint killed the process with status 133. It killed
winedevice in every page run, pipes too. With doors, services also lost
a thread that way before setting `__wine_SvcctlStarted`, and wineboot
hung. The leg for it is `tests/w3/threadexit.sh`.

### 10.2 Full migration status (2026-10-12)

What §10 item 3 has built so far, on `wskern/main` and `wskern/wine`:

- **Session mirror (§6.1): done.** The server publishes its session
  block in the door area (+128 base, +132 size, `door_set_session` from
  `set_session_mapping`). win32u copies the live bytes with
  `gex.session_copy` and checks a seqlock's release against the live
  sequence with `gex.session_seq` (page: `#sessionCopy`, `#sessionSeq`;
  native: `door::session_copy`, `session_seq`). There are no TTL and no
  `read_session_data` requests while a door session exists; without one
  (pipes, `WINEGEX_DOOR=0`, SP) the request path stays. Leg:
  `tests/w3/session.sh`. A child sees its parent's window, and sees it
  gone as soon as the parent destroys it. Natively, 20000 IsWindow calls
  with a 1 µs TTL made no requests (6637 door calls in the whole run).
- **Wakeups in place (§4.5, first step): done.** The full wake-ring
  design is not built. What is built removes the proxy that the census
  blamed: `send_thread_wakeup`'s `write(2)` to the client's wait pipe.
  `init_thread`, which the pump serves, names the pipe to the host
  (`gex.wake_pipe`: the global pipe id in the page, the file natively).
  A door then writes the wakeup itself (`gex.wake_write`), and EPIPE
  still ends the thread. The client's wait is unchanged: a blocking read
  of its wait pipe, which is already a step-2 wait in its own shim.
  `WINEGEX_DOOR_WAKE=0` restores the proxied `write(2)`. In a native
  echo, the 45 proxied writes go to 0. In the page echo under Node,
  101 proxied writes go to fewer than 11 (no longer among the 16 most
  frequent proxied calls).
- **Per-process USD (§6.2): done.**
  - Every process keeps a private page and a 16 ms clock thread
    (`virtual_map_user_shared_data`), since the server is its own
    program.
  - Until now every client's page was zeros, apart from its own clocks:
    no processor features, version 0, no processor count. Both writers
    (ntdll's first-boot fill and wineboot) wrote a mapping that gex
    copies, and the server's own page never reached the file either.
  - Now the server publishes its page in the door area (+140), and each
    process copies it at start (`gex.usd_copy`).
  - ntdll's first-boot fill goes to the server's page
    (`gex.usd_publish`) and to the filling process's own page. It now
    includes init_peb's version (10.0.19045), because wineboot's write
    is still lost.
  - A probe (`scratch/K/w3t/usd.c`) now reads version 10.0, the
    processor features and the processor count, natively and in the
    page.
  - `wasm32_inproc.h` and the loader's `wineserver_session_data` /
    `wineserver_user_shared_data` stubs are gone.
- **In-door word and poisoning (§4.2): done for the page and native.**
  - The page: host.mjs defers a Worker's termination while it is in a
    door (`#inDoor`), for up to 10 s.
  - A door call that throws marks the server poisoned (area +124). So
    does a lock that stays stuck that long.
  - Native: a trap in a door now also poisons the server.
  - A poisoned server refuses doors. Its pump exits at its next lock and
    first SIGKILLs every client by Unix pid (`door_kill_clients`).
    Without that, the session hung: a client thread blocked in a wait
    reads a pipe whose write end it holds itself, so it never sees the
    server go. Wine behaves that way everywhere; a server that simply
    dies (`WINEGEX_SERVER_DIE`) still leaves its clients hanging, pipes
    or doors. With `WINEGEX_DOOR_TRAP` the session now ends in about 20 s
    in the page under Node, and natively, instead of hanging.
  - Test hooks: `WINEGEX_DOOR_TRAP=N` (the Nth door call traps) and
    `WINEGEX_SERVER_DIE=N` (the Nth request exits).
- **A door bug found on the way: the door's register context was too
  small.** `door_init` allocated 34 slots (272 bytes), only the
  system-call registers. gex's kernel writes past them: `clock_call`
  stores the last call's number at 45 * 8. So an in-door
  `clock_gettime64` wrote 403 into the next heap block. Natively, with
  W's page prefix, that block was a new process's handle table, and the
  next `close_handle` asserted (`obj->handle_count`). The context is now
  4 KiB (the i386 context is 1120 bytes). The page glue writes only slot
  0, but the context is the same memory there, so the fix covers both.
  The area now gives the context's size (+136), and the host opens no
  doors when it is smaller than gex's register context. The
  `tests/w3/handles.sh` leg runs many processes making and closing
  handles and threads at once. Natively it passes. In the page it trips
  a call-out signature mismatch (`RefCell already borrowed` after a trap
  at the return pad), which pipes show too; that is W3's, not the
  door's.
- **Session keeper:** not started. With the server as its own program,
  the keeper reduces to running the server persistent (`-p`) and having
  the page start later programs against the running session.
- **SP:** see §10.3.

Click-to-present on edger (Chromium, P's latency probe through
`site/edger-lat.sh`, the median of three clicks per run, one bench call
per run, alternating builds):

| Click (median of 3, ms) | s4-b: doors, no mirror | s4-s: + session mirror | s5: + wakeups in place, context fix |
|---|---|---|---|
| tab-view | 375*, 222 | 223, 227, 233 | 205, 206 |
| tab-home | 1011*, 533 | 508, 525, 592 | 483, 503 |
| tool-fill | 90*, 62 | 83, 81, 71 | 57, 70 |
| tool-pencil | 91*, 59 | 62, 68, 65 | 74, 70 |
| dropdown-size | 94*, 72 | 64, 68, 56 | 58, 55 |
| color-red | 77*, 65 | 78, 76, 63 | 57, 73 |
| window shown (s from start) | 51.2*, 41.2 | 41.1, 39.7, 36.9 | 37.6, 36.6 |

\* s4-b's first run was the box's first after a sync and is slow
throughout; its second run is the comparable one.

- **The session mirror alone** changes no click beyond run-to-run noise
  (s4-b against s4-s).
- **With wakeups in place**, tab-view is about 10% faster than with the
  mirror alone (205-206 ms against 223-233 ms), and tab-home is 483-503 ms
  against 508-592 ms.
- **Tool and colour clicks** stay within noise. By the census, a click
  spends a few ms in door calls and well under 1 ms in proxies, so what
  remains of a click is not in the server.
- **The wake-ring design** (§4.5) would save only the client's read of
  its wait pipe, which is already a step-2 wait in its own shim. It is
  not worth building on these numbers.

### 10.3 Doors in the SP build (plan)

W3 Wine does not run in the SP build today, with doors or without. The
W3 cmd echo under `GEX_SINGLE` fails right after the first
`rt_sigprocmask` with "trying to suspend JS frames". So SP doors wait for
W3 in SP. When it runs, two things change for doors:

- **No lock contention.** One thread runs every process. A door call
  takes the lock with one CAS, and the lock never needs a wait.
- **Proxies must suspend.** A proxied system call has to wait for the
  pump's fiber to run it as the server process. JSPI cannot suspend
  through the JS frames of today's glue (client wasm, then
  `gex.door_call` in JS, then the server's dispatch, then `gex.syscall`
  in JS).

The options, in order of preference:

1. **A wasm door glue** (§4.2's original shape): a small multi-memory
   module that imports the client's memory and SM. It copies the request
   in, calls the server's dispatch and copies the reply out. Its
   `syscall` import is a `WebAssembly.Suspending` wrapper that posts the
   proxy and awaits the pump. Every frame between the client's promising
   entry and the suspension is then wasm. The MP page gains too: by the
   census, about 1 µs of a door call's 7 µs is the JS around it.
2. **A handoff to the pump.** When a door handler needs a proxy in SP,
   the glue would run the system call on P_s's shim directly, with no
   suspension. That is safe only when the pump's fiber is parked outside
   its shim's borrowed state, which the shim does not promise today.

Until then, `door_missing` returns 0 in SP and the pipes serve.

## 11. Risks

- **Instantiating the server module in every Worker.** That is a wasm32
  program of a few MB: compile once (module cache), and instantiation
  per Worker is about a millisecond. It happens lazily, at the Worker's
  first door call.
- **Proxied syscalls on a hot path.** These make a request as slow as
  today. The census decides what to serve in place next.
- **A server trap inside a door** kills the client's Worker. It is
  treated as a server crash: the in-door word is left set, the lock is
  poisoned, and every client fails as Wine's do when wineserver dies.
- **Asynchronous Worker termination mid-door** (§4.2). The in-door word
  is the fix, and the page needs it before anything ships.
- **Memory.** SM is P_s's whole linear memory: a few tens of MB, plus
  the 16 MiB session. It is mapped by every Worker of the session, at no
  copy cost.
