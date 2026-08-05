# gex

Take an unmodified AArch64 Linux binary, translate its machine code to
WebAssembly ahead of time, and run it against a userspace re-implementation of
the Linux kernel ABI.

`PLAN.md` is the design document. This README says what is actually built and
what is not, because the gap between the two is the useful thing to know.

```console
$ file hello
hello: ELF 64-bit LSB executable, ARM aarch64, statically linked

$ gexc run hello
hello from aarch64
```

That binary was compiled by `aarch64-linux-musl-gcc`, never ran on an ARM
core, and executed here as WebAssembly.

---

## Status

M0 and M1 are done, M2 is most of the way, and M3's runtime translation
landed. What runs today:

* **static, static-PIE and dynamically linked** aarch64 ELF images — musl's
  own interpreter loads itself, maps libc, relocates and runs;
* **the instruction set**: integer, scalar single/double floating point, and
  NEON — lane arithmetic, permutes, horizontal reductions, conversions,
  table lookup. 317 instructions out of 590,000 in the corpus are still
  undecoded, all of them SIMD long tail;
* **runtime translation** — a branch to an address with no translation gets
  one, so a guest JIT or a late-loaded library needs no special handling;
* **signals** — handler frames, `sigreturn`, alternate stacks, synchronous
  faults, and delivery inside tight loops;
* **a real filesystem** — mount table over tmpfs, host passthrough, synthetic
  `/proc` and `/dev`, with the T0-T2 syscall tiers over it;
* a native host on wasmtime, and the same modules execute unmodified on V8.

What does not:

* **Threads.** The futex is built and tested; `clone` is not. The host needs
  restructuring first — a shared memory instead of a module-owned one, one
  store per thread because a wasmtime store is not shareable, and a
  translation cache each thread instantiates from because a wasm table is not
  shareable either. That shape is what the browser runtime needs anyway.
* **`fork`/`exec`.** `fork` needs a second address space and the bounded
  page copy PLAN §6.3 describes; `exec` needs to tear down and reload in
  place.
* **Tier-2.** Everything runs on tier-1 blocks. CFG recovery, superblocks and
  cross-block register allocation are unbuilt (PLAN §5.5, M4).
* **glibc.** The `GLIBC_PRIVATE` shim of PLAN §6.4 does not exist, so only
  musl's cooperative path works. The dlopen manifest and prebinding are
  likewise unbuilt.
* **The browser host.** The kernel is written against a byte slice
  specifically so it can be compiled to wasm without a second
  implementation, but the Worker/OPFS shim does not exist.

---

## How it fits together

```
 aarch64 ELF ─▶ gex-ld ─▶ gex-xlate ─▶ gex-isa ─▶ gex-lift ─▶ gex-emit ─▶ .wasm
                 load      discover     decode      GIR       tier-1 wasm
                                                                    │
                          gexc run ◀── shims/native ◀── gex-kernel ◀┘
                                        wasmtime        syscalls
```

| crate | what it owns |
|-------|--------------|
| `gex-abi` | the layout contract: thread context, address space, host imports |
| `gex-isa` | A64 decoder and disassembler |
| `gex-ir` | GIR, the SSA IR, and its block-local passes |
| `gex-lift` | AArch64 semantics, expressed in GIR |
| `gex-emit` | wasm binary writer and tier-1 code generation |
| `gex-ld` | ELF loading, relocations, initial process stack |
| `gex-xlate` | code discovery and the translation driver |
| `gex-kernel` | the Linux personality: syscalls, VFS, signals, memory map |
| `shims/native` | wasmtime host |
| `gexc` | the command-line driver |

### The three decisions that shaped the code

**Guest virtual address equals linear-memory offset.** Because gex is the
kernel, it allocates every guest address itself and keeps them inside one
wasm32 memory. A guest load becomes a wasm load with no translation
arithmetic — just a truncation and one guard that steers a 33-bit effective
address into an unmapped top page, so a wild pointer traps instead of
silently aliasing low memory.

**One wasm function per guest basic block, chained with `return_call`.**
Guest call depth never becomes wasm stack depth, and any 4-byte-aligned
address can become an entry point later without retranslating anything.
Indirect branches go through a funcref table indexed by `(pc - base) >> 2`.

**Discovery is never load-bearing for correctness.** Anything the translator
fails to find lands on a stub, which now translates it on demand. That is
also what makes it safe for discovery to *speculate*: runtime translation
constant-folds through initialised memory to read a PLT's GOT slot and follow
the call, and if that slot later changes the only cost is a wasted
translation, never a wrong branch.

**The dispatch table is a page directory, not a flat array.** A flat table
indexed by `(pc - base) >> 2` cannot span an address space with libraries
mapped far apart. One `u32` per 4 KiB page of guest text, held in guest
memory, keeps the table proportional to the code actually mapped. The lookup
stays branchless: a page with no translation has an entry of zero, so the
index lands in a reserved span of miss stubs, which is the answer wanted
anyway.

---

## Building and running

Needs a Rust toolchain. Running the tests additionally needs an aarch64 musl
cross toolchain and `qemu-aarch64` as the oracle.

```console
$ cargo build --release

$ gexc run <elf> [args...]      # translate and execute
$ gexc build <elf> -o out.wasm  # translate only
$ gexc disasm <elf>             # disassemble
$ gexc ir <elf> --at 0x400abc   # dump the GIR for one block

$ gexc run --strace --verbose <elf>   # syscall trace and translation stats
```

## Testing

Three layers, none of which trust gex to check itself.

**Decoder, against binutils.** Every instruction in a real binary is decoded
and re-rendered, then compared to `objdump`:

```console
$ aarch64-linux-musl-objdump -d /path/to/binary \
    | cargo run --release --example objdump-diff -p gex-isa
```

Across 590,593 instructions from linked aarch64 binaries — musl, libstdc++,
libgfortran, libgomp, libitm, libgcc — there are zero mnemonic mismatches and
317 undecoded.

**Lifter, against QEMU.** Random instructions are wrapped in a harness that
seeds every register and the flags, executes the candidate, and dumps the
register file. gex and QEMU must produce identical bytes. A failure arrives
already minimised to one instruction:

```console
$ cargo run --release -p gex-difffuzz -- --iters 5000 --verbose
```

7,500 cases — 5,000 single instructions and 2,500 four-instruction sequences,
across all thirteen data-processing families — currently agree with QEMU
exactly.

**Whole programs, against QEMU.** Thirty-seven comparisons over compiled C,
C++ and hand-written assembly exercising arithmetic and flags, floating point, memory and strings,
control flow (jump tables, function pointers, `longjmp`), the filesystem and
descriptor surface, signal delivery, `-O3` auto-vectorised NEON, the NEON
structure loads driven through ACLE intrinsics, conditions that outlive the
block that set them, and a C++ program that throws across a shared-object
boundary — built static, static-PIE and dynamically
linked, at `-O0`, `-O2` and `-O3`, with runtime translation both on and off.
A comparison that fails is retried once and reported as `FLAKY` if it then
passes, so a genuine intermittent difference is distinguishable from a
consistent one:

```console
$ tests/integration/run.sh
```

**A conformance suite.** Lua's own test suite — 18 of its files, covering
bitwise operations, closures, `goto`, to-be-closed variables, metamethods,
string patterns, UTF-8, `string.pack`, coroutines, error handling, the garbage
collector, `math` and `sort`. Every one produces byte-identical output to
QEMU, except `math` and `sort`, whose only differences are a random seed and
printed millisecond timings. `all.lua` fails on both engines, but not for the
same reason: QEMU forks and execs successfully and then the *host* shell
cannot run an aarch64 binary, while gex returns `ENOSYS` from `clone`.

Finding that suite is what turned up the `fmadd` bug below. Four of its files
failed before it was fixed.

**Real programs.** BusyBox 1.36.1, built static, runs `sort`, `awk`, `sed`,
`grep`, `tr`, `uniq`, `head`, `tail`, `wc`, `expr`, `date`, `basename`,
`md5sum`, `sha256sum` and a `gzip`/`gunzip` round trip through a pipe. Every
line matches QEMU's, and all 246,949 of its instructions disassemble exactly.

Lua 5.4.6 runs a script exercising the parts of a language
runtime that stress a translator: the interpreter's computed-goto dispatch,
recursive `fib`, a 100k-element sort, string interning and pattern matching,
metatables and inheritance, closures, coroutines, `pcall` (which is `longjmp`
underneath), and the garbage collector. Output is identical to QEMU's.

SQLite 3.45 built for aarch64-linux-musl — 285k lines of
C, 290k guest instructions, FTS5 and JSON1 and R-Tree compiled in — runs a
script covering recursive CTEs, indexes, joins, full-text search, JSON
extraction, a transaction and its rollback, and `ANALYZE`. Output is identical
to QEMU's, and with `randomblob` replaced by `zeroblob` so the run is
deterministic, the 1.4 MB database file the two engines produce is byte-for-byte
the same. Across libstdc++, libgfortran, SQLite, Lua and BusyBox — 1.1 million
instructions — the disassembler reproduces every one exactly, with no
undecoded instructions and no operand-text differences from binutils. Over the
whole 1.34-million-instruction corpus, including the system libraries'
feature-gated paths, 111 remain undecoded and 2 differ in operand text.

**Cross-engine, on V8.** The native host runs on wasmtime; the browser target
is V8. Tail calls are load-bearing enough that a difference between the two
engines would invalidate the design, so the *same* module `gexc build`
produced is also executed under Node:

```console
$ gexc build prog -o prog.wasm
$ node tests/cross-engine/run-node.mjs prog.wasm prog
```

`cargo test --workspace` runs the unit tests.

---

## Performance

A CPU-bound mix of integer ALU work, unpredictable branches, array traversal,
double-precision arithmetic and `memcpy`/`memset`, compiled `-O2`, run three
times and taking the best. Startup is separated from throughput by running the
same benchmark at two work scales and taking the marginal cost, because gex's
fixed cost is large and its per-unit cost is not.

| | startup | throughput vs native | vs QEMU |
|---|---|---|---|
| native x86-64 | — | 1.0x | |
| **gex, tier-1** | 2.54 s | **2.2x** | **6.5x faster** |
| qemu-aarch64 (user mode) | 0.26 s | 14.3x | |

All three produce byte-identical output.

Two caveats, both worth stating plainly. The native baseline is x86-64, not
aarch64, so this is the same C source at `-O2` rather than the same machine
code — PLAN §13's target is against native aarch64 and this is not that
measurement. The QEMU comparison *is* apples to apples: same guest binary,
same host, same work.

The headline is that tier-1 — the completeness tier, whose target in PLAN §13
is only 8-10x — is already inside the band set for tier-2. The startup cost is
the other half of the story: it is almost entirely wasmtime compiling the
thousands of functions exhaustive discovery produces, and it is what the
content-addressed translation cache and runtime translation exist to remove.

### Startup: the compiled-module cache

Translating a program to wasm is cheap. Compiling that wasm is not, and on a
large program it is nearly all of startup:

| SQLite 3.45 (285k lines of C, 290k guest instructions) | |
|---|---|
| gex translates it to 14 MB of wasm | 1.8 s |
| Cranelift compiles that | ~31 s |
| everything else (instantiate, place the guest, run `select 1`) | < 0.1 s |

The compiled code is the same on every run of the same binary, so it is kept.
The key is a SHA-256 over the *placed* segments, where they were placed, and
the translation options — the placed image rather than the file, because that
is what the translator reads. wasmtime checks engine compatibility itself when
the artifact is loaded, so the key does not have to cover it, and an artifact
it rejects is deleted and rebuilt rather than being an error.

The dispatch metadata a translation produces — how many table slots, which
pages it claimed, where the page table left off — is written beside the
compiled module, so a hit skips translating too.

That leaves what static discovery cannot find. SQLite's VDBE is a
computed-goto interpreter, and recursive descent cannot follow a jump table:
10,000 blocks were being translated on demand, every run. So gex records where
execution actually went and reads it back next time as extra entry points.
That changes the translation, hence the cache key, so a run that discovers
something new compiles once more and then settles:

```console
$ gexc run sqlite3 :memory: 'select 1;'
37.9 s      # translate, compile, discover 10230 blocks at run time
32.1 s      # translate those too, compile again
 0.16 s     # cache hit, nothing left to discover
 0.16 s
```

On the SQL script from the testing section, that is 35.0 s settling to 0.99 s
against QEMU's 1.14 s; on a heavier one — 200k rows inserted, indexed, joined
— 7.0 s against QEMU's 6.3 s. The profile is a hint and never a requirement:
a stale entry costs a wasted translation, and an address that is no longer
code is dropped by discovery's own executability check. The honest cost is the
occasional expensive run when a new path is found, which on a program that
does something different every time is a recompile that never quite settles.

`--no-cache` turns all of it off. `--lazy` is the opposite trade — discover
only from the entry point and leave the rest to runtime translation, 33 s to
9.5 s with nothing cached, which is what a genuinely one-shot run wants.

The key includes a fingerprint of the running `gexc` — its size and
modification time — and a build that cannot identify itself does not use the
cache at all. This is not belt-and-braces. A decoder fix that makes an
instruction translatable changes nothing else the key can see: not the guest
image, not the options. Without the fingerprint the fixed binary reuses the
broken module, and the failure looks exactly like the fix not working. It cost
an hour of chasing a bug that was already repaired.

Deserialising a compiled module is `unsafe`: the bytes become executable code
with only a header check in the way. That is why `gex-cache` is a crate of its
own — the host forbids `unsafe`, and this is the one place the argument has to
be made and written down.

### Two tier-1 optimisations

Both are measured the same way: the minimum of runs alternating between the
transform enabled and disabled, on a VM noisy enough that a single run means
nothing and even a minimum-of-eight moves by a few percent between sessions.
The combined column is the reliable one; attributing a share to each transform
individually is at the edge of what this machine can resolve.

| workload | baseline | both on | speedup |
|---|---|---|---|
| `ldr q`/`ldr q`/`fmla`/`str q` + counted branch, 200M | 1.803 s | 0.726 s | **2.48x** |
| `add`/`cmp`/`b.ne`, 200M | 1.452 s | 0.697 s | **2.08x** |
| `fmla` accumulating into one register, 200M | 1.877 s | 1.274 s | **1.47x** |
| 320x320 float matmul, 12 passes | 1.514 s | 1.267 s | **1.19x** |
| matmul + dot product + `fccmp` classification | 1.368 s | 1.241 s | **1.10x** |

The pattern is consistent: a loop whose cost was bookkeeping gets most of it
back, and a loop already bound by a floating-point dependency chain or by
memory does not.

**Self-loop fusion** (`GEX_NO_LOOP_FUSION=1` disables it). A guest block whose
only successor is itself becomes a real wasm `loop` rather than a tail call to
its own function, and the guest registers it carries stay in wasm locals
across the back-edge instead of round-tripping through the context every
iteration. The safepoint poll still runs on the back-edge, and its slow path —
which may deliver a signal and unwind — flushes first, so a handler sees the
right architectural state; the flush sits inside the conditional, so the
emitter keeps treating those registers as dirty for the paths that leave. A
block containing a host call that can rewrite guest state is not fused at all.

Only the *live-in* registers are preloaded, not everything the block reads.
That is not a micro-optimisation: a preloaded register is live for the whole
loop, and a wasm local that stays live is a machine register Cranelift cannot
use for anything else. Preloading the full read set made matmul *slower* than
not fusing at all.

**Cross-block flag liveness** (`GEX_NO_FLAG_LIVENESS=1` disables it). Every
`cmp`, `adds` and `subs` writes all four of NZCV; the branch that follows reads
one or two. Within a block the other three have to be kept, because a successor
might want them — but once the whole control-flow graph is in hand that
question is answerable, and the answer is almost always "nothing". Dropping the
write lets dead-code elimination delete the arithmetic behind it, which for a
`subs` is the sign, zero, carry and overflow synthesis: about a third of the
statements in a tight loop. On a static C++ binary the pass deletes roughly
2000 statements and takes 5% off the module.

It is an ordinary backwards liveness fixpoint over the blocks a translation
unit produced. What makes it safe is what it refuses to reason about: an
indirect branch, a syscall, a SIGILL, or any successor not translated in the
same unit marks the block opaque, and an opaque block keeps every flag. An
incomplete graph loses the optimisation rather than producing wrong code. A
`bl`'s return site counts as a successor even though it is not a control-flow
edge, so `cmp; bl f; b.eq` — legal whenever `f` preserves the flags, which the
ABI permits though no compiler relies on it — keeps its compare.

This is the part of tier-2 that PLAN §5.4 describes as needing a CFG, done in
tier-1 because discovery had already built one.

Because a compiler almost never emits a condition that crosses a block
boundary, the C tests cannot exercise the cases this analysis must not break.
`tests/integration/programs/flags.S` is hand-written assembly that does:
conditions consumed one and two edges away from their compare, across a call,
through `adc` and `ccmp`, and as a loop's exit test. With the analysis
deliberately broken to treat every flag as dead, that program hangs — which is
the point of having it.

---

## Threads

Guest threads are real host threads, as PLAN §8 asks: a `clone` with
`CLONE_THREAD` spawns one, and a futex wait is a real wait rather than a
scheduled one.

The shape follows from two facts about wasmtime. A `Store` is `!Sync`, so each
guest thread has its own — and therefore its own instances and its own
dispatch table. And a module-defined memory belongs to its instance, so the
guest's memory is host-created, shared, and imported by every module. The ABI
already anticipated the rest: contexts are addressed as `ctx_addr(slot)`, and
every translated function takes its context as a parameter, so starting a
thread is a call to `enter` with a different one.

Three pieces make it work:

* **A shared kernel with an explicit per-thread seam.** `Kernel` holds process
  state — descriptors, the VMA tree, mounts, signal dispositions — behind one
  mutex. The three fields that describe *the calling thread* (`ctx_addr`,
  `tid`, `clear_child_tid`) live in the host's per-thread state and are
  installed under the same lock that guards the syscall, so what the kernel
  holds always describes whoever owns it.

* **Blocking syscalls run unlocked.** This is the part that is easy to get
  wrong and impossible to miss: a futex wait holding the kernel lock can never
  be woken, because the waker needs the same lock to issue the wake. `futex`
  and `nanosleep` are recognised before dispatch and answered as a
  `BlockOp` that the host performs with nothing held, then completes under the
  lock again. The first version deadlocked on exactly this.

* **A registry of translated modules.** Each store's dispatch table starts
  with only what the image module installed. Everything translated since is
  replayed into it — instantiate, not retranslate — and the same registry
  answers a dispatch miss for code another thread has already compiled. It is
  append-only and ordered, so a store catches up by walking forward from its
  own watermark.

Scaling, on a 24-core VM, with each thread running the same independent
CPU-bound loop:

| threads | wall | aggregate | vs QEMU |
|---|---|---|---|
| 1 | 2.22 s | 180 M iter/s | 2.86 s |
| 2 | 2.32 s | 344 M iter/s | 2.60 s |
| 4 | 2.74 s | 584 M iter/s | 2.60 s |
| 8 | 3.37 s | 949 M iter/s | 3.32 s |
| 16 | 5.61 s | 1140 M iter/s | 4.99 s |

Sixteen times the work for two and a half times the wall clock.

### `fmadd` is fused, because it has to be

Wasm has no fused multiply-add. Rounding the product before the addition
sounds like a last-place difference, and PLAN §5.7 files it under fast mode —
but musl builds its double-double arithmetic out of exactly this instruction,
and the second rounding destroys it. Under gex, Lua's `2^10` was
1024.0000000000002 and `2^30` no longer converted to an integer. Four files of
Lua's test suite failed on it.

There is no way to express one rounding in wasm, so translated code asks the
host: a `gex.fma` import taking the three operands as bit patterns. Every host
has an FMA instruction, and a browser shim can supply its own. It is a call
per `fmadd`, which sounds expensive and is not — a loop doing nothing but
`__builtin_fma` runs 8x faster under gex than under QEMU, and byte-identically.

The vector `fmla` is still unfused; wasm has no vector FMA either, and the
same trick would mean a host call per lane.

### The snapshot that was rebuilt on every syscall

A syscall-heavy threaded benchmark — `clock_gettime` and `getpid` in a loop —
was the measurement that found this. Eight threads took **31.7 seconds**
against QEMU's 0.58, and even one thread was four times slower than QEMU.

The cause was not the lock. `/proc` is served from a snapshot of the kernel's
state, and that snapshot was refreshed *before every syscall*: formatting the
whole VMA tree into `/proc/self/maps`, listing every descriptor, and rebuilding
`cmdline` and `environ`. Hundreds of allocations before a `clock_gettime`.

A `/proc` file's contents are captured when it is opened, so only a syscall
that resolves a path needs the snapshot to be current. That is now a list of
syscalls that *skip* the refresh rather than a list that requires it —
forgetting to exclude one costs a little speed, while forgetting to include
one would serve a stale `/proc`.

| | before | after | QEMU |
|---|---|---|---|
| 1 thread | 1.50 s | **0.11 s** | 0.40 s |
| 8 threads | 31.75 s | **1.41 s** | 0.58 s |
| threaded SQLite, 6 writers | 4.5 s | **3.2 s** | 4.4 s |

3.7 M syscalls/s on one thread, against QEMU's 1 M. What remains is the single
kernel lock: aggregate syscall throughput falls from 3.7 M to 1.7 M between one
thread and sixteen, where QEMU stays flat. Splitting the lock — descriptors,
the VMA tree, mounts and signals are almost never touched by the same call —
is the next move, and now it really is the lock.

Beyond the synthetic test: SQLite built with `SQLITE_THREADSAFE=1`, six
threads inserting 24,000 rows through one shared connection — so every
statement takes the connection's mutex and the futex traffic is heavily
contended. Correct on the first run and on ten more, at 4.5 s against QEMU's
4.3 s.

`tests/integration/programs/threads.c` covers eight workers over a mutex,
thread-local storage, `pthread_once`, a condition variable with a separate
producer and consumer, and detached threads that nobody joins. Every number it
prints is order-independent, so the comparison against QEMU is of the
arithmetic rather than of the scheduler. It ran 20 times without a difference.

When the process ends, the other threads have to stop before the stores and
the memory they are using go away. `exit_group` sets a flag and releases
everyone parked on a futex, since no guest code is going to wake them now.
Threads notice it at a safepoint — and also at syscall entry, because a thread
looping on a blocking call is exactly what a condition variable is, and it
never reaches a back-edge safepoint at all. With only the safepoint check, two
threads parked on a `pthread_cond_wait` sat out the whole 500 ms grace period;
with both, teardown takes a millisecond.

## Processes

`fork` and `execve` work, which is what a shell is made of.

A `clone` without `CLONE_THREAD` is a process, and a process needs a second
address space. It gets one: a fresh shared memory with the parent's mapped
regions copied into it. There is no copy-on-write, because wasm gives the host
no way to fault on a write, so the cost is what the parent has mapped rather
than what the child touches. What the child *does* share is what Linux says it
should — descriptors are `Arc` handles, so parent and child hold the same open
file descriptions, offsets included, while the tables are separate.

The child reuses every module the parent has translated. Same program, same
addresses, and a translated module imports the memory it acts on rather than
owning it, so instantiating one against a different address space is exactly
what that import is for.

`execve` keeps the process — its pid, its descriptors, its working directory,
its mounts — and replaces the address space and the program in it. From there
it is the same work as starting fresh, so it is the same function, told not to
build a new kernel. Close-on-exec descriptors go; the rest survive, which is
how a shell hands a pipe to the command it starts. Everything that can fail is
checked *before* the point of no return, because a failed `execve` has to
return: the guest sees `ENOENT` and carries on in the program it already had.

QEMU cannot be the oracle here. `qemu-aarch64` re-execs only *itself*, for the
program it was given, so a guest that execs another aarch64 binary fails under
QEMU and works under gex. The exec test therefore checks against a written-down
expectation of what Linux does, and the harness has an `expect` alongside
`compare` for exactly this.

A `clone` that names a child stack starts the child on it. A plain `fork`
passes none and the child keeps the parent's SP, but `posix_spawn` allocates
one and musl's `__clone` trampoline pops the function to run off it — a child
left on the parent's stack pointer reads whatever happens to be there, which is
how every `os.execute` and `popen` died with SIGSEGV before reaching `execve`
at all.

With that, Lua running under gex spawns a real aarch64 BusyBox `sh` out of a
sysroot: `os.execute` prints through the shell, the exit status comes back, and
`io.popen` reads the command's output through a pipe.

Lua's own `all.lua` driver gets three levels deep on this — gex runs Lua, Lua
execs the shell, the shell execs Lua again and it prints its version banner —
and then fails in `main.lua`'s interactive-interpreter checks, on an assertion
about prompt output with stdin redirected from a file. That one is
undiagnosed. QEMU cannot be used to tell whether it is gex's fault, because
QEMU does not get as far as running the shell at all.

`--cwd` and `--argv0` exist for this: under a sysroot the host path a program
was loaded from is not a path the guest can resolve, and a program that passes
its own `argv[0]` to a shell needs it to be one that is.

Not done: `vfork`'s suspend-the-parent contract (a full fork is stricter, and
`posix_spawn` synchronises through a pipe anyway), and `execve` from a thread
other than the process's first.

---

## Known fidelity gaps

These are consequences of the design, not oversights. Each one is a
deliberate trade recorded here so nobody has to rediscover it.

* **Protection bits are bookkeeping, not enforcement.** Wasm cannot fault on a
  stray load or store, so a write through a `PROT_NONE` page succeeds. The
  VMA tree still records protections, because `/proc/self/maps`, translation
  invalidation and kernel-mediated accesses all need them.
* **`frinta` rounds ties to even, not away from zero.** Wasm's `nearest` is
  the only rounding primitive available; the two differ only exactly on
  halfway values.
* **`fminnm`/`fmaxnm` propagate NaN** like `fmin`/`fmax`, rather than
  returning the non-NaN operand. The same holds for the across-lane
  `fminnmv`/`fmaxnmv` and the pairwise `fminnmp`/`fmaxnmp`.
* **`fmulx` is `fmul`.** They differ only where zero meets infinity, which
  requires modelling a case wasm's multiply does not distinguish.
* **The remaining undecoded instructions** are `ld3`/`ld4`, the
  halving-narrow (`addhn`) and saturating-doubling (`sqdmulh`) multiplies,
  and the register-shift forms (`ushl`/`sshl`). Nothing in the corpus reaches
  them.
* **CRC32, half precision and SVE are not implemented.** None is advertised
  in `AT_HWCAP`, so no well-behaved guest emits them. They do appear in
  libgcc's and musl's feature-gated paths — 97 half-precision `fcvt`s and 14
  SVE instructions across the system libraries — which is why the corpus
  reports a handful of undecoded instructions that nothing reaches.
* **`fcmpe` does not raise on quiet NaN**, because FP exceptions are not
  modelled at all.
* **Protection changes do not invalidate translations.** A page rewritten
  before it was ever branched to needs nothing, because translation is lazy
  and keyed by address. Invalidating a page that *was* already translated —
  the `IC IVAU` hook in PLAN §6.2 — is not implemented, so a guest JIT that
  reuses an address for different code would run the old translation.
