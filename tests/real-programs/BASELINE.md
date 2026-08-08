# nbench, three ways

Recorded 2026-08-07 by `tests/real-programs/nbench.sh`, MINSECONDS=2,
24-core x86-64 host. Iterations per second; the multiplier is against QEMU.

| KERNEL           | QEMU      | gex (native) |       | gex (browser) |        |
|------------------|-----------|--------------|-------|---------------|--------|
| ASSIGNMENT       | 8.8244    | 5.5049       | 0.62x | 8.385         | 0.95x  |
| BITFIELD         | 1.6097e+08| 2.9955e+08   | 1.86x | 2.4045e+08    | 1.49x  |
| FOURIER          | 589.75    | 8990.6       |15.24x | 9761.6        |16.55x  |
| FP EMULATION     | 168.49    | 143          | 0.85x | 181.36        | 1.08x  |
| HUFFMAN          | 612.4     | 631.51       | 1.03x | 926.23        | 1.51x  |
| IDEA             | 2344.5    | 1854.9       | 0.79x | 2194.7        | 0.94x  |
| LU DECOMPOSITION | 15.938    | 137.18       | 8.61x | 60.957        | 3.82x  |
| NEURAL NET       | 0.66013   | 10.08        |15.27x | 7.4206        |11.24x  |
| NUMERIC SORT     | 300.23    | 205.52       | 0.68x | 276.34        | 0.92x  |
| STRING SORT      | 63.896    | 112.26       | 1.76x | 146.87        | 2.30x  |

The module: 27,594 blocks, 149,913 guest instructions, 7,225 KiB of wasm
(12.3x the guest's text), built with `gexc build --exhaustive`, so every
block is statically recompiled and nothing is translated at run time.

## The first thing to notice

**V8 is faster than Cranelift on six of the ten kernels**, and by a lot on
some: HUFFMAN 1.03x native against 1.51x in the browser, STRING SORT 1.76x
against 2.30x, ASSIGNMENT 0.62x against 0.95x. Native numbers are not a proxy
for browser numbers and never were. Anything decided from a `gexc run`
measurement is a decision about a debug adapter.

The exception is LU DECOMPOSITION, where it goes the other way and hard: 8.61x
native, 3.82x in the browser. That gap is the most diagnostic number in the
table -- the same wasm, one engine handling it more than twice as well as the
other -- and it is the place to look for something V8 does badly with what
tier-1 emits.

## Where the browser is still behind QEMU

    NUMERIC SORT   0.92x
    IDEA           0.94x
    ASSIGNMENT     0.95x

All three are close, and all three are QEMU's strong suits: tight integer
loops over arrays, where a template-JIT's per-block overhead is small and its
register allocation is good. These are the kernels a tier-2 pass would have to
earn its keep on.

## What the FMA call costs

The same suite built with `GEX_INLINE_FMA=1`, which replaces every fused
multiply-add with an unfused multiply and add. That is *wrong* -- it rounds
twice -- so it measures the prize rather than claiming it.

Compare the multipliers, not the raw rates: QEMU's own numbers moved 20-30%
between the two runs, so the machine was not the same, and the ratio is what
divides that out.

| KERNEL           | browser, fused | browser, inlined |        |
|------------------|----------------|------------------|--------|
| LU DECOMPOSITION | 3.82x          | 23.11x           | 6.0x   |
| NEURAL NET       | 11.24x         | 36.18x           | 3.2x   |
| FOURIER          | 16.55x         | 19.51x           | 1.2x   |
| everything else  | --             | unchanged        | noise  |

Exactly the three floating-point kernels, and in the order the static counts
predict: `DoNNetIteration` has 18 fused multiply-adds, `lusolve` 5 and `DoLU`
3, `DoFourier` 1. The rest of the table has none and does not move.

`crates/gex-emit/src/fma.rs` has arithmetic that is exact and can therefore
ship; what is left is to lower it instead of the call.

## After the inline FMA

Re-run 2026-08-07 with the fused multiply-add emitted inline. Browser
multipliers against QEMU, before and after:

| KERNEL           | before | after  |        |
|------------------|--------|--------|--------|
| LU DECOMPOSITION | 3.82x  | 8.03x  | 2.1x   |
| NEURAL NET       | 11.24x | 17.66x | 1.6x   |
| STRING SORT      | 2.30x  | 2.65x  | noise  |
| everything else  |        |        | noise  |

The two that moved are the two with fused multiply-adds worth counting. LU
matches the isolated measurement (2.2x) closely enough to believe both.

Several kernels appear to have got *worse* -- FP EMULATION 1.08x to 0.81x,
HUFFMAN 1.51x to 1.37x -- and that is the noise, not a regression. FP
EMULATION measured on its own is 160-182 iterations/sec with the call and
168-178 inline: the ranges overlap, and no fused multiply-add leaves the
module during it, so the guard is not failing either. Anything that looks
like a change here should be isolated before it is believed.

## Caveats worth keeping

* **The noise is about 30%, not a few percent.** QEMU running the same
  binary three times gave 8.82, 11.19 and 11.46 on ASSIGNMENT, and 300, 386
  and 383 on NUMERIC SORT. MINSECONDS=2 on a shared machine is simply not
  enough. Only effects the size of the FMA one are readable from this table;
  for anything smaller, isolate the kernel and run it repeatedly, which is
  what `-c<FILE>` with a cut-down config is for.
* 890 of the module's blocks end in an instruction gex cannot execute, which
  raise SIGILL if reached. None is on a path any kernel took, or the run would
  have died -- but a change that moves work onto one of them would look like a
  crash rather than a slowdown.
* The browser leg of this harness produced nothing at all until 2026-08-07,
  because the shim had stopped compiling to wasm and nothing noticed. The
  numbers above are the first ones it has ever produced.
