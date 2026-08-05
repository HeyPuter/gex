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

## Status: M0 complete

PLAN's M0 exit criterion is *"static musl `hello`, `cat`, `echo` run; the
differential-fuzz harness runs in CI."* Both hold. The work also reaches into
M1 and M2 in the places the exit criterion forced it to — musl's `memcpy` and
`memset` are NEON, so NEON could not wait, and `printf("%f")` is scalar
floating point, so that could not either.

What runs today:

* static and static-PIE aarch64 ELF images (`ET_EXEC` and `ET_DYN`);
* the integer instruction set, scalar single/double floating point, and the
  NEON subset real string and memory routines are built from;
* the T0 syscall tier — enough for a program to start, do file and terminal
  I/O, manage memory and exit;
* a native host on wasmtime.

What does not:

* **Dynamic linking.** `PT_INTERP` is rejected with a clear error. gex-ld
  (PLAN §6.4) is the largest single piece of remaining work.
* **Threads, signals, `fork`/`exec`.** The context struct, safepoint counter
  and interrupt flag are laid out for them; nothing drives them yet.
* **Runtime translation.** An address with no translation reports precisely
  where and why instead of translating on demand (PLAN §6.2, M3).
* **Most of NEON**, and the FP fidelity modes in PLAN §5.7. Fast mode only.
* **The browser host.** The kernel is written against a byte slice
  specifically so it can be compiled to wasm later without a second
  implementation, but the Worker/SharedArrayBuffer shim does not exist.

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
| `gex-kernel` | the Linux personality: syscalls, VFS, memory map |
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
fails to find lands on a stub that reports the exact address. That is what
made it safe to ship an aggressive discovery strategy and tighten it later.

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

Across 584,417 instructions from linked aarch64 binaries — musl, libstdc++,
libgfortran, libgomp, libitm, libgcc — there are zero mnemonic mismatches.

**Lifter, against QEMU.** Random instructions are wrapped in a harness that
seeds every register and the flags, executes the candidate, and dumps the
register file. gex and QEMU must produce identical bytes. A failure arrives
already minimised to one instruction:

```console
$ cargo run --release -p gex-difffuzz -- --iters 5000 --verbose
```

**Whole programs, against QEMU.** Compiled C exercising arithmetic and flags,
floating point, memory and strings, control flow (including jump tables,
function pointers and `longjmp`) and the syscall surface, at `-O0` and `-O2`,
static and static-PIE:

```console
$ tests/integration/run.sh
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
  returning the non-NaN operand.
* **CRC32 is not implemented.** gex does not advertise the extension in
  `AT_HWCAP`, so no well-behaved guest emits it.
* **`fcmpe` does not raise on quiet NaN**, because FP exceptions are not
  modelled at all.
* **Exhaustive discovery is a stopgap.** Until runtime translation lands,
  every decodable 4-byte-aligned text address is translated ahead of time,
  which costs module size. Jump tables are why: they store scaled relative
  offsets that no pointer scan can recognise.
