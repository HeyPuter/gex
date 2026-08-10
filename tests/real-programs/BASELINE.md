# nbench, three ways

Recorded 2026-08-10 by `tests/real-programs/nbench.sh`, MINSECONDS=2,
24-core x86-64 host. Iterations per second; the multiplier is against QEMU
measured in the same run, which is what makes the ratios comparable even
though the absolute numbers move with what else the host is doing.

| KERNEL           | QEMU      | gex (native) |       | gex (browser) |        |
|------------------|-----------|--------------|-------|---------------|--------|
| ASSIGNMENT       | 12.778    | 9.0521       | 0.71x | 12.945        | 1.01x  |
| BITFIELD         | 2.2932e+08| 4.4807e+08   | 1.95x | 4.482e+08     | 1.95x  |
| FOURIER          | 835.66    | 11446        |13.70x | 14958         |17.90x  |
| FP EMULATION     | 228.85    | 210.44       | 0.92x | 230.03        | 1.01x  |
| HUFFMAN          | 842.19    | 1009.3       | 1.20x | 1356          | 1.61x  |
| IDEA             | 3475.3    | 2797.7       | 0.81x | 3944.8        | 1.14x  |
| LU DECOMPOSITION | 23.015    | 172.11       | 7.48x | 187.84        | 8.16x  |
| NEURAL NET       | 0.90099   | 13.162       |14.61x | 16.293        |18.08x  |
| NUMERIC SORT     | 424.04    | 377.82       | 0.89x | 475.74        | 1.12x  |
| STRING SORT      | 85.024    | 200.53       | 2.36x | 243.13        | 2.86x  |

The module: 27,594 blocks, 149,913 guest instructions, 7,899 KiB of wasm
(13.5x the guest's text), built with `gexc build --exhaustive`, so every
block is statically recompiled and nothing is translated at run time.

## The browser now beats QEMU on all ten

Against the same suite recorded 2026-08-07, the browser improved on nine of
the ten kernels and the tenth moved 0.07x:

| KERNEL | 2026-08-07 | 2026-08-10 |
|---|---|---|
| ASSIGNMENT | 0.95x | 1.01x |
| BITFIELD | 1.49x | 1.95x |
| FOURIER | 16.55x | 17.90x |
| FP EMULATION | 1.08x | 1.01x |
| HUFFMAN | 1.51x | 1.61x |
| IDEA | 0.94x | 1.14x |
| LU DECOMPOSITION | 3.82x | 8.16x |
| NEURAL NET | 11.24x | 18.08x |
| NUMERIC SORT | 0.92x | 1.12x |
| STRING SORT | 2.30x | 2.86x |

Three kernels were below 1.0 and none are now. LU DECOMPOSITION doubled and
NEURAL NET went from 11.2x to 18.1x.

## The first thing to notice

**V8 is faster than Cranelift on nine of the ten kernels**, and by a lot on
some: HUFFMAN 1.20x native against 1.61x in the browser, IDEA 0.81x against
1.14x, NUMERIC SORT 0.89x against 1.12x. Native numbers are not a proxy for
browser numbers and never were. Anything decided from a `gexc run`
measurement is a decision about a debug adapter.

## What the emitter says about itself

    boundary: 15% registers, 5% safepoints, 4% dispatch
              (24% together, 1,918,511 bytes over 27,594 blocks)
    bounds guards: 11% (926,109 bytes)
    145 loops emitted as one function each, 366 blocks in them
    890 blocks end in an instruction gex cannot execute yet

A quarter of the module is the cost of blocks being separate functions --
registers handed across boundaries, safepoints, dispatch -- and another
tenth is bounds checking. Those are the two places a tier-2 optimizer (#15)
would find room, and the 890 blocks ending in an unimplemented instruction
are 890 ways for a program that wanders far enough to take SIGILL.
