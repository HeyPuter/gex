# What costs what

Where a gex run's time actually goes, measured at this commit, plus the
measurement traps that hide it. Every number here was taken with the binaries
in this tree; where a claim came from a probe rather than a real program, the
probe is named and lives beside this file.

Read the last section first if you are about to optimise something.

## Half of a real command is the translator

`GEX_TIME_JIT=1` splits a run into the part spent translating guest code and
everything else. Debian trixie arm64, Node 26, best of three, `--no-shared-memory`:

| program | total | translate + install | everything else | startup share |
|---|---:|---:|---:|---:|
| `bash /gex-loop.sh` | 3630 | 335 | 3295 | 9% |
| `sort -o /dev/null corpus` | 1362 | 215 | 1147 | 16% |
| `perl /gex-bench.pl` | 1798 | 453 | 1345 | 25% |
| `gzip -k -f -9 corpus` | 745 | 232 | 513 | 31% |
| `du -s /usr` | 472 | 203 | 269 | 43% |
| `wc -w corpus` | 425 | 196 | 229 | 46% |
| `openssl dgst -sha512` | 748 | 399 | 349 | 53% |
| `sha256sum corpus x3` | 397 | 221 | 176 | 56% |
| `sed -e s/abc/xyz/g corpus` | 474 | 277 | 197 | 58% |
| `apt-cache --version` | 708 | 476 | 232 | 67% |
| `tar cf /dev/null /usr/bin` | 378 | 269 | 109 | 71% |
| `grep -c abc corpus` | 361 | 258 | 103 | 71% |
| `dpkg --version` | 291 | 210 | 81 | 72% |
| `find /usr/bin -name a*` | 331 | 245 | 86 | 74% |
| `ls -la /usr/bin` | 397 | 294 | 103 | 74% |

Milliseconds. "Everything else" is the total minus translating and installing,
so it includes instantiation and the guest's own work; the two columns sum to
the total exactly.

The median is **56% of the wall clock spent translating**, and these are
generous workloads -- `corpus` is 4 MB and `gex-loop.sh` runs 30,000 iterations
precisely so the commands do not finish before they start. A bare `ls` in a
real shell is worse than the row above, not better.

Reproduce with `bench/startup-matrix.sh [reps] [tree-A] [tree-B]`, which builds
each program with each tree's own `gexc` and runs it under each tree's own
shim, alternating sides so machine drift cancels.

### Which is why `bench/` disagrees with a person using this

Everything under `bench/` runs for seconds. `bench/suite.sh`, `bench/ab.mjs`
and `bench/nbench-ab.sh` all report the minimum CPU time of the *run phase*,
which is the right measurement for codegen quality and structurally cannot see
the column that is 56% of a real command.

So a change that makes the translator slower and the translated code faster
reads as a clean win in every harness in this directory and a regression on
`ls`. That is not hypothetical: it is what happened to the work this file was
written during. A second optimiser round bought a few percent on nbench and
cost **1.57-1.87x on translate time for all fifteen programs above**, which no
`bench/` harness reported, because none of them time translation.

If you change anything on the lifting or emitting path, run
`bench/startup-matrix.sh` as well. It is the only harness here that shows both.

## What is not worth optimising

Measured at this commit, so these are current:

### The bounds guard: 2% of perl, 0% of openssl

`GEX_UNSAFE_NO_GUARD=1` at build time drops the `select`/branch that clamps a
guest effective address, which is the entire memory-safety mechanism -- so it
is a measurement tool, not an option. Best of two, each side:

| program | guarded | unguarded | cost |
|---|---:|---:|---:|
| perl | 1796 | 1761 | 2.0% |
| openssl | 686 | 688 | 0% |

The guard has been described in this repo as 8-10% of a memory-heavy program.
On real programs at this commit it is 2% and nothing. V8 folds the compare into
the load's own bounds check often enough that the marginal cost is close to
zero, and openssl -- which is almost entirely `memory.copy` and arithmetic in
libcrypto -- cannot see it at all.

Do not spend effort here. There is more time in the 56% column above than the
guard costs in total.

### The intrinsic table finds one function

`crates/gex-xlate/src/intrinsic.rs` substitutes a wasm `memory.copy` for a
recognised `memcpy`. Against Debian trixie's aarch64 `libc.so.6`:

```
libc.so.6: 2775 symbols
  recognised, non-IFUNC, in text: mempcpy, __mempcpy    (one function, two names)
```

aarch64 glibc resolves `memcpy`, `memmove` and `memset` through IFUNCs, whose
resolver address is not an implementation and is correctly skipped. The real
`__memcpy_generic` and siblings are *local* symbols, and a shipped `libc.so.6`
has no `.symtab` -- only `.dynsym` survives. So the mechanism works exactly as
designed and finds `mempcpy`.

It is worth having for statically linked and musl-linked objects, where
`memcpy` is an ordinary `STT_FUNC`. It is not a lever on a Debian rootfs.
Reaching glibc's would mean reading the GOT slot after `ld.so` has run the
resolver, which is a runtime hook this does not have.

(Until the commit that added this file, the table was keyed at twice the load
bias -- `Symbol::value` is already biased by `elf::read_symbols` and `gexc`
added the bias again -- so it matched nothing at all, in any object, for as
long as it had existed. `crates/gex-ld/examples/dump-relocs.rs` prints what an
object offers.)

### memory64 still does not pay

`bench/memory64/probe.mjs` times the same loop against a 32-bit memory, a
64-bit memory, and a 32-bit memory with the index widened -- four hand-written
`.wat` files, no gex involved, so it re-checks the engine rather than this
translator. On Node 26 the 64-bit memory is still slower. Re-run it before
believing otherwise; it costs seconds.

### The closure is inert

Building a closure (`gexc build --closure --sysroot`) produces the pins, the
images and 1,053,758 blocks of ahead-of-time code across five objects. Running
with `GEX_CLOSURE` and `GEX_PINS` set:

| | total | translating |
|---|---:|---:|
| openssl, no closure | 679 | 291 |
| openssl, with closure | 695 | 286 |

It translates the same work either way. Whatever the closure builds is not
reaching the running program, and has not been. This is a real bug with real
value behind it -- 41% of that openssl run is translation that a working
closure would delete -- and it is the single largest unclaimed win in the
tree. It is not a codegen problem.

## What did move

### `--exhaustive` is faster end-to-end

Static discovery from the entry point plus symbols finds **85,620 blocks** in
Debian's perl. `--exhaustive` finds **384,923**. So the ordinary build ships
22% of what the program contains, and the rest is translated at runtime, one
faulting page at a time, while the user waits.

| perl | total | translating |
|---|---:|---:|
| ordinary build | 1834 | 375 |
| `--exhaustive` | 1483 | 178 |

**19% faster end-to-end**, and it halves the time spent translating. The module
is much larger and takes longer to build, and it still wins, because a block
translated ahead of time is translated once by a batch compiler and a block
translated at runtime is translated one small module at a time with a fault
and an instantiation around it.

If you want one lever, this is the one that is already in the tree.

## Where a guest register should live

`bench/regstore/probe.mjs` prices four shapes against each other with
hand-written `.wat`: a value in linear memory, in an imported global, passed as
a call parameter, and reloaded per call. Every block exit flushes the registers
it wrote back to the context struct and the next block reloads them, so the
question is whether a global beats that store. Run it before designing
anything that moves register traffic; the answer depends on the engine and has
changed between Node versions.

`bench/compile.mjs` times V8's compile of a given `.wasm` in isolation, which
is how to tell a module-size change from a codegen change.

## Byte share is not time

A section of the emitted module being 20% of its bytes does not make it 20% of
the run. Guard sequences, register spills and dispatch stubs are all cheap
per byte and abundant; the arithmetic that dominates the clock is neither.
Measure the time.

## Things tried in this file's history that did not survive

Recorded so they are not repeated. **None of this code is in the tree** -- it
was written, measured, and reverted for the reasons given.

- **Global value numbering plus a second optimiser round.** A few percent on
  nbench, 1.57-1.87x slower translation on every real program. The translator
  runs at startup *and* at runtime, so anything added to it is paid twice, and
  the 56% column pays it.
- **If-conversion of small diamonds.** Worth roughly one kernel on nbench.
  Speculating a load in an arm is **unsound**: gex's out-of-bounds address is
  `POISON_ADDR`, which is past `GUEST_MEM_BYTES` and traps, so a load moved
  above its guard faults on an address the original program never used.
  Guarding the arm's address costs most of what the transform buys.
- **Registers as call parameters between blocks.** Built; did not transfer to
  real programs.
- **Growing regions.** Four variations, all neutral.

The pattern in all four: a transform that wins on a benchmark whose run phase
is seconds long, evaluated by a harness that only times the run phase, on a
system where half of a real command is startup.

## How to re-run any of this

```sh
# The split that matters. Two built trees, all fifteen programs.
bench/startup-matrix.sh 3 /path/to/baseline-worktree .

# One run's own split.
GEX_TIME_JIT=1 node shims/browser/js/run-node.mjs --dir "/=$ROOT" --cwd / \
    prog.wasm "$ROOT/usr/bin/perl" /gex-bench.pl

# Codegen quality only, two gexc binaries, real programs, on V8.
bench/real-v8.sh /path/to/gexc-a /path/to/gexc-b
bench/ab-bin.sh  /path/to/gexc-a /path/to/gexc-b

# Engine questions, no gex involved.
node bench/memory64/probe.mjs
node bench/regstore/probe.mjs
node bench/compile.mjs prog.wasm

# What an object offers the intrinsic table.
cargo run -p gex-ld --example dump-relocs -- "$ROOT"/lib/*/libc.so.6
```

`GEX_TIMING` breaks the build down by phase, `GEX_XLATE_STAGES` reports what
each stage of translation cost, `GEX_LIFT_THREADS` bounds the lifter's
parallelism, and `GEX_REG_HISTOGRAM` describes the register traffic that was
emitted. `GEX_TRACE_MISSES` shows what the runtime translator is being asked
for and is the fastest way to see why a program is spending its life in the
56% column.
