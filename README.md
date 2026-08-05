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

## Known fidelity gaps

These are consequences of the design, not oversights. Each one is a
deliberate trade recorded here so nobody has to rediscover it.

* **Protection bits are bookkeeping, not enforcement.** Wasm cannot fault on a
  stray load or store, so a write through a `PROT_NONE` page succeeds. The
  VMA tree still records protections, because `/proc/self/maps`, translation
  invalidation and kernel-mediated accesses all need them.
* **`fmadd` is not fused.** Wasm has no fused multiply-add, so the product is
  rounded before the addition and results can differ in the last place. This
  is PLAN §5.7's fast mode; strict mode needs a software FMA.
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
  half precision, and the fixed-point converts — 116 instructions across a
  752k-instruction corpus of system libraries.
* **CRC32 is not implemented.** gex does not advertise the extension in
  `AT_HWCAP`, so no well-behaved guest emits it.
* **`fcmpe` does not raise on quiet NaN**, because FP exceptions are not
  modelled at all.
* **Protection changes do not invalidate translations.** A page rewritten
  before it was ever branched to needs nothing, because translation is lazy
  and keyed by address. Invalidating a page that *was* already translated —
  the `IC IVAU` hook in PLAN §6.2 — is not implemented, so a guest JIT that
  reuses an address for different code would run the old translation.
