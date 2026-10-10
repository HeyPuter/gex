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

## 2. State / resume

- Done (branch commits, in order): knobs and counters (9e84b3cc); region
  grown only at back-edge targets, once per block (byte-identical output);
  no page-table clones per miss, slab-allocated kernel module store,
  word-wise content key (3e8dab8e); dispatch through the module's `enter`
  (43fa9fc5: node cold 158.5 -> 78.6 MiB of wasm; `GEX_INLINE_DISPATCH=1`
  restores the old form byte for byte).
- Queued on edger: `t2.sh b3` (new vs inline dispatch ABBA, lazy threads,
  lift cap 64, a profile).
- Next: profile warm `node -e 0` (stream C: warm 30.2 s tier 1 vs 28.8 s
  fjit, 0.2 s translating, `--jitless` 26.2-26.9 s): the cost is the speed
  of translated V8 builtins and interpreter, not translation volume.
