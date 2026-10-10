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
what is left fixed is about 7% of the bytes. The seed module already exports a
dense-capable `enter`, so units could import it (2.9% more), and unused imports
could be dropped (most of the 3.6%). Neither is likely to move start-up by
more than a few percent: compiling is about 1.5 s of a 19 s cold start, and
the page's unit cache removes the repeat compiles.

## 2. State / resume

- Branch head bbb7f237. Commits, in order:
  - 9e84b3cc knobs and per-process counters (`xlate[...]`);
  - 0f5d120b region grown only at back-edge targets, once per block
    (byte-identical output);
  - 3e8dab8e no page-table clones per miss, slab kernel module store,
    word-wise content key;
  - 43fa9fc5 dispatch through the module's `enter` (`GEX_INLINE_DISPATCH=1`
    restores the old form byte for byte);
  - c15680ca translator progress report (`?xlatereport=N`), re-lift and
    anonymous-memory counters, served-module cursor;
  - 6928037b page-wide cache of compiled units (`?unitcache=1`, off by
    default); dense base as an imported global, so a served unit's bytes are
    the same in every process;
  - 64b129ce fewer allocations and no SipHash in tier-1 emission;
  - bbb7f237 tier-1 tee folding (`GEX_T1_FOLD` / `?t1fold=1`, off by default,
    loop-safe), emitter switches in the native module cache key.
- Tests: gate on 64b129ce passed (a64 485, x86 474, nt 85/85 both, glibc,
  lockstep). Integration ladder with `GEX_T1_FOLD=1` on bbb7f237: 100 passed;
  the 50 failures all reproduce on the base tree (c47bbff1): the browser
  comparisons see host.mjs's `console.debug` lines on stdout, vfs segfaults,
  priority sees the run's nice level, park-across-switch, two-workers and
  threads-shared fail identically, browser-builds hit the scope's task limit.
- Queued on edger: `page.sh p1` (base vs new vs new with `unitcache=1`, node
  in the native-debian page).
- Next: decide the unit cache and fold defaults from page numbers; HK
  translation volume with `?xlatereport=500` (needs a coordinator run);
  per-unit fixed overhead (about 830 B a unit); discovery overshoot (63% of
  lifted blocks never run).
