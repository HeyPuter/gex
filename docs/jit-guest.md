# Stream T: translating code that guest programs generate at run time

Branch `merge/jitguest` (worktree `wt/jitguest`), off master 0bcce81c and
fast-forwarded to stream C's c47bbff1 (kernel fixes node needs in the page:
`F_GETFL`, fork-safe socketpairs, socket kinds).

Problem (coordinator, 2026-10-10): translating code a guest generates at run
time (V8, Mono's JIT) is the biggest cost in three places: node in the
native-debian page (`node -e 0` about 30 s), Muck's menu in the page, and
Hollow Knight on the Mono interpreter (about 3.5 MiB/s translated).

## 1. Measuring

### Tools

- `GEX_LIFT_MAX=N` (run-node) / `?liftmax=N` (page): the most blocks one
  dispatch miss lifts. The default is 2048: discovery follows successors, call
  returns and call targets folded through memory, breadth-first. With 1 only
  the block that missed is lifted, so the blocks lifted are the blocks run.
- `xlate[...]` on each process's exit line (run-node `GEX_TIME_JIT=1`, and the
  page's console line): guest instructions lifted, wasm KiB emitted,
  translations (units), SMC page invalidations, pages retired by
  munmap/mprotect, retire calls, and blocks lifted from anonymous memory (a
  guest JIT's code).
- `scratch/T/t1.sh TAG MODE...` (edger, under bench.sh): node start-up under
  `run-node.mjs` (the browser runtime on Node 26's V8), modes `t1`, `fjit`,
  `t1prof`, `fjitprof` (`--cpu-prof` of every Worker), summarised by
  `scratch/T/prof.mjs`.
- Rootfs: `scratch/T/ndnode.tar` = native-debian.tar plus Debian's nodejs and
  its libraries (stream C's `scratch/CC/demo/noderoot`), so no apt run is
  needed.

### Counts (gooner, run-node, `exec node -e 0`, one fresh runtime; counts only)

| | blocks lifted | guest insns | wasm emitted | units (modules) |
|---|---|---|---|---|
| default (2048 per miss) | 246,955 | 1,102,158 | 158.5 MiB | 7,414 |
| `GEX_LIFT_MAX=1` | 90,375 | 453,161 | 160.6 MiB | 90,375 |

- libnode.so.115 is 201,491 of the 246,955 blocks; node's own JIT code
  (anonymous memory) is 1,721. Code generated at run time is under 1% of the
  blocks node translates at start-up.
- About 90k distinct blocks run; the default lifts 2.7 times that (63% of the
  lifted blocks never run).
- About 640 bytes of wasm per lifted block (about 144 per guest instruction).
- SMC: 9 page invalidations; 13 pages retired by munmap/mprotect. Node's
  V8 maps its code space RWX and does not flip protections.

### Baseline under run-node on edger (b-base, 2026-10-10, before any change)

`node --version`, then `node -e 0` twice, in one fresh runtime (the first
`node -e 0` is cold: nothing translated yet; the second is warm: it is served
the first one's modules from the kernel's store).

| | `node --version` | `node -e 0` cold | `node -e 0` warm |
|---|---|---|---|
| tier 1 | 3.21 s | 32.29 s | 20.76 s |
| `GEX_FJIT=1` | 2.72 s | 32.33 s | 20.33 s |

Cold: 10.1 s in the shim's translator (224k blocks), 2.1 s compiling.
Warm: 0.13 s translating, but 8,855 modules served, 2.1 s compiling them
again and each of the process's six thread Workers instantiating all of them.

Profile of the cold process's main Worker (39.8 s sampled, `--cpu-prof`):

| where | ms | share |
|---|---|---|
| guest code (shows as `js-to-wasm`: tail calls hide the frames) | 11,690 | 29% |
| `gex_dispatch_miss` (inclusive) | 18,111 | 46% |
| - `translate_runtime` | 8,366 | 21% |
| -- emission (`ModuleBuilder::finish`) | 6,038 | 15% |
| --- `region::grow` (twice per block) | 2,029 | 5% |
| -- discovery and lifting | 2,059 | 5% |
| - installing (`jit_install`: compile, instantiate, hash) | 4,298 | 11% |
| - kernel module store (`memory.grow` of the shared kernel memory) | 2,099 | 5% |
| - page table clones and drops | about 1,000 | 3% |
| waiting on a futex | 2,269 | 6% |

Warm process's main Worker (26.4 s sampled): guest code 11.0 s (42%),
installing served modules 4.0 s (15%), futex waits 2.7 s (10%), syscalls
0.9 s.

So for node the code V8 generates is not the cost; translating and installing
libnode (V8 itself, its builtins included) is, and then the speed of the
translated V8.

### Changes under run-node on edger (b4, 2026-10-10, branch 64b129ce)

Same commands as the baseline. One run per mode, in order; `new` and `new2`
are the same mode at the start and end of the run.

| mode | `node --version` | `node -e 0` cold | `node -e 0` warm | cold wasm |
|---|---|---|---|---|
| baseline (b-base, before any change) | 3.21 s | 32.29 s | 20.76 s | 158.5 MiB |
| new (all changes, defaults) | 2.23 s | 19.27 s | 13.61 s | 68.5 MiB |
| new2 (same, last in the run) | 2.32 s | 18.76 s | 12.90 s | 68.5 MiB |
| `GEX_INLINE_DISPATCH=1` (old dispatch form) | 2.61 s | 24.58 s | 19.62 s | 140.1 MiB |
| `GEX_LIFT_MAX=64` | 2.14 s | 19.17 s | 14.68 s | 53.6 MiB |
| `GEX_DISCOVER_SKIP=1` (no call-return discovery) | 3.45 s | 34.41 s | 28.64 s | 68.3 MiB |
| V8 `--liftoff-only` | 3.35 s | 24.09 s | 14.54 s | 68.5 MiB |
| V8 `--no-wasm-dynamic-tiering` (TurboFan up front) | 2.19 s | 16.81 s | 11.87 s | 68.5 MiB |

- Cold `node -e 0`: 40% faster than the baseline (19.27 s against 32.29 s);
  warm 34% faster (13.61 s against 20.76 s).
- Outlined dispatch alone: cold 22% faster than inline (19.27 s against
  24.58 s), warm 31% faster (13.61 s against 19.62 s).
- Not lifting call returns ahead of time costs far more than it saves: 4.5
  times the modules (39,213 against 8,680 installed) and cold 79% slower
  (34.41 s against 19.27 s).
- A lift cap of 64 changes cold little (19.17 s) and makes warm 8% slower
  (14.68 s against 13.61 s): more, smaller modules to install.
- V8's tiering matters: compiling with TurboFan from the start is 13% faster
  cold (16.81 s against 19.27 s) and 13% faster warm (11.87 s against
  13.61 s); Liftoff only is 25% slower cold. The page cannot set V8 flags, so
  this only says the translated code's optimised speed is what counts.

### Discovery overshoot (gooner, run-node, `exec node -e 0`, counts only)

`GEX_DISCOVER_SKIP=256 GEX_DL_FILE=PATH` logs why and at which breadth-first
depth each block was lifted. The blocks that run are the ones a
`GEX_LIFT_MAX=1` run lifts (90,277).

| reason | lifted | of them run | run share |
|---|---|---|---|
| seed (the block that missed) | 7,414 | 7,373 | 99.4% |
| conditional branch, taken side | 69,363 | 18,388 | 26.5% |
| conditional branch, fall-through | 86,169 | 27,519 | 31.9% |
| sole successor (goto, direct call, syscall return) | 22,455 | 10,501 | 46.8% |
| call return address | 61,554 | 25,489 | 41.4% |
| all | 246,955 | 89,270 | 36.1% |

The run share falls with depth: 78.7% at depth 1, 50.2% at 4, 34.2% at 8,
22.0% at 19; depth 20 and beyond is 88,927 blocks at 25.9%. No single edge
kind is the waste. Cutting discovery short trades blocks for units:

| discovery | blocks lifted | units |
|---|---|---|
| default | 246,955 | 7,414 |
| depth at most 16 | 194,737 | 9,482 |
| depth at most 8 | 157,886 | 13,027 |
| depth at most 4 | 131,430 | 20,312 |
| `GEX_LIFT_MAX=64` | 161,028 | 8,946 (11,678 installed) |

`GEX_LIFT_MAX=64` was no faster cold on edger (19.17 s against 19.27 s) and
8% slower warm, and skipping call returns was 79% slower cold: a unit costs
about as much as the blocks it saves. Depth limits 8 and 16 are queued (b5).

### Per-unit fixed cost (7,414 units of node's start-up)

| part | bytes a unit | share of the wasm |
|---|---|---|
| imports (21, all of them in every unit) | 351 | 3.6% |
| `gex.reloc` (removed for dense units: 11b1b601) | 340 | 3.5% |
| `enter` | 285 | 2.9% |
| `gex.dense` | 153 | 1.6% |
| types, elem, func, export, `gex.meta` | 227 | 2.3% |
| miss stub | 53 | 0.5% |

Code is 88.9%. With `gex.reloc` gone (77,397 -> 74,938 KiB for `exec node -e 0`),
about 7% of the bytes are still fixed per unit. Since f469c15a, `enter` is
split: a store-free `dispatch` that blocks tail-call, plus a small `enter`
(75,050 KiB). Importing the seed's `enter` instead was tried (34cf5db9) and
reverted (4ca3b163): the seed's `enter` stores `ctx::PC`, which blocks must not
do (see f469c15a). Dropping unused imports would save most of the 3.6% they
take. Neither is likely to move start-up by more than a few percent, because
compiling is about 1.5 s of a 19 s cold start.

### Node in the native-debian page (p1, edger, 2026-10-10)

`apt-get install nodejs`, then `node --version` and `node -e 0` three times,
in the page (`web/test/site.mjs native-debian-run`).

| leg | `node --version` | `node -e 0` #1 (cold) | #2 | #3 |
|---|---|---|---|---|
| base (c47bbff1) | 3.48 s | 38.87 s | 27.79 s | 26.14 s |
| base again | 3.85 s | 38.73 s | 27.55 s | 26.00 s |
| branch at 64b129ce | 2.63 s | crashed | - | - |
| branch at 64b129ce, `unitcache=1` | 2.79 s | 23.15 s | crashed | - |

- With `unitcache=1`, the branch started node cold 40% faster than base
  (23.15 s against 38.80 s, the mean of the two base legs).
- The plain branch leg crashed before it finished a cold start, so p1 has no
  number for the unit cache on its own, and no warm number at all.
- The crash was the outlined `enter`, fixed in f469c15a. On gooner the page
  run reproduced it in 3 of 6 runs before the fix and 0 of 4 after (inline
  dispatch: 0 of 3).
- The branch has not been timed in the page since the fix.

## 2. Final state

Stream T ends here (coordinator, 2026-10-10).

**On by default:**

- Dispatch through the module's `dispatch` instead of inline (43fa9fc5,
  f469c15a). Under run-node on edger, node's cold start went from 24.58 s
  inline to 19.27 s, and warm from 19.62 s to 13.61 s (b4).
- Region grown at back-edge targets only; no page-table clones per miss; the
  slab module store; word-wise content keys; fewer allocations in tier-1
  emission. Together with the dispatch change: cold `node -e 0` from 32.29 s to
  19.27 s, and warm from 20.76 s to 13.61 s (b4).
- No `gex.reloc` in dense units.
- The dense base comes from an imported global (`gex.dense_base`), so a unit's
  bytes are the same in every process.

- The page-wide unit cache (merge/unitcache-default): on in the page and
  under run-node; `?unitcache=0` / `GEX_UNIT_CACHE=0` turn it off. A second
  `node -e 0` in one page: 15.8 s against 30.9 s without it (edger, traced);
  95,188 lazy wasm compiles become 2,365. A module taken from the cache keeps
  this process's dense base with it (`#compiled[].dense`), so a fork child
  never takes the base from another process's compile.

**Off by default (knobs kept, not proven faster in the browser):**

- Tier-1 tee folding: `GEX_T1_FOLD` / `?t1fold=1`. Loop-safe; 8% less wasm.
- Lift cap `?liftmax=` / `GEX_LIFT_MAX`.
- `GEX_DISCOVER_SKIP`: bit 8 is the discovery log (`GEX_DL_FILE`); bits 16-23
  are a depth limit.
- Inline dispatch: `GEX_INLINE_DISPATCH` / `?inlinedispatch=1`.
- The null guard, a debugging opt-in: `GEX_NULL_GUARD=1` / `?nullguard=1`
  (`gex_abi::null_guard`). Guest accesses below 64 KiB fault, as a null
  pointer's do natively, in tier 1 (block and function mode) and the C units.
  Off, their output is byte-identical to a build without it and no key names
  it; on, tier 1 is 2-8% slower (scratch/KC/ab-45.txt, variant G). Test:
  tests/x86-programs/null-access.c, which run.sh runs with it on.
- `?xlatereport=N`.
- The emitter switches are in the native module cache key, with a test
  (`cache_key_tests`).

**Tests at the final head:**

- gex-emit 17, gex-xlate 22 + 3, `cache_key_tests` 1: all pass.
- Integration ladder (bbb7f237, folding on): no failure that the base tree
  does not also have.
- The page reproduction on gooner: 4 runs, no crash.

**Open items:**

- **Discovery overshoot.** 63% of the blocks lifted for node never run.
  Shallower discovery only trades blocks for units (§1).
- **Per-unit overhead.** About 7% of the bytes. Options: imports a unit uses
  only; a store-free `dispatch` shared from the seed.
- **Warm translated-code speed.** Warm `node -e 0` spends 0.1 s translating
  out of 13.6 s. The main Worker's profile: guest code about 42%, compiling
  served modules about 1.4 s (what the unit cache removes), futex waits about
  2 s.
- **Hollow Knight translation volume** on U's .NET provider path is
  unmeasured. `hk1.sh` (`?xlatereport=500`) was cancelled before it ran.
- **A persistent translation cache** across page loads (compiled units
  surviving a reload) is not started.
- **The unit cache's default** needs a page timing at f469c15a or later. One
  thing to check before turning it on: for a cached Module, the dense base has
  to come from this process's bytes, never from the cached Module's custom
  section (catch-up and fork adopt instantiate from `mod`).
