# Wine overhead on the W3 page (stream K)

What Wine's own layers cost Noita on the W3 page per world frame, measured
against pewasm, and the fast paths that remove that cost, hottest first.
Stream W owns the runtime side (main-thread scheduling, parking, unit code
quality, JS paths, msvcr120 math, `Context::Yield`). This document covers the
Wine boundary: NT system calls, Unix calls and server calls.

## 1. Setup

- **Tree.** `wt/census-k`, branch census/k: klock-9 7ee38ef2, plus land/wskern
  (wineserver in the kernel, door RPC), plus gate/noita-leg 34a4e2db. It uses
  the x86 shims and `target/x86/release/gexc`.
- **Wine.** dist-k is wskern/wine 6f179222fbe, linked by
  `scratch/K/bin/link-loader.sh`. That is the base-d loader, so image
  placement differs from W's base-m. The units are rebuilt for it, so no
  image runs on tier 1.
- **Counting build.** dist-census is dist-k plus `WINEGEX_CALLSTATS`, which
  is not committed (`wt/census-wine`). It records:
  - every PE system call (`__wine_gex_syscall_body`) per (table, id);
  - every Unix call (`__wine_unix_call_dispatcher`) per (library, code);
  - kernel callbacks;
  - a per-thread table: syscalls, Unix calls, yields, sleeps, waits;
  - object waits per handle, with the handle's type and the thread waiting.

  Counts are taken on every call. One call in eight is timed, using
  `clock_gettime`. Waits are timed on every call. Server requests are counted
  by `WINEGEX_REQSTATS`.
- **Runs.** One `bench.sh` call per run, on edger: `tests/games/noita-page-leg.sh`
  with 300 s of world. The tool scripts are in `scratch/K/bin`
  (`noita-census.sh`, `pewasm-census.sh`, `cs-sum.py`). Outputs are in
  `scratch/K/census/`.

## 2. World frame rates (edger, timed)

| run | world fps |
|---|---|
| census-k, doors (n2, n3) | 20.6, 19.3 |
| census-k, pipes, `WINEGEX_DOOR=0` (n3) | 15.9 |
| census-k with the counting build (n3, n4, n5) | 19.6, 17.7, 18.9 |
| pewasm (p1) | 48.1 |

- Doors are 1.25× faster than pipes on the same loader (19.3 vs 15.9 world
  fps, n3 pair).
- The W3 page with doors is 2.4× slower than pewasm (20.6 vs 48.1 world fps).
- The counting build costs about 5%: 18.9 vs 19.3-20.6 world fps.

## 3. Census per world frame (n4, game process, 17.7 world fps)

### 3.1 NT system calls: 990 per frame

| call | per frame | µs/call (incl.) | ms/frame |
|---|---|---|---|
| NtQueryPerformanceCounter | 795 | 2.68 | 2.13 |
| NtDuplicateObject | 64 | 18.4 | 1.17 |
| NtClose | 64 | 15.4 | 0.99 |
| NtDelayExecution (worker Sleep) | 47 | 3190 (asleep) | (off main) |
| NtWaitForSingleObject (workers) | 2.7 | 145000 (idle) | (off main) |
| NtReadFile | 8.2 | 30.5 | 0.25 |
| NtReleaseSemaphore | 2.7 | 84 | 0.23 |

"Incl." means the time includes one `clock_gettime` of the measurement. That
inflates the cheap calls: QPC is itself a clock read plus one W3 system-call
round trip.

### 3.2 Unix calls: 4591 per frame, all opengl32

Without the swap, 4580 calls/frame at 0.75 µs inclusive, about 3.8 ms/frame.
The swap (`wglSwapBuffers`, code 8) costs 9.1 ms/frame, but that is the GL
present (W's gl-bridge), not Wine.

| code | function | per frame |
|---|---|---|
| 72 | glDisable | 772 |
| 350 | glActiveTexture | 736 |
| 2645 | glUseProgram | 712 |
| 81 | glEnable | 478 |
| 14 | glBindTexture | 477 |
| 2542 | glUniform2f | 362 |
| 2524 | glUniform1i | 368 |
| 82 | glEnableClientState | 158 |
| 73 | glDisableClientState | 158 |
| 74 | glDrawArrays | 53 |

### 3.3 Server requests: 135 per frame

| request | per frame | µs in the server |
|---|---|---|
| dup_handle | 64 | 4.9 |
| close_handle | 64 | 3.1 |
| release_semaphore | 2.7 | 54 |
| select | 3.7 | 38 |

Kernel callbacks: 0 per frame.

### 3.4 Per thread (n5, 10 s window)

| tid | role | per second |
|---|---|---|
| 0024 | main (render, game loop) | 5.4k syscalls, 89k Unix calls; 17 File waits, 39 µs |
| 00b0, 00b4 | workers | 4.3k and 3.6k syscalls (QPC), no waits |
| 00c4 | pool | 47 Semaphore waits on h 014c, 20 ms each (idle) |
| 00c0 | Sleep(1) | 628 sleeps, 1.15 ms each |
| 00bc, 00d4 | Sleep(10) | 93 and 98 sleeps, 10 ms each |

The main thread has almost no NT waits. Where it parks in W's profile
(`k_ws_park` under `f_78a2f6a0`), it is inside msvcp120's `_Thrd_current`.
Wine's `thread_current()` makes a DuplicateHandle(GetCurrentThread) and a
CloseHandle each time, and each one is a door call into the wineserver.

## 4. Against pewasm

pewasm (p1-prof, 200 s, 44.4 world fps while profiling) spends its time as
follows:

| share | where |
|---|---|
| 40.3% | lifted guest code (`sub_*`) |
| 44.0% | `(program)` |
| 14.7% | JS and WebGPU |

It makes no system calls and no Unix calls. Its runtime calls straight into
Mesa. Its clock (`now` plus `_emscripten_get_now`) is 6.3% of all samples,
so the game's QPC habit is costly there too.

The Wine boundary costs the W3 page about 7.5 ms of the main thread per
world frame, measured inclusively in §3:

| item | ms/frame |
|---|---|
| QPC | 2.1 |
| dup/close | 2.2 |
| GL Unix calls (thunk, dispatcher, Unix wrapper, GL command) | about 3.4 |

At 18.9 world fps a frame is 53 ms, so this is about 14% of the frame.
Removing all of it would give about 22 world fps, still about 2.2× slower
than pewasm's 48.1. Most of the gap is guest code and the GL path, which are
W's runtime side. The Wine side is still worth taking because it is cheap to
remove and the gains add up.

## 5. Fast paths, hottest first

Each fast path has a switch back to the slow path. Each gets a leg that runs
both paths: correctness on gooner without the game (tests/w3), plus a timed
pair of Noita runs on edger. Each round reports "N× slower than pewasm
(a vs b world fps)".

### F1. `_Thrd_current` without the two door calls (msvcp120)

gex already has the provider, `crates/gex-cgen/providers/msvcp120_thrd.c`.
It returns EAX=0 and EDX=TEB+0x24 (the thread id). It is marked `nowine`
because it was written for MSVC's own msvcp120.

Wine's `_Thrd_current` (dlls/msvcp90/misc.c) has the same i386 ABI: a
cdecl `ULONGLONG`, the `_Thrd_t {hnd, id}` in EDX:EAX. It makes the same
duplicate-then-close pair and returns a handle that is already closed. So
dropping `nowine` is behaviour-preserving: only the id is usable, and when
DuplicateHandle fails, both versions return handle 0.

- **Saves:** 128 NT calls and 128 server requests per frame (2.2 ms/frame
  inclusive), plus the main-thread parking W sees under `f_78a2f6a0`.
- **Cost:** the msvcp120 unit key gains `-p<hash>`, so Wine's msvcp120 units
  must be rebuilt.
- **Slow path:** `GEX_PROVIDER_SKIP=_Thrd_current`. The page has no
  environment, so this round adds a page parameter, `provskip=<export,...>`.
  It reaches `gex_cgen::provider` through a static the shim sets, in place
  of `std::env` in the browser. The skip changes the unit key, so the skip
  leg has its own units.

### F2. QPC without a system call (Wine's wasm32 PE ntdll)

ntdll, kernel32, kernelbase, msvcrt, ucrtbase, user32, gdi32, win32u and
opengl32 are native wasm32 on the W3 page (`winepe.wasm`). So the cgen
`nt_qpc` provider never applies. Its `gex.nt_qpc` also reads the NT layer's
process clock, which W3 does not have, so binding it there would return 0.

The slow chain is:
`QueryPerformanceCounter` → `ntdll.RtlQueryPerformanceCounter` (wasm) →
`NtQueryPerformanceCounter` stub → `gex.syscall` → `__wine_gex_syscall_body`
→ `NtQueryPerformanceCounter` (unix) → `clock_gettime(CLOCK_BOOTTIME)` →
the shim's syscall.

The fast path is a new import in winepe.wasm, `gex.clock_ns(clockid) -> i64`.
It is the kernel clock that the shim's `clock_gettime` returns, so the
values are the ones the slow path gives:
- browser: `shims/browser/src`, beside `gex_nt_qpc`;
- native: `shims/native/src/libprov.rs`.

With it, PE ntdll's wasm32 `NtQueryPerformanceCounter` and
`RtlQueryPerformanceCounter` compute `CLOCK_BOOTTIME / 100` and the
frequency themselves. This covers direct `NtQueryPerformanceCounter`
callers as well.

- **Saves:** 795 W3 system calls per frame on the main thread (about 2 ms
  inclusive), plus the workers' 8k/s.
- **Slow path:** none any more. `WINEGEX_FASTQPC=0` served the timed
  pair and was then removed (wineperf/wine 81f620b5962). One call in
  `QPC_SAFEPOINT` still makes the system call, so a thread spinning on QPC
  still reaches a signal safepoint (f23cb427f50).
- **Correctness leg:** tests/w3/qpc.sh, native and page: QPC and NtQPC
  called alternately, monotonic, agreeing with each other and, across
  threads, with `clock_gettime`.

### F3. GL calls (opengl32, native wasm32): measure before cutting

The Wine part of a GL call is small:
- the PE thunk (`map_context_objects`, the params struct);
- `__wine_unix_call_dispatcher`, already a direct `call_indirect` into the
  unixlib table that saves and restores 9 context words;
- the Unix wrapper.

The rest of the 0.75 µs is the GL command itself and the i386 → wasm
adapter. The census timing (with its `clock_gettime`) cannot split these. A
CPU profile with this layout (n1-prof) gives the split. A fast path is
taken only if the Wine part is at least about 1 ms/frame. The candidates
are:

- **(a)** skip the 9-word context save/restore when the call comes from
  native PE code that cannot clobber it. The dispatcher's comment says Unix
  syscalls use EBX..EBP, so this is only safe for libraries whose Unix side
  makes no system calls; opengl32's hot calls write the GL ring;
- **(b)** let the PE thunk call `unix_funcs[code]` directly, bypassing the
  dispatcher, for opengl32's hot codes.

Batching (W's gl ring) is W's.

**Not taken this round.** Nothing was profiled, so neither (a) nor (b)
went in. The swap's 9.1 ms per frame is W's async present (327c4f66), not
the Wine boundary.

### F4. Not taken (owner noted)

- Worker sleeps and semaphores: they are off the main thread, and the pool
  is idle about 93% of its time. No action.
- `NtReadFile` (8/frame, 0.25 ms): main-thread File reads, left alone.
- msvcr120 x87 math and `Context::Yield`: W's.

## 6. Order and reporting

1. **F1:** drop `nowine`, add the `provskip` page parameter, rebuild Wine's
   msvcp120 units. Legs: tests/w3 threadexit and session (thread ids),
   then a timed Noita pair, provskip vs default.
2. **F2:** add the `gex.clock_ns` import to both shims, change the Wine
   side, relink dist. Legs: tests/w3 qpc native and page, then a timed
   Noita pair, `WINEGEX_FASTQPC=0` vs default.
3. **F3:** profile first. Implement (a) or (b) only if the Wine share is at
   least 1 ms/frame.

Each round ends with the timed Noita world fps against pewasm's 48.1.

## 7. Results

### 7.1 Per call (wbench, tests/w3/thrd.c)

Microbenchmarks, the same tree with the fast and the slow path:

| Path | node (fast vs slow) | Chrome (fast vs slow) |
|---|---|---|
| F1 `_Thrd_current`, provider vs `provskip` | 66 vs 6029 ns: 91× faster | 72 vs 13211 ns: 183× faster |
| F2 QPC, fast vs `WINEGEX_FASTQPC=0` | 305 vs 726 ns: 2.4× faster | 757 vs 1274 ns: 1.7× faster |

The floor (`GetCurrentThreadId`, no call) is about 100 ns in Chrome. Of
the 757 ns of a fast QPC, about 650 ns was `gex.clock_ns` reading the host
clock as a BigInt (the `now` import). The shim's clock now reads
`performance.now()` as an f64 plus a per-Worker origin
(`host_now_ns`). The values are the same as before (`hostNowNs`).

### 7.2 In game (census, Noita on the page, per world frame)

| | before | after |
|---|---|---|
| QPC system calls | 794.7 | 44.8 |
| NtDuplicateObject | 63.9 | 0 |
| NtClose | 64.0 | 0.2 |

### 7.3 Timed pairs (edger, pinned)

Noita on the page, edger, pinned (bench.sh's central pinning since 12:47),
300 s of world frames per run:

| Leg | Tree, Wine | World fps per run | Mean |
|---|---|---|---|
| master | master b26b02f0, land/wskern's Wine | 20.7, 22.4 | 21.55 |
| on | wineperf/m, dist-f (F1, F2, nosig, fixed bases) | 20.0, 22.8 | 21.4 |
| off | wineperf/m, dist-kb, `provskip=_Thrd_current` | 20.6, 19.2 | 19.9 |

- **On against master:** 0.7% slower (21.4 vs 21.55 world fps). That is
  inside the in-run spread (8%, 21.5 vs 23.2 for one config), so there is
  no regression and no measurable gain.
- **On against off:** 7.5% faster (21.4 vs 19.9 world fps), also inside
  the spread.
- **Against pewasm:** 2.25× slower (21.4 vs 48.1 world fps).

The fast paths remove about 4 ms of Wine-boundary work per world frame on
the main thread (§5), but world fps does not move. The frame is bound
elsewhere: the main-thread scheduling and the swap are W's.

Two provskip runs (nr-off-1 and nr-off-2) never reached the world: the
skip changes msvcp120's unit key, and that unit had not been built. Their
msvcp120 ran tier-1. Those runs are left out.

## 8. DLL placement: fixed bases for Wine's builtins

The server's `assign_map_address` (server/mapping.c) placed DYNAMIC_BASE
images at system-wide addresses: top-down in [0x60000000, 0x7c000000), in
the order the processes first mapped them. Every Wine builtin is
DYNAMIC_BASE (ImageBase 0x10000000, characteristics 0x140), so its base
changed from run to run. Function-tier units are keyed by image and base,
so a builtin that moved ran tier-1. When that builtin was msvcp120, Noita
never reached the world.

Wine's builtins now get fixed bases when they are staged:

- **Table.** `tools/wine-wasm/w3-bases.txt` maps each DLL to a 64 KiB
  aligned slot. Slots are packed down from 0x7b000000, and the band
  0x60000000-0x70000000 is skipped: GNU ld's auto-image-base puts MinGW game
  DLLs there (SDL2 at 0x6c740000). The table is append-only. A new DLL takes
  the highest free gap, and an existing DLL keeps its slot unless it outgrows
  it.
- **Rebase.** `rebase.py apply` (called at the end of `stage-native.sh`)
  rewrites ImageBase, applies the HIGHLOW relocations, and clears
  DYNAMIC_BASE, so the loader maps the image at its slot. The native wasm32
  skeletons (`.wasmref`) are rebased too.
- **Checks.** `rebase.py check` fails on an overlap, a DLL missing from the
  table, a slot the DLL outgrew, or a DLL without relocations. After `apply`
  it also checks that each DLL is at its base with DYNAMIC_BASE cleared.
  `check --fixed IMAGE...` reports a program's fixed-base images that
  overlap a slot; the games corpus has none. `tests/w3/bases.sh` runs the
  check, then bases.exe (native and page), which loads nine builtins and
  compares their bases with the table.
- **Server pool.** Under `__wasm32__` the pool is 0x60000000-0x70000000 plus
  0x7b000000-0x7c000000, clear of the slots. A game's own DYNAMIC_BASE DLLs
  land there (Noita: fmod 0x7bdc0000, lua51 0x7bf90000) and keep the
  server's first-mapped order. Base-independent units for those are W's
  (design A), not this table's.

