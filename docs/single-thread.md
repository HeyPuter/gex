# The single-thread build (no SharedArrayBuffer)

Stream SP. Branch `merge/singleproc`, worktree `wt/singleproc`.

The goal is a page that runs the native Debian demo without
SharedArrayBuffer. Such a page needs no COOP/COEP headers, so any static
host can serve it. It has to keep:

- apt over Wisp;
- fork/exec, pipes and signals;
- multi-threaded guests;
- the terminal;
- the Wayland compositor.

The default build (a Worker per guest thread, SAB everywhere) is
unchanged, and runs as before wherever the page is cross-origin isolated.

## 1. Choice: one JS thread, a JSPI stack per guest thread

Every guest thread and process is a **fiber**: the guest runs on a JSPI
stack (`WebAssembly.promising`), and a call that would block suspends that
stack (`WebAssembly.Suspending`). All fibers share one JS thread, the page's
**system Worker** (`web/src/system-worker.js`). Memories are plain, not
shared:

- A process's threads are instances on one memory.
- Every process's kernel instance is on the one global kernel memory.
- They are all in one realm and are passed by reference, never cloned.

### Measurements

Microbenchmarks are in `scratch/SP/mb`. They ran in headless Chrome 153 on
edger under bench.sh (cores 2-8 plus siblings), on a page with no COOP/COEP
(`crossOriginIsolated=false`, no SAB). The raw JSON is in
`scratch/SP/mb/r1-{main,worker}{,-fx}.json`. All figures are ns per
operation.

| operation | main thread | dedicated Worker |
|---|---:|---:|
| plain import call | 17.5 | 16.4 |
| Suspending import, no suspend (inside promising) | 174.1 | 165.7 |
| JSPI suspend+resume round trip, depth 1 | 168.6 | 170.0 |
| … 20 wasm frames deep | 168.2 | 166.8 |
| … 200 wasm frames deep | 166.0 | 171.0 |
| promising export call (new stack), awaited | 1037.6 | 939.0 |
| JSPI context switch, 2 fibers round robin | 311.8 | 271.0 |
| … 8 fibers | 307.0 | 249.0 |
| … 64 fibers | 348.5 | 318.5 |
| hot loop, direct (per iteration) | 2.06 | 2.70 |
| hot loop on a JSPI stack | 2.10 | 2.71 |
| hot loop + back-edge budget poll, refill 65536 | 2.32 | 3.15 |
| hot loop + back-edge budget poll, refill 4096 (`now()` at 0) | 4.47 | 5.82 |
| hot loop + flag-word check per back edge | 3.82 | 4.93 |
| 2000 fibers suspended 20 frames deep: create+suspend, each | 17750 | 19050 |
| 2000 fibers: resume+finish, each | 1700 | 1000 |
| wasm trap caught in JS (stackTraceLimit=0) | 4311 | 4066 |
| MessageChannel round trip (macrotask) | 15355 | 11830 |
| `scheduler.yield()` | 22580 | 18435 |
| `setTimeout(0)`, nested (clamped) | 4070500 | 4026500 |
| `await` (microtask) | 165.1 | 144.9 |
| asyncify unwind+rewind, 1 frame | 302.5 | 525.5 |
| asyncify unwind+rewind, 20 frames | 257.5 | 632.5 |
| WasmFX resume+suspend, depth 1 (`--experimental-wasm-wasmfx`) | 110.8 | 88.0 |
| WasmFX, 21 frames deep | 88.1 | 70.0 |
| WasmFX switch, 8 fibers via a scheduler loop | 124.6 | 93.2 |

Maximum recursion depth (8 i64 locals per frame):

| stack | main thread | Worker |
|---|---:|---:|
| the thread's own stack | 7804 | 3977 |
| a JSPI stack | 7558 | 7550 |

A JSPI stack is about twice as deep as a Worker's own stack.

### What the numbers decide

- **JSPI.** A suspend/resume round trip costs about 170 ns whatever the
  depth: 166-171 ns from 1 to 200 frames. A switch between fibers costs
  250-350 ns, rising only slightly from 2 to 64 fibers. A blocking guest
  syscall already costs microseconds in the SAB build, so this is noise next
  to it.
- **Hot code is free on a JSPI stack.**
  - Main thread: 2.10 ns/iteration on a JSPI stack against 2.06 direct.
  - Worker: 2.71 against 2.70.
  - Hence tier 1, fjit and the LP64 libraries run at full speed under JSPI;
    the page comparison in §6 confirms this on real programs.
- **WasmFX is about 1.9× faster per switch** than JSPI on the main thread
  (88-111 ns against 166-175 ns), and about 2.4× faster on a Worker (70-88 ns
  against 167-171 ns). It still sits behind a flag in Chrome 153, though.
  Node 26 (V8 14.6) crashes compiling it in Turbofan. JSPI ships, so JSPI is
  the choice. WasmFX is the upgrade path once it ships: the fiber layer is
  small (`Sched` in single.mjs), so moving to it later is cheap.
- **asyncify** costs about as much as JSPI on the main thread (258-303 ns)
  and 2-4× more on a Worker (526-633 ns). Its real cost is the
  instrumentation of everything it unwinds through, so it is kept only where
  it already is: wasm32 fork (§4).
- **Traps** cost 4.1-4.3 µs. Translated guests already leave wasm by trap.
  The single build adds nothing to that path.
- **System Worker, not the main thread.** Per-operation costs are close
  (the Worker is a little faster at switches and macrotasks). The main
  thread refuses two things the runtime does:
  - synchronous `WebAssembly.Module` compiles above 8 MB;
  - synchronous XHR with an arraybuffer response.

  Running the guests on the page thread would also stall input and drawing.
  So the page keeps xterm and the compositor's listeners, and the whole
  system runs in one dedicated Worker.
- **Yielding to the event loop** costs 12-15 µs per MessageChannel turn and
  18-23 µs per `scheduler.yield()`. `setTimeout(0)` nests to a 4 ms clamp
  and must never be the yield. The design therefore wakes parked fibers with
  microtasks while an 8 ms slice lasts, and uses one MessageChannel turn
  after it.

## 2. How it works

### Binaries

- The single build uses the shared build's binaries:
  - `gex_browser.shared.wasm`: per-instance stacks and TLS via
    `gex_worker_init`;
  - `seed.shared.wasm`;
  - `gex_kernel.wasm`.
- `enableSingle()` (shims/browser/js/single.mjs) clears the `shared` flag of
  every memory import and definition as the binaries are compiled. It does
  this by patching `WebAssembly.compile`, `validate`, `instantiate`, the
  streaming variants, and `Module`/`Memory`; `instanceof` keeps working.
- Atomic instructions are valid on a plain memory. The one that is not,
  `memory.atomic.wait32`, is never reached: a fiber never waits inside wasm.
- No second build of anything is needed.

### Fibers

- `FiberWorker` runs `worker.mjs`'s `workerMain` in this realm, behind the
  same `postMessage` / `addEventListener` / `terminate` surface, so
  `ProcessHost` is unchanged.
- Messages are not cloned.
- All host↔fiber traffic goes through one FIFO queue drained by microtasks,
  bounded to 8 ms per turn. A child's output and its parent's therefore
  arrive in order, which separate ports never promised.

### Blocking

Where the SAB build waits, the single build suspends:

| SAB build | single build |
|---|---|
| `k_park` / `k_park_futex` (kernel imports, `Atomics.wait` on wake words) | `Suspending` imports that return `sched.park(ns)` / `sched.parkFutex(addr, ns)` promises. `k_wake` / `k_futex_wake` call the kernel and then resolve the matching parked fibers. A parked fiber re-checks `k_wake_seq` / `k_futex_gen` first, so a wake that raced the park is not lost. |
| the guest's run loop calls `enter(ctx, pc)` | `await promising(enter)`: each guest thread's run loop is a JSPI stack |
| translated syscall → `gex_syscall` (may park in the kernel) | the `syscall` import is a `Suspending` that runs `promising(gex_syscall)` |
| a wasm32 program's syscall, a provider's call-out | the same Suspending path. For fork, the image is captured in the `after` hook before the parent resumes (§4). |
| park in `#runLoop` (`gex_park(20)`) | `await promising(gex_park)` |

### Scheduling (`Sched` in single.mjs)

- A parked fiber is a promise held in a global set, or in a per-address
  futex map.
- A wake resolves it:
  - with a microtask while the current 8 ms slice lasts;
  - otherwise it is held for one MessageChannel turn, so the event loop gets
    keyboard, network and paint in between.
- Timeouts use `setTimeout` (`timeouts` in the stats).

### Network

- The fibers share the realm, so they also share the one `WispNetwork`
  (`wispNet` in `extra`). No net-bridge is needed.
- Each stream event calls `onActivity`, which wakes the parked fibers.
  `NodeNetwork` (the node harness) does the same.
- A process waiting on the network **parks**: a bounded `gex_park`, i.e. a
  promise.
  - The SAB build instead takes a MessageChannel turn per loop, because its
    bytes arrive on the Worker's own event loop.
  - Copied into the single build, that turn spun the one thread. An exec'd
    `node` child installing about 30k translated modules got a turn only
    between thirteen spinning node threads. `child_process.spawn` then took
    minutes, and the page run hit its 900 s cap.

### Terminal

- `PageTty` sits on the kernel memory as in the SAB build, because the
  kernel is in this realm.
- The page forwards keys and resizes to the system Worker. Every input,
  resize and signal pokes the scheduler.

### Compositor

- `compositor-worker.js` runs in the system Worker's realm via `inRealm`,
  because only that realm can read the guests' memories.
- The page keeps the canvas listeners. It transfers the canvas
  (OffscreenCanvas) once, then talks through `comp` / `comp-out` messages.
- The scene arena is a plain ArrayBuffer.
- The compositor draws on `requestAnimationFrame` in the Worker, between
  fiber slices.

## 3. Preemption and yield points

A fiber runs until it suspends. To keep one guest from starving the others
and the page, the single build yields at two kinds of point:

- **Safepoints.** Translated code already calls `gex.safepoint` every 1024
  loop back edges (`SAFEPOINT_INTERVAL`, shims/browser/src/lib.rs). gexcc
  code polls (`__gex_poll`) every 65536 iterations.
  - In the single build the import is a `Suspending`, which costs about
    174 ns when it does not suspend, against 17.5 ns for a plain import.
  - Amortised over 1024 back edges, that is about 0.17 ns per back edge.
  - It yields (`sched.maybeYield()`) only once the 8 ms slice has run out.
- **System calls.** wasm32 programs and providers yield after each syscall
  once the slice has run out.
  - Each such syscall goes through the Suspending path.
  - That adds about 1.2 µs (a `promising` entry at about 1 µs plus a
    suspend at about 170 ns) on top of what the syscall costs.

No new poll was added to generated code. The microbenchmark prices one:

- a per-back-edge budget counter with a refill of 65536 costs 0.26 ns per
  iteration on an empty loop (2.32 against 2.06 ns);
- a flag-word load costs 1.8 ns (3.82 ns).

Both are measurable on tight loops. The existing 1024-edge safepoint is the
better place to yield.

## 4. Fork

- A **translated** (arm64/x86) process forks as in the SAB build. The child
  is a new fiber with a copied memory.
- A **wasm32** program forks with asyncify, as on the SAB build. The stack is
  unwound to the syscall wrapper, the image is captured, and both parent and
  child rewind. Under the single build, the image capture runs in the
  Suspending wrapper's `after` hook (a `.then`), before the outer fiber
  resumes, so the parent cannot run ahead of its own snapshot.
- No musl patches were added.

## 5. SAB and Worker uses, and their single-build paths

| where | what (SAB build) | single build |
|---|---|---|
| shims/browser/js/gex.mjs | shared process memory, `Atomics.wait` in kernel imports | plain memory; Suspending kernel imports |
| gex.mjs module store | growable SAB | `null` (falls back to the per-process registry, as already designed) |
| worker.mjs | a Worker per guest thread; `outputAck` SAB | `FiberWorker`; no ack (one queue keeps the order) |
| host.mjs `#outputAck` | SAB + `Atomics.wait` | off |
| net-bridge.mjs | SAB ring to the page's Wisp | not used: `wispNet` shared by reference |
| tty.mjs | shared scratch memory | plain (kernel is in-realm) |
| wl-bridge.mjs | SAB scene arena | plain ArrayBuffer |
| compositor.js / compositor-worker.js | Worker reading SAB memories, stats SAB | in-realm in the system Worker; stats mirrored to the page every 500 ms |
| runtime.js kernel | shared kernel memory | plain kernel memory in the system Worker |
| gl-bridge.mjs, mesa-gl.js | SAB ring + `Atomics.wait`; Mesa is an Emscripten pthread build | **not yet**: needs a non-pthread Mesa build in the system realm, with `gl_submit` as a Suspending import |
| xwayland-worker.js / hostx.js | Xwayland pthread build, own Worker on kernel SAB | **not yet**: same, needs a non-pthread JSPI Xwayland build |
| dom-compositor.js | page reads SAB memories | **not yet** (the canvas compositor works) |
| persist.js, nt-page.js | persist Worker on the kernel SAB; SAB bundle | NT page only, which is not part of this build (the NT layer is legacy) |

The default build is selected exactly as before. Every single-build path
sits behind `single.on`, and `single.on` is true only when the realm runs
`enableSingle()`:

- in the system Worker;
- or under node with `GEX_SINGLE=1`.

The page picks the single build when either holds:

- `?single=1` is given;
- the page has no SAB and the engine has JSPI.

`?single=0` refuses it.

## 6. Results

### Correctness (gooner, untimed)

**Node harness** (`GEX_SINGLE=1`, `SharedArrayBuffer` deleted;
`scratch/SP/nrun.sh`):

- bash, pipes, command substitution, fork, `trap`/`kill` USR1/TERM,
  `sleep & wait`;
- python3 with 4 threads;
- `apt-get update` / `install` (real TCP, `GEX_NETWORK=1`);
- `rg`;
- nodejs 20.19.2: `node -e "console.log(1+1)"` prints 2;
- neovim 0.10.4:
  - headless edit and write;
  - `vim.fn.system({"echo","hi"})`, which forks and execs from inside nvim.

**Chrome page without COOP/COEP** (`GEX_NO_COI=1`):

- `apt-get update` over `wss://wisp.mercurywork.shop/`;
- `apt install ripgrep tcc libc6-dev`, then `tcc -run` and `rg`;
- foot in the compositor (`?gui=1`):
  - it draws;
  - typed input reaches it (`seq 40 42`).

  The same foot run on the SAB build still passes, so the default
  compositor is unchanged.

### SAB against single, same commands, edger

Run with `scratch/SP/page/cmp.sh` in headless Chrome 153 under bench.sh.

- The page is the native Debian demo over `wss://wisp.mercurywork.shop/`.
- "single" is served with no COOP/COEP; "SAB" is the same branch served
  cross-origin isolated.
- Runs c1 (before e4779cca and e10ec855) and c2 (after).
- Raw output is in `scratch/SP/page/out/c{1,2}-{single,sab}.txt`.
- All values are ms of wall time as measured in the guest, except where the
  row says s.
- Network-bound rows (apt) swing 2-4× from run to run with the public Wisp
  proxy, in both builds.

| command | single c1 | single c2 | SAB c1 | SAB c2 |
|---|---:|---:|---:|---:|
| `true` | 135 | 124 | 175 | 156 |
| `for i in $(seq 100); do x=$(echo $i); done` (100 forks) | 7216 | 6646 | 10417 | 10104 |
| `yes \| head -c 20000000 \| wc -c` | 2205 | 2072 | 441 | 391 |
| python3, 2M-iteration loop (its own clock, s) | 2.53 | 2.05 | 2.44 | 2.39 |
| `apt-get update` | 26266 | 11361 | 10183 | 39357 |
| `apt-get install ripgrep tcc libc6-dev` | 52342 | 50783 | 47936 | 109482 |
| `rg --version` | 1408 | 1402 | 1541 | 1531 |
| `tcc -run hello.c` | 1474 | 1829 | 1839 | 1679 |
| `rg -c define stdio.h` | 2922 | 3154 | 3221 | 2416 |
| `apt-get install neovim` | 17051 | 16450 | 18653 | 31483 |
| `nvim --version` | 3672 | 4135 | 3345 | 3728 |
| `nvim --headless +q` | 6765 | 7163 | 6755 | 6611 |
| `apt-get install nodejs` | 78290 | 104249 | 141265 | 169786 |
| `node --version` | 3380 | 3235 | 3130 | 3464 |
| `node -e 0` | 59699 | 62812 | 57113 | 56873 |
| node, 3e7-iteration loop (its own clock) | 8427 | 8434 | crashed | 9980 |

What the table shows:

- **Hot code.** It is not slower on a JSPI stack:
  - the python loop takes 2.05-2.53 s single against 2.39-2.44 s SAB;
  - the node loop takes 8427-8434 ms single against 9980 ms SAB.
- **fork and exec are faster.** 100 command substitutions take 6.6-7.2 s
  single against 10.1-10.4 s SAB, about 1.5× faster. A fork in the single
  build makes a fiber, not a Worker.
- **Pipelines are slower.** A three-process pipeline takes 2072-2205 ms
  single against 391-441 ms SAB, about 5× slower. It loses the cores its
  processes ran on, and every byte crosses the kernel on one thread.
  - Profile (node harness, edger, single): `Pipe::read` popped a byte at a
    time (182 ms), and `k_pipe_read` zeroed the whole request on every call
    (`alloc_zeroed`, 240 ms), of 2970 ms in total. Both are fixed in
    4fcc6417, which helps both builds.
  - Scheduling itself is small: 48.9k parks and 29.3k wakes for the 20 MB,
    with `wasm-to-js` at 72 ms.
- **node start-up** (`node -e 0`) takes 59.7-62.8 s single against
  56.9-57.1 s SAB, 5-10% slower. It installs about 30k translated modules
  with an async compile each, on one thread instead of seven.
- **Not single-specific.** node's child_process calls fail in **both**
  builds on the page from the provider bug in §7. In single mode it already
  fires on `node -e "console.log(1+1)"` (rc 137).

**The default build is not slower.** C's head e4779cca (same wasm,
without this branch's JS; run c3, leg 1, cut short) against this branch's
SAB build (c2). Raw ms:

| command | C's head | this branch, SAB |
|---|---:|---:|
| `true` | 139 | 156 |
| pipeline (20 MB) | 407 | 391 |
| `nvim --version` | 3921 | 3728 |
| `nvim --headless +q` | 6873 | 6611 |
| `node --version` | 2760 | 3464 |
| `node -e "console.log(1+1)"` | 38949 | 40063 |
| `node -e 0` | 57034 | 56873 |
| node 3e7 loop (guest clock) | 10216 | 9980 |
| `node t.js` | 80718 | 81141 |

Node harness, edger (whole harness run, ms; `scratch/SP/prof`):

| command | SAB | single |
|---|---:|---:|
| `yes \| head -c 20000000 \| wc -c` | 1562, 1653 | 2453, 2377 |
| 100 command substitutions | 7495, 7538 | 5204, 4927 |

## 7. Limitations and next steps

- **Provider bug (C), both builds, page only.** In node, the panic goes:
  - "provider address 0x71dc1a1 keeps losing its bridge";
  - then `gex-emit/src/module.rs:661` "text base must be
    instruction-aligned".

  In the single build the dead fiber held the process's lock, so its
  siblings then trap in `memory.atomic.wait32` ("Atomics.wait cannot be
  called in this context"). In the SAB build they hang instead.

- **Blocking inside a nested JS frame cannot suspend.** This covers a
  call-out (`#nest`: `call_out`, `nt_callback`, `exc_dispatch`,
  `provider_try`), i.e. a guest callback called from JS called from wasm.
  The single build throws an explicit error there rather than hang. A fix
  makes those JS loops async. None of the runs above hit it.
- **Provider atomic waits on contention.** These are the `crt_heap` /
  `glibc_malloc` `heap_lock_slow` and `gp_wait` waits, and the libprov
  `wait_epoch`. They would trap (`memory.atomic.wait32` on an unshared
  memory) if they ever wait. In the single build only one fiber runs at a
  time, so a lock is never contended while its holder is suspended inside
  it, except across a suspension. This is not seen so far.
- **GL and the X server need non-pthread builds.** Mesa and Xwayland would
  then run in the system realm, with JSPI where they block.
- **Wakes are wake-all.** Every parked fiber re-checks its word. This is
  fine for tens of fibers; a node run peaked at 272.
- **The kernel has 128 stack slots**, which caps the number of fibers alive
  at once.
- Dead fibers' memories are freed by GC.

## 8. State and resume

- **Head:** see `git log merge/singleproc`.
- **Commits:**
  - `21652fb1`: shim glue;
  - `0f9f926d`: page system Worker;
  - `703db0e7`: compositor;
  - plus this doc.
- **Builds:** `shims/browser/build.sh` (plain and shared), the kernel and
  the seed, then `cd web && npm run build`. web/public has rootfs, cgen and
  xwayland hard-linked from wt/gexcc.
- **Tests:**
  - node: `GEX_SINGLE=1 [GEX_NETWORK=1] scratch/SP/nrun.sh 'cmd'`;
  - page: `scratch/SP/page/t1.sh` (apt and tools);
  - page: `scratch/SP/page/gui.sh DIR single|sab` (foot);
  - edger: `scratch/SP/page/cmp.sh TAG single sab` under bench.sh (the
    SAB-vs-single table).
- **Microbenchmarks:** `scratch/SP/mb/run.mjs OUT.json main|worker
  [--js-flags]` under bench.sh.
