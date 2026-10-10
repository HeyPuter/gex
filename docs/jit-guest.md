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

## 2. State / resume

- Done: knobs and counters above (uncommitted until the first commit below).
- Running: baseline `t1.sh base t1 t1prof fjit` on edger.
- Next: profile warm `node -e 0` (stream C: warm 30.2 s tier 1 vs 28.8 s
  fjit, 0.2 s translating, `--jitless` 26.2-26.9 s): the cost is the speed
  of translated V8 builtins and interpreter, not translation volume.
