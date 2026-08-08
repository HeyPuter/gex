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

## Caveats worth keeping

* MINSECONDS=2 is short. Most kernels print "NOT 95 % statistically certain",
  and the browser run is noisier than the other two. Treat single-digit
  percentage differences as noise; the shape of the table is what is solid.
* 890 of the module's blocks end in an instruction gex cannot execute, which
  raise SIGILL if reached. None is on a path any kernel took, or the run would
  have died -- but a change that moves work onto one of them would look like a
  crash rather than a slowdown.
* The browser leg of this harness produced nothing at all until 2026-08-07,
  because the shim had stopped compiling to wasm and nothing noticed. The
  numbers above are the first ones it has ever produced.
