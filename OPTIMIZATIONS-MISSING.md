# Optimizations gex does not have

Written 2026-08-09 against `7cade36`. Read from `gex-ir/pass.rs`,
`gex-emit/tier1.rs`, `gex-emit/region.rs`, `gex-lift/src/lib.rs`,
`gex-xlate/src/lib.rs` and `tests/real-programs/BASELINE.md`.

The JIT and the static recompiler share one backend — tier-1 is both — so
this splits into *codegen* (affects both paths) and *whole-program* (only the
AOT path can do it). Items PLAN already promises are marked; items the code's
own comments record as measured losses are listed at the end so nobody
re-treads them.

---

# Part 1 — Tier-1 codegen (the JIT, and also the AOT backend)

## A. Condition codes

1. **`cmp` + `b.cond` fusion.** [PLAN §5.4.1, unbuilt] `eval_cond` reads four
   1-bit registers and recombines them. For `cmp x0,x1; b.lt` DSE kills Z/C,
   but N (`result >> 63`) and V (~4 ops of overflow algebra) survive, then get
   compared for equality — ~8 wasm ops where `i64.lt_s` is one. The most
   common pattern in AArch64 integer code, and the biggest codegen gap here.
   QEMU (`cc_op`), FEX and Rosetta all defer or fuse.
2. **Lazy/deferred flag representation.** No equivalent of TCG's "remember the
   operands and the op, materialize NZCV only if read". Four independent bits
   makes DSE trivial (good) but forces eager materialization whenever a flag
   escapes a block.
3. **`ccmp`/`fccmp` chains.** Lowered by materializing all four flags twice
   plus a select. `cmp; ccmp; b.cond` — the canonical `&&`/`||` idiom —
   should collapse to a short-circuit boolean.
4. **`cset`/`csinc`/`csneg` fusion.** Same problem inverted: flags
   materialized then re-tested instead of folding the producing comparison
   into the select.
5. **NZCV packing for the flush path.** Four separate `i32` fields stored and
   loaded individually at block boundaries; one packed byte quarters that
   traffic where flags do escape.

## B. Value-level optimization

6. **No CSE / GVN / hash-consing.** `Builder::bin` unconditionally allocates a
   new `Def`. Repeated `adrp`+`add` bases, repeated sign-extensions, and the
   same address expression recomputed for each of a pair of accesses all
   survive to wasm.
7. **No algebraic simplification / peephole pass.** `pass::optimize` is
   const-fold, DSE, DCE. Missing: `x+0`, `x&-1`, `x<<0`, double negation,
   `and` after an already-zero-extended value, redundant
   `i32.wrap`/`i64.extend` round-trips.
8. **No known-bits / range analysis.** W-register writes are architecturally
   zero-extending; without a known-zero-upper fact the emitter keeps masks and
   extends that provably cannot change the value.
9. **No strength reduction.** Constant divide/modulo → multiply-high + shift;
   `mul` by small constants → shift/add. `mul_high` exists only as a general
   helper.
10. **No reassociation** to expose constant folding (`(x + c1) + c2`).
11. **No copy propagation across guest-register reads** beyond the local
    cache — `mov x0,x1` produces a real local round-trip.
12. **No if-conversion.** A tiny forward diamond stays two blocks, two tail
    calls and two full register flushes where `select` would do. Standard in
    every mature SBT.

## C. Memory

13. **No redundant-load elimination or store-to-load forwarding.**
    [PLAN §5.4.3, unbuilt] Memory ops are kept in strict program order with no
    exceptions. Prologue `stp`/epilogue `ldp` pairs and reload-after-spill
    around calls are pure waste in translated code.
14. **No alias analysis** (stack vs. heap vs. global vs. ctx) — the
    prerequisite for the above and for any reordering.
15. **No stack-slot promotion.** A frame slot provably not address-taken
    within a region could live in a wasm local. Large, because callee-saved
    spill/reload traffic dominates translated function bodies.
16. **Bounds guard emitted per access.** `emit_address` elides it only for
    literal-constant addresses; everything else pays `local.set`/`local.get`,
    `i32.wrap`, a const, `i64.lt_u` and `select` on every load and store.
    Missing: (a) dominance-based redundant-guard elimination when accesses
    share a base; (b) guard hoisting out of loops; (c) range facts for
    SP-relative accesses (`sp` is checked at entry under `GEX_CHECK_SP`, and
    the fact is not propagated). What was (d) — a `memory64` guest memory,
    where the engine's own bound check subsumes the select entirely — was
    built and measured and lost; see below.
17. **No address-mode reuse across a pair.** `ldp x0,x1,[x9,#16]` computes the
    guarded address twice rather than once with two static offsets.
18. **`ldp`/`stp` not fused** into a single `v128.load`/`v128.store` plus lane
    extracts where alignment permits.
19. **No idiom recognition.** [PLAN §5.4.4, unbuilt] memcpy/memset/strlen
    loops, `DC ZVA`, atomic spin loops. No bulk-memory ops are used at all —
    `memory.copy`/`memory.fill` turn a hot guest loop into one instruction.
20. **No caching of "this base is in range"** — each access re-derives the
    guard even when the base is unchanged.

## D. Control flow and dispatch

21. **No return-address stack.** [PLAN §5.6, unbuilt] `ret` takes the full
    indirect path: shift, directory load, mask, page offset, `br_table` over
    `DISPATCH_TABLES = 8`, `return_call_indirect`. Rosetta-style return
    prediction (compare against a small in-ctx stack, then a direct
    `return_call`) is the highest-value dispatch change available: call/return
    pairing predicts near-perfectly and `ret` is a large share of all indirect
    branches.
22. **No inline caches on `br`/`blr`.** [PLAN §5.6, unbuilt] Cache the last
    target PC per site: predicted hit = one compare + direct `return_call`.
    Critical for interpreter dispatch loops (CPython, Lua) and every
    PLT-mediated call.
23. **8-way `br_table` on every indirect branch.** Even a monomorphic site
    pays table selection. A per-site cached funcref in a wasm global skips it.
24. **No cross-module direct linking.** `jump_to_constant` uses `return_call`
    only for blocks in the same module; everything else goes through the table
    forever, even once both sides are translated. Wasm can import functions
    across instances.
25. **No block coalescing for straight-line fallthrough.** Every non-loop edge
    costs a full flush of dirty registers; the code's own figure for register
    traffic is 19–20% of emitted code, and regions fix it only for leaf loops.
26. **No tail duplication / trace formation.** No Dynamo/HQEMU-style hot-path
    superblocks with side exits — the general answer to the above and to
    irreducible CFGs.
27. **No guest-function inlining.** Nothing is inlined, ever. Inlining leaf
    callees would unlock regions across calls (currently a hard disqualifier)
    plus everything in §B.
28. **Safepoint counter lives in ctx memory.** Every backward branch and every
    indirect branch does load/sub/store. A wasm global removes the memory
    traffic; the fused-loop form already does this with a local — generalize.
29. **PC materialized more often than needed** — stored before every non-local
    jump and every host call, including sites that cannot observe it.

## E. Loops and regions

30. **Regions are leaf-only, ≤8 blocks, no nesting, single entry.** A call
    ends the region, which excludes nearly every hot loop that is not a
    memcpy. Mature systems form regions over loop nests with side exits.
31. **No LICM.** Address bases, guard computations, constant loads and
    reloaded globals stay in the loop body.
32. **No loop unrolling or peeling**, so no amortization of the safepoint
    check or the back-edge dispatch.
33. **No induction-variable analysis** — the prerequisite for both, and for
    proving in-range addresses (which kills the guard, §16).
34. **Region exit cost is unmitigated:** every exit stores all written
    registers because dirtiness is emission-order-dependent. Path-sensitive
    dirty tracking, or a merge block per exit, restores that.
35. **No OSR into a region.** A loop already running in tier-1 blocks cannot be
    promoted mid-flight; it must be re-entered.

## F. Wasm-level lowering

36. **No multi-value usage.** Block signatures could keep values on the
    operand stack across `if`/`block` instead of round-tripping through
    locals.
37. **`call_ref` / typed function references not used** — would drop the table
    bounds check and the runtime signature check on every indirect branch.
38. **No stack-scheduling pass.** Values are pushed into locals eagerly; a
    scheduler would keep more results on the stack (`local.tee` chains,
    operand reordering for commutative ops) and shrink both module size and
    V8's register pressure.
39. **`br_if` vs `if`+`br`.** The terminator emits `if_void` blocks where a
    `br_if` is shorter and gives V8 a better shape.
40. **No constant pooling.** Repeated 64-bit constants (page bases, masks) are
    re-encoded inline instead of held in globals or a data segment.
41. **Alignment hints are natural-only.** Where a base is provably 16-aligned
    (stack frames), stronger hints help V8 and Cranelift.

## G. SIMD / FP

42. **No relaxed-SIMD.** [PLAN §5.7 says "used opportunistically"; absent]
    `relaxed_madd` on a host with a real FMA would replace the software FMA —
    BASELINE.md measures the FMA path costing **6×** on LU decomposition.
    Also `relaxed_laneselect`, `relaxed_swizzle`, `relaxed_min/max`,
    `relaxed_trunc`.
43. **No multi-instruction NEON fusion.** Per-instruction mapping only:
    `mul`+`add` lane pairs, `shift`+`narrow` (`rshrn`), widening MACs →
    `i32x4.dot_i16x8_s`, `q15mulr_sat`.
44. **No FPCR/rounding-mode specialization** beyond the default path (strict
    mode from §5.7 unbuilt).
45. **No vector-specific region cost model** — a v128 flush is 16 bytes and is
    treated identically to a GPR in the ratio that gates region formation.

## H. Runtime/JIT policy

46. **Single compilation tier.** Every block is compiled once at tier-1
    quality forever. No execution counters, no hot-block re-optimization, no
    run-time promotion of a hot block into a region — region formation only
    sees blocks that happened to land in the same translation unit.
47. **No profile collection at run time** to drive any of the above.
48. **Translation is synchronous on the miss path.** No background or parallel
    translation on another Worker, no `compileStreaming`, no speculative
    translation of a function's successors ahead of the guest.
49. **No persistent code cache in the browser.** `gex-cache` is wasmtime-only
    (`Module::deserialize`); the browser re-translates and re-compiles
    everything on every page load. V8 exposes wasm code caching via
    IndexedDB — probably the biggest startup win available, and not codegen.
50. **No compile-granularity policy** on the miss path — no measured choice
    between "translate the enclosing function" and "translate one block".

---

# Part 2 — The static recompiler (whole-program, AOT only)

## A. Program recovery

51. **No function recovery.** [PLAN §5.5, unbuilt] No `.eh_frame`/unwind-range
    or symbol-driven function boundaries, so there is no function-level IR to
    optimize over. Everything below depends on it.
52. **No CFG structurizer / relooper.** [PLAN §5.5, unbuilt] Consequence: V8
    and Cranelift see essentially no loops outside the small region case. This
    is the difference between a template JIT and a compiler.
53. **No jump-table recovery.** Discovery is recursive descent + pointer scan +
    `--exhaustive`. Recognizing the `adr`/`ldrsw`/`add`/`br` switch idiom
    matters less for coverage (runtime translation covers that) than for
    precision: with complete successor sets, cross-block flag liveness and
    register liveness stop bailing to "assume everything live".
54. **No relocation-typed data scan.** The pointer scan is untyped;
    `.rela.dyn`/`.rela.plt` and vtable-shaped data give a much more precise
    function-pointer set with no false positives.
55. **No call-graph construction**, hence no ordering, no leaf detection, no
    recursion detection.
56. **No exhaustive-mode reachability pruning.** `--exhaustive` on nbench emits
    12.3× the guest's text; reachability over a recovered call graph would cut
    module size and engine compile time.

## B. Interprocedural

57. **No cross-call register liveness of the useful kind.** Whole-graph GPR
    dead-store elimination is already recorded as a dud (44 statements in
    27,614) — do not redo it. The useful IPA is different: a callee summary
    saying it never touches `v0`–`v31` lets a caller keep them in locals
    across the call instead of flushing. That is the 19–20% traffic figure.
58. **No inlining decisions at AOT** (see §27), where the call graph and the
    compile budget both exist.
59. **No PLT prebinding / devirtualization.** [PLAN §6.4, unbuilt] Every
    cross-DSO call still goes GOT → indirect at run time even though ld.so's
    resolution is a fixed point long before the hot loop.
60. **No specialization on settled GOT/relro state.** Runtime translation
    constant-folds through initialized memory; the AOT path could re-recompile
    hot objects after relocation and fold the resolved pointers.
61. **No constant propagation of read-only data** into the static path the way
    `const_fold_with_memory` does at run time — `.rodata` is fully known at
    build time.
62. **No purity/effects summaries** to allow reordering across calls.

## C. Whole-program codegen

63. **No profile-guided recompilation.** No path from a run's hot-block profile
    back into `gexc build`: region selection, inlining, block ordering and
    which objects ship AOT are all static heuristics (`GEX_REGION_RATIO`) with
    no dynamic input. Every mature AOT translator is profile-driven.
64. **No region cost model worth the name** — edges per live register, with the
    real inputs (trip count, exit frequency) absent because there is no
    profile.
65. **No block/function deduplication.** PLT stubs, identical prologues and
    thunk bodies repeat thousands of times across a large object; function-level
    CSE is straightforward module-size savings.
66. **No cold/hot splitting.** Modules split by a fixed `MAX_FUNCS_PER_MODULE`,
    not by expected execution, so cold code inflates the module the browser
    must compile before the guest runs.
67. **No code layout for locality or compile order** within a module.
68. **No lazy per-function module fetch** — everything an object needs ships in
    one bundle.
69. **No unreachable-block GC** — linker-style dead-code elimination over the
    recovered call graph.
70. **No cross-object optimization** — each object is translated in isolation:
    no shared constants, no shared thunks, no whole-rootfs view.

## D. Not a compiler optimization at all

71. **No library-call substitution.** box64 and FEX get much of their speed by
    not translating libc/zlib/OpenSSL at all — they bind the guest's calls to
    native implementations. The symbol names are already known (`.dynsym` is
    used for seeds). Replacing guest `memcpy`/`memmove`/`strlen`/`memset`/
    `malloc` with host wasm implementations would beat any codegen change on
    the workloads where gex trails QEMU, and it is independent of everything
    above.

---

# Already measured as not worth redoing

Recorded in the code so this list is not read as 71 open opportunities.

* **Registers in block parameters** — tried, reverted: tcc 27% slower, Lua and
  SQLite unchanged, modules 6% larger (`tier1.rs::flush`).
* **Natural-loop region growth** — 7–11× more coverage, measurably slower
  (Lua −11%) (`region.rs`).
* **Region member stubs / spanning calls** — works, costs (zstd −5%).
* **Nested loops in regions** — 16× coverage, −34%.
* **Whole-graph GPR dead-store elimination** — 44 statements out of 27,614
  (`xlate::eliminate_dead_flags` doc).
* **`--no-liftoff` (forcing V8's optimizing tier)** — bought nothing, so
  "fewer, bigger functions" is not by itself the prize.
* **Caching the bounds guard per base value** (§17). The offset already
  rides along as the access's static offset, so what gets guarded is the
  base, and an SSA base guarded twice emits identical arithmetic twice.
  Caching it in a local removed 18% of gzip's guard bytes (523,872 →
  427,907) and shrank nbench's module 0.9%. It moved no kernel: over two
  clean runs, NUMERIC SORT −4.5%, STRING SORT −1.0%, FOURIER −5.5%, IDEA
  +1.0%.

  The reason generalises, which is why it is here rather than in a commit
  message. The guard is pure arithmetic on a value that does not change --
  precisely what an optimiser removes unaided -- so the bytes deleted are
  bytes V8 had already collapsed, while the extra local is real. **Fewer
  emitted instructions is not less work once the engine has its own CSE.**
  Anything on this list that only removes redundancy should be expected to
  measure at zero; the items worth doing are the ones that remove
  computation nothing downstream can recover, as `cmp`+`b.cond` fusion
  removes the materialising of N and V.

---

# What a block boundary actually costs, measured

Added 2026-08-09. `gexc build` now reports this, and did not before: the
counters behind it were `thread_local!` and the blocks are compiled on a
pool, so every count landed on a worker thread and the main thread printed
its own, which was zero. The "19-20% register traffic" quoted throughout
this file came from somewhere else; this is the first time the build has
measured it.

    gzip     18% registers,  7% safepoints,  5% dispatch  (31% of the module)
    nbench   16% registers,  5% safepoints,  4% dispatch  (25%)
    bash     18% registers,  9% safepoints,  7% dispatch  (34%)

**This reorders the list below.** A return-address stack and inline caches
(sections 21-23) are ranked second here and attack the 4-7%. The 16-18% is
the flush, and the mechanism that already avoids it -- regions -- covers 149
of gzip's 15,338 blocks, 366 of nbench's 27,594 and 1,155 of bash's 211,979.
About one percent, and `region.rs` records four separate attempts at more
coverage that all measured worse.

Bytes are not seconds. A dispatch byte carries an indirect call and a
directory load and may cost more at run time than a `local.set`. But 4-7% of
emitted code is a ceiling on what deleting dispatch entirely could save.

`GEX_REG_HISTOGRAM` works now too, and says what the 18% is made of on gzip:

    X0 12.8%  X1 9.0%  X2 6.7%  X30 5.8%  Sp 5.0%  X19 4.6%  X3 4.4%
    Z 4.3%    C 3.4%   N 3.1%   V 3.1%

The four condition flags together are **13.9%** -- more than any register but
X0, and that is *after* the `cmp`+`b.cond` fusion, which removes the reads
but not the writes. They are four separate `i32` slots in the context,
stored and loaded one at a time, which is section 5 below. Packing them is a
representation change with no ABI question in it, and it is the largest
single item this measurement exposes.

## Packing the flags: built, and the runtime effect is not readable here

Added 2026-08-09. The four condition flags are one slot with a byte each now,
and a flush that has all four packs them into one `i32.store` instead of
four. Correctness is settled -- 300 differential-fuzz cases against QEMU with
none differing, gzip byte-identical over 13 MB, 114 unit tests -- and the
boundary bytes fall about 1.5% on gzip, bash and nbench alike.

The runtime measurement failed, and it is worth saying how rather than
quoting the number it produced:

    NUMERIC SORT   475.10 -> 482.63   +1.6%
    STRING SORT    236.42 -> 241.05   +2.0%
    IDEA          4018.7  -> 3835.5   -4.6%
    FOURIER      13864    -> 15406   +11.1%   (control)

The control moved more than anything else. FOURIER is floating point and
hardly touches NZCV, so an 11% gain there is not flag packing; it is the
machine, which has a VM on it that comes and goes. `nbench-ab.cfg` says
exactly this -- "an effect that appears in all four is the machine" -- and
the case where the *control alone* moves most is the same warning louder.

So: no readable runtime effect, on a machine that cannot currently resolve
one this size. The change stands on the traffic argument rather than on a
measurement, which is weaker than this file's usual standard and is flagged
here as such. Re-run it on a quiet machine, interleaving the two sides
within one run rather than one after the other, before believing any number.

## Register liveness across block edges: worked out, not built

Added 2026-08-09. The flush is the largest measured category -- 17% of gzip's
emitted bytes, 17% of bash's, 18% of perl's -- and it is in the one bucket
this file says actually pays. The obvious attack is a liveness fixpoint over
the object's whole CFG: at a direct branch to a known block, flush only the
registers that block or its successors read. Ahead-of-time translation is
what makes it possible, so it looks like the flagship optimisation.

It is worth much less than it looks, for two reasons, and both are visible
without writing it.

**Every `ret` is an unknown target.** Liveness is a backward analysis, so a
block whose successor is unknown has `live_out = ALL`, and that floods
backwards through the function. gzip has 138 `ret`s and 85 `br`s among 3286
blocks -- roughly one indirect terminator per fourteen -- so almost no block
is far enough from one to keep a small live set. Bounding `live_out` at a
`ret` means asserting AAPCS: caller-saved registers are dead across a return.
That is false for `ret`-as-indirect-jump, which is a real idiom in resolvers
and trampolines, and this project has already been bitten once by assuming a
computed branch behaves like a structured one.

**The flush is already minimal.** It writes *dirty* registers only -- ones
this block actually wrote. A basic block writes two to four. So liveness
cannot remove a flush that a block needed; it can only remove a **dead
store**, where a block computes a register that nothing downstream reads.
Those exist but they are a minority of two-to-four.

So the 17% is not 17% of removable work. It is 3286 blocks each storing the
handful of registers they genuinely changed, which is close to the floor for
a design where a block boundary is a function boundary.

What would actually move it is not liveness but making fewer boundaries:
regions already do that and cover about 1% of blocks, and `region.rs` records
four attempts at more coverage that all measured worse. That is the honest
place to push, and it is a harder problem than dataflow.

## What perl actually spends its back edges on

Added 2026-08-09, with `GEX_HOT=1` on statically recompiled perl running a
40,000-key hash build, a sort with a Perl comparator and a regex over 12,000
words. The twelve hottest back edges:

    2593  [image] +0x10bb28          perl's own text
    1876  [image] +0x10bb30            "  (the adjacent block)
    1171  libc +0x95f48              memcmp / bcmp / __memcmpeq
    1160  [image] +0x1065c0
    1158  libc +0x95f68              memcmp, 0x20 further in
    1154  [image] +0x4a610
    ...   nine more, all [image], 1130-1153

Two things to take from it, and one caution.

**The only library code in the list is `memcmp`.** Not `memcpy` -- that is an
intrinsic already and no longer appears -- and not `strlen` or `malloc`. perl
compares hash keys, and glibc's aarch64 `memcmp` is a hand-written SIMD loop
whose every iteration becomes a block and a dispatch here. That is the same
argument that made `memcpy` an intrinsic, and section 71 is where it belongs.

It is *not* the same fix. `memcpy` collapses to `memory.copy`, one wasm
instruction; wasm has no `memcmp`. Serving it means either emitting a loop --
which `intrinsic::block` cannot express today, since it builds a single
straight-line block ending in `ret` -- or a host call, which on the short keys
a hash table compares may cost more than the loop it replaces. Either way the
replacement has to be measured against what it replaces.

**Everything else is perl's own text**, spread over a dozen sites within a few
hundred KiB of each other: an interpreter dispatch loop and the functions it
reaches. That is the honest answer to "where does the time go" for this class
of program -- nowhere in particular, which is why no single change has moved
it.

**The caution.** These counts come from a table of 4096 slots that *ages* on
collision: a loser decrements the winner rather than taking the slot. The
ranking is meaningful and the magnitudes are not. A flat cluster at 1130-1170
is what aging produces when several addresses share slots, not twelve equally
hot blocks. Read it as "these are the hot addresses", never as "this one is
2.2x that one".

# The test an item has to pass

Added 2026-08-09, after one item on this list measured at exactly zero for a
reason that applies to several others.

Caching the bounds guard per base (section 17) removed 18% of gzip's guard
bytes and moved no kernel at all. The guard is pure arithmetic on a value
that does not change, so **V8's own optimiser had already removed the
duplicate** -- the emitted bytes were there, the work was not.

So emitted bytes are not a cost model. Before building anything here, ask
which of these it is:

* **Redundancy an optimiser can see** -- a repeated pure computation, a mask
  over a value already known to be masked, a `wrap`/`extend` round trip.
  Expect zero. V8 does this already, and deleting it from the emitter only
  makes the module smaller. Sections 6, 7, 8, 10, 17 and 20 are mostly this.
* **Computation nothing downstream can recover** -- materialising N and Z and
  C and V so that a branch can compare two of them, when the branch could
  have asked about the operands. That is why `cmp`+`b.cond` fusion is worth
  16-29% and this was worth nothing.
* **Memory traffic an optimiser must keep** -- the register flush to the
  context, the safepoint counter's load and store. V8 cannot prove the host
  will not read those bytes, so it cannot elide them however redundant they
  look. This is where the remaining real cost is.

The measured split -- 16-18% registers, 5-9% safepoints, 4-7% dispatch,
12-15% bounds guards -- is bytes. Read against the test above, the register
flush and the safepoint are real, the duplicate half of the guards is not,
and the per-base half of the guards is.

## The bounds guard is the biggest single item here, measured

`GEX_UNSAFE_NO_GUARD=1` builds without the guard at all -- not shippable,
since a guest that computes an address above `GUEST_LIMIT` then aliases low
memory instead of taking SIGSEGV. It exists to price the removal, because
the principled way to do it needs a custom Rust target and nobody should
start that blind.

nbench, statically recompiled both ways, two runs each:

    STRING SORT   240.21 -> 391.33  +62.9%
    IDEA         3828.4  -> 4154.7   +8.5%
    NUMERIC SORT  488.73 -> 525.98   +7.6%
    FOURIER     15861    -> 16128    +1.7%

three runs each, and the module falls from 7673 KiB to 6820 KiB.

What the guard protects is less than it looks, which matters when weighing
that number. `GUEST_MEM_BYTES` is `0xFFFF_0000` and the wasm memory covers
all of it, so a wild guest pointer *below* 4 GiB does not fault today
either -- it reads zeros from a part of the flat mapping nothing mapped.
The guard catches only addresses at or above `0xFFFF_0000`, meaning those
with high bits set. It buys detection of one class of wild pointer, while
the other already goes undetected, for 62% on string code. The host is safe
either way: every access stays inside the wasm memory.

The spread is the mechanism showing: STRING SORT chases pointers and bytes,
so almost every instruction is a guarded access, while FOURIER strides
through doubles and does far less addressing per unit of arithmetic. Real
programs that move strings and structures -- apt parsing an index, a shell,
a compiler -- are at the STRING SORT end.

**Nothing else on this list is close.** It also passes the test above:
every access has a different address, so this is not redundancy V8 can
collapse -- it must emit all eight instructions, every time.

## apt has never been statically recompiled, and when it is, it is 2.3x slower

Added 2026-08-09, after a question that should have been asked months ago:
has apt's four minutes ever been measured with the feature this project is
for?

It has not. `gexc build <elf>` translates the *image* and follows no
DT_NEEDED, and `apt-get` is a 67 KB wrapper:

    apt.wasm: 212 blocks, 914 guest instructions, 51 KiB wasm

All of apt is in `libapt-pkg.so`, 1.9 MB, which that build never sees. perl
only looked like a whole-program build because Debian links libperl into the
3.7 MB `/usr/bin/perl`. So every apt number in this file, including today's
3min 42s, is the runtime translator end to end.

The static path for a library is `GEX_RECOMPILE_OBJECTS`, which translates a
whole object once it proves hot rather than a block per dispatch miss. With
it, at the usual 8 MB cap:

    runtime translation      Fetched 14.9 MB in 3min 13s   wall 222s
    whole-object recompile   Fetched 14.9 MB in 7min 54s   wall 520s

2.34x slower, and the slowdown is in the fetch phase, which is where apt
decompresses and hashes.

Three candidate reasons, none yet separated:

  * **Translating code that never runs.** The cap recompiles every object
    under 8 MB of text -- libapt-pkg, libstdc++, libc, libgcrypt, liblzma,
    libzstd, libsystemd -- and apt update executes a small fraction of any of
    them. A dispatch miss translates exactly what is reached.
  * **Per process.** apt forks a helper to fetch and more to verify, and each
    child maps the same libraries and pays the same whole-object translation
    again. Section 54's cache is per store.
  * **The module, not the code.** Thirty megabytes of extra wasm per process
    is V8 compile time and instruction cache, which is the same effect that
    made the N=256 dispatch ballast cost 6% while executing nothing.

This does not say static recompilation is wrong. It says whole-object
recompilation, triggered by a hotness threshold on the *object*, is the wrong
granularity for a short-lived fork-heavy program: it pays for a library and
uses a function. The measurement to make next is how much of each object apt
actually reaches -- if it is a few per cent, the answer is per-function
recompilation seeded from what the profile already knows, not a bigger cap.

## Reusing a translation across `execve`: built, does not fire

Added 2026-08-09. `apt update` lifts two million blocks across 112 processes
for a closure of 348,056 -- the same code 5.7 times over, 55.3s of a 224s run
(commit 0b0194a). The obvious cause is `execve`: the address space is
replaced, the dispatch directory is cleared and `page_table` is reset, but
the Worker is the same Worker and its tables still hold every function it
installed. `shared` already knows which slots a page's code went into. So the
miss path ought to be able to restore the directory entry and send the branch
round again instead of lifting into slots that already contain the code.

Built. An `installed` set of `(page, base)` describing the *tables* rather
than the program, not cleared by `execve`, seeded when a translation
publishes and again in `gex_adopt_page` so a forked child inherits its
parent's. A `shortcut` set to take the path at most once per page, since a
page whose slots are installed but whose particular block was never
translated would otherwise miss and shortcut forever.

    before   112 processes, 55.3s lifting, 2,000,204 blocks, 224s wall
    after    112 processes, 55.3s lifting, 1,946,469 blocks, 220s wall

2.7% of the blocks and a wall difference inside this machine's noise.
Correct -- the substitution suite passes all six including the twenty-process
loop, and apt completes with `Fetched 14.9 MB in 3min 11s` -- so it is not
broken, it simply almost never applies. Reverted.

**Counted, and the suspect was wrong.** `gex_reuse_stat` reports per process
how the lookup went. On a shell loop that execs `ls` and `wc` once an
iteration -- apt's shape in seconds rather than four minutes:

    pid 3:  20868 blocks   reuse have=445 hit=0 miss=13 noident=2
    pid 6:  20868 blocks   reuse have=445 hit=0 miss=13 noident=2
    pid 9:  20868 blocks   reuse have=445 hit=0 miss=13 noident=2

`have` dwarfs everything: 445 of 460 misses are on a page this process
already owns slots for, which are new blocks in a known page and not
re-translation at all. The shortcut sat behind `lookup(page).is_none()` and
was never reached -- which is why it moved 2.7%.

And `hit=0` on the case that should hit. The third `ls` lifts exactly the
20,868 blocks the first `ls` lifted: same program, same libc, same addresses.

The cause is scope, not hashing. `Jit::shared` is documented as being "for
the whole tab rather than for one process" and it is not -- it is a field of
`Jit` in each Worker's own memory, and `#sharedMap()` is called in exactly
one place, the fork record. It travels parent to child at `fork` and nowhere
else, so a child's translations never reach its parent and never reach the
next sibling. Each `ls` starts from dash's snapshot, and dash has never run
`ls`.

So the waste is siblings unable to reuse each other's work, not a child
failing to reuse its parent's. The mechanism is right and the scope of one
map is wrong.

**Half of that is now done.** The identity map moved into the global kernel
-- `k_slot_get` / `k_slot_put`, a `static` in the memory every Worker
instantiates against, which is where the process table and the filesystem
already live. `miss` goes to zero and `hit` to twelve on the loop above, and
stays there across iterations.

It does not save the lifting on its own, and that was expected: a hit gives
the *slots*, and a dispatch table belongs to a module instance, so a Worker
that has not instantiated the module cannot reach the code however well it
knows the slot number. Blocks moved 0.4%.

**The other half was built and reverted.** Put the module bytes in the kernel
too, and on a miss whose identity the kernel knows, copy them out and push
them onto the shim's own `pending` queue -- the queue a fresh translation
already uses, so JavaScript installs them by the path that exists and no new
host plumbing is needed. It hangs, for a reason worth writing down:
`PendingModule` carries the size the dispatch table must be grown to, a
*foreign* module was built against another process's high-water mark, and
passing the local one understates it. The table is not grown far enough, the
install throws inside the drain loop, and the run wedges.

That fix was then made -- the slot count stored with the module and read back
through `k_mod_slots`, the module's `dir_entries` stored and replayed into
`page_table` *before* the install so its blocks resolve to real slots instead
of being silently dropped -- **and it still hangs**, in the same place: the
second `ls` of the loop, the first process to serve a module rather than lift
one. So the diagnosis was incomplete, and this is an open question rather
than a known bug with a known fix.

Ruled out by the two attempts: the slot count (fixed, no change); the blocks
failing to resolve (dirs replayed first, no change); and the kernel's `Mutex`
being unsound across Workers, which was the next suspect and is not it --
`ProcTable` holds a `Mutex<Table>` in exactly the same way and every `fork`
in the suite goes through it.

Left, in the order worth trying. Whether an early `return true` from inside
the `with_host` closure reaches the same `trap()` the normal path falls
through to. Whether pushing a `PendingModule` onto a queue that already has
one confuses `publish_pending`, which publishes the *front* rather than the
entry just added. And whether replaying another process's `dirs` reassigns a
page this one had already placed elsewhere. One trace line in the serve path
separates those in a single run. See #95.

**And a control worth keeping.** Per-process lifting was unchanged and wall
clock did not move either. That says the 55.3s is not on the critical path:
those processes lift concurrently while apt waits on the network. Summing
per-process translation time and subtracting it from wall clock -- which is
how "a closure build is worth at least 25%" was derived one commit earlier --
is not valid when the processes run in parallel. The 55.3s is CPU, and CPU
matters in a tab with one core to share; it is not four minutes becoming
three.

## The indirect-dispatch path is not a cost, and sections 21 and 23 are dead

Added 2026-08-09. Sections 21 and 22 sit at number two on the ranked list --
"the highest-value dispatch change available", on the reasoning that `ret`
takes the full indirect path and `ret` is a large share of indirect branches.
That reasoning counts instructions, which is the mistake this file has now
made three times. So it was priced first, and it is worth nothing.

`GEX_DISPATCH_BALLAST=N` adds N load-and-store pairs to every indirect
dispatch: a load from the dispatch directory beside the entry the path is
about to read -- the same cache line, so the cheapest extra load there is and
therefore a lower bound -- and a store into `ctx::BALLAST`, a slot nothing
reads. The store is what makes it survive: an engine may delete a load whose
value goes nowhere and may not delete a store, because it cannot prove nobody
reads the address.

perl, statically recompiled, `GEX_TIME_GUEST` around `gex.run`:

    N=4    9 pairs   median ratio 1.133   4 of 7 favour A   p = 1.000
    N=16   9 pairs   median ratio 0.897   5 of 9 favour B   p = 1.000

Both unreadable, and -- more informative than either -- *four times the added
work moved the estimate the other way*. That is what a perturbation below the
noise floor looks like.

So a positive control, to be sure the ballast executes at all. N=256 doubles
the module, 17.8 MB to 37.5 MB, which proves it is emitted:

    plain 6077ms   ballast256 7460ms
    plain 6434ms   ballast256 6582ms
    plain 6539ms   ballast256 6933ms      median about +6%

**Five hundred and twelve memory operations on every indirect dispatch cost
six per cent.** The real dispatch path is one directory load and about
eighteen ALU operations -- three pair-units, generously. Removing all of it,
on every indirect branch, is worth something like 0.1%, and that is an
overestimate: at N=256 the module is twice the size, so part of the 6% is
instruction cache and V8's compiler rather than executed work.

Why so cheap, when perl is an interpreter and interpreters dispatch? Because
most of perl's control flow never reaches this path. A branch to a constant
target inside the same module is a `return_call` (`jump_to_constant`), a loop
is fused, and a region branches among its members with a `br_table` over
locals. `emit_dispatch_tail_call` is left with genuinely computed branches
and cross-module calls, and those are rarer than the story suggests.

That kills sections 21, 22 and 23 as performance work, and section 24 and
section 77 with them: they all make the same path cheaper. It does not kill
the *correctness* reasons to want them.

The instrument generalises, and that is the part worth keeping. Any path can
be priced before it is optimised, by adding work an optimiser may not remove
and measuring the slope. Do that first, every time. Three items on this list
have now been built or half-built before being priced -- guard caching,
memory64, and nearly this -- and all three were worth nothing.

## memory64: built, measured, and deleted

Section 16(d) proposed a 64-bit guest memory so the engine's own bounds
check subsumes ours, and priced it at the 62% above. The whole thing was
built -- a `wasm64-unknown-unknown` shim, `i64` addressing in the emitter,
the 0x04 limits flag, a 64-bit `WebAssembly.Memory` in the glue -- and gzip
1.12 ran through ld.so to a byte-identical result on it. Bounds guards went
from 13% of the emitted module to 0%, and the module from 793 to 712 KiB.

It is not faster. gzip -6 over 13 MB, six runs a side, alternating:

    guarded wasm32     9563  9609  9840 10168 10206 11048   median 10004 ms
    memory64           9027  9060  9469  9645 10916 11363   median  9557 ms
    wasm32, no guard   9925 10017 11441                     median 10017 ms

The third row is the control and it is the answer: removing the guard
*entirely*, unsafely, with nothing in its place, changes nothing on gzip.
There was no 62% to collect, and memory64's 4% is inside the spread.

nbench, the benchmark the 62.9% came from, with FOURIER as the
floating-point control:

    NUMERIC SORT   475.10 -> 395.57   -16.7%
    STRING SORT    236.42 -> 322.59   +36.4%
    IDEA          4018.7  -> 2917.4   -27.4%
    FOURIER      13864    -> 14165     +2.2%   (control)

The control moved 2.2%, so the rest are real -- and they disagree with each
other. Two effects run in opposite directions: our guard goes away, which is
worth a lot exactly where V8 could not already see through it; and V8's own
bounds check arrives, and it is *not* free the way the 32-bit one is. A
wasm32 memory gets a 4 GiB virtual reservation and the checks become guard
pages costing nothing. A 64-bit memory does not, so every access carries a
real compare, and the kernels whose addresses V8 had already proven in range
pay it for nothing.

perl was supposed to be the confirming case, being the pointer-chasing
program par excellence, and it is not. Fourteen interleaved pairs:

    11 of 14 pairs favour wasm32, p = 0.057 by sign test
    median per-pair ratio: memory64 is 13% *slower*

Suggestive rather than established at p = 0.057, and if anything it
understates the penalty: those pairs ran A first every time, and first place
turned out to be a disadvantage worth about 29% -- see the ordering bias
`tests/ab.sh` now corrects for -- so memory64 had the easier slot and still
lost.

**One kernel out of five programs gains.** NUMERIC SORT -16.7%, IDEA -27.4%,
perl -13%, gzip nothing, STRING SORT +36.4%. That is an exception waiting for
an explanation, not a rule, and the code is gone rather than left behind a
flag: a second address width in the emitter, the shim, the glue and the
kernel is a standing cost paid by everyone who reads them, in exchange for an
option nobody should exercise. The measurement is what was worth keeping.

Two things it was unambiguously worth, if either ever becomes the point:
modules were 11-14% smaller, and a wild pointer below 4 GiB traps under
memory64 where today it reads zeros from the flat mapping unnoticed.

And the scoping this puts on the 62.9%: that figure is nbench STRING SORT,
which chases pointers. gzip indexes arrays with induction variables. The
guard is pure arithmetic on the address V8 is about to use anyway, so when
V8 can see the range it folds the guard into the load and the bytes cost
nothing; when it cannot -- an address loaded from memory, as in a linked
list -- the guard survives to run time and costs what it costs. Which is
this file's own cost model: *redundancy an optimiser can see is worth
nothing to remove*. The expected value of removing the guard is whatever
fraction of the program chases pointers, not 62%.

# Ranked by expected value

1. **`cmp`+`b.cond` fusion** (§1–4) — largest pure-codegen win, touches every
   block.
2. ~~**Return-address stack + indirect inline caches** (§21–22)~~ — **dead.**
   Priced with `GEX_DISPATCH_BALLAST`: 512 memory operations on every indirect
   dispatch cost 6% of perl, so removing the whole path is worth about 0.1%.
   §23, §24 and §77 die with it.
3. **Browser code cache in IndexedDB** (§49) — not codegen, likely the largest
   user-visible improvement.
4. **Library-call substitution** (§71) — beats codegen on exactly the kernels
   where gex trails QEMU.
5. **Redundant-load elimination + stack-slot promotion** (§13, §15) — attacks
   the spill traffic regions only partly fix.
6. **Guard elision via range facts** (§16) — five instructions off every
   memory access. Range facts only: `memory64` was built and measured and
   lost on four programs out of five, and has been deleted.

## Deterministic object placement: built, and it is a prerequisite rather than a win

A sibling can now install the module a sibling translated, and on the two-`ls`
loop it serves five modules in the first round and sixteen in the second. It
saves almost no work, and the reason is in the addresses.

`ld.so` reserves each object with `mmap(NULL, span)` and takes whatever it is
handed. The arena hands out addresses in the order things are asked for, and
no two processes ask in the same order, so the same libc landed at
`0x6010000` in one process and `0x6060000` in the next. A translated block has
its guest addresses baked into it, so those are two different translations of
the same bytes with nothing in common -- different dispatch pages, different
identities, no sharing possible even in principle.

So the placement is now gex's to make rather than the arena's, and it makes it
once: `k_place` in the global kernel maps a hash of the guest path to a base,
first asker fixes it, everyone after is handed the same. The signal is the
`openat` immediately before the reservation -- the same one the closure pins
already used, because an anonymous `PROT_NONE` mapping carries no file to key
on -- and it is armed only for a name that looks like a shared object, since
the executable's own base is fixed by the loader rather than reserved.

Measured: libc lands at `0x5000000` in every process of the run, which is what
was wanted.

The cost is a reserved window, and it could not be avoided. A process's memory
grows to the highest address it touches, and the runtime's own heap takes from
the same monotonic cursor the guest arena does -- so an address the placement
table hands out later can be one the heap has already used, and the first
attempt zeroed the shim's own allocations and trapped inside `mmap`. Nothing
learns the right size during a run and nothing can take it back afterwards, so
`ARENA_BOTTOM` sits above a fixed 64 MiB window and every process pays all of
it. Measured at 104 MiB per process before, 168 MiB after. The true cost is
smaller than that looks -- the libraries in the window are ones each process
was going to map anyway -- but it is not nothing, and it buys nothing until
the sharing above it can actually use it.

## Why the sharing still cannot use it: the module fixes its own slots

The obvious next step is to stop serving a page only its *first* module. A
page's code is spread over as many modules as it took misses to translate it,
the block cap sees to that, and the first is a small part of the whole; that
is why `blocks` barely moves while `serve` fires sixteen times.

It does not work, and the reason is `Linkage::Shared`'s element segments. A
runtime-translated module used to export every block by name and let the host
place them one at a time, which cost half a minute per shared library, so it
now fills the imported table itself with active element segments whose offsets
were fixed when it was built. That makes the producer's slot assignment part
of the module rather than a hint: installing it writes *those* slots whatever
the installing process would have preferred.

Three attempts, all measured, none kept:

* Adopt the producer's bases for every page it touches. Segfaults: the base
  that names one process's libc page is the base another gave to something
  else, and the segment writes the wrong functions over it.
* Same, but decline any module whose bases collide with pages already placed
  here -- and store *every* page the module holds blocks for, not just the
  ones it was first to claim, since the fresh ones do not show the collision.
  Still segfaults, then wedges.
* Ignore the producer's bases and allocate locally. `WebAssembly.instantiate`
  refuses it outright: the element-segment offsets are past the end of a table
  grown to this process's high-water mark.

The way to earn this is to make the two processes agree by construction rather
than by luck: one system-wide dispatch-slot allocator, keyed by the page
identity that already names a translation, instead of one counter per process.
Then every process gives the same object page the same base, every module's
segments land where they were built to land, and the whole collision class
stops existing. That is a change to `PageTable::base` and a counter in the
global kernel, not to any of the three things above. See #95.

## A whole closure, statically recompiled, and a run that translates nothing

Built and landed. `gexc build --closure` translated a program's transitive
`DT_NEEDED` closure before this session, but nothing could instantiate the
result, so the modules had never been run. Now they are.

Three things had to agree before a library module could be correct, and all
three were wrong:

**The address.** A module has its object's guest addresses baked in, so the
object has to be *there*. `gexc build --closure` writes a `.pins` manifest,
`gex_pin_object` seeds it before the loader runs, and `ld.so` cooperates
without knowing it, because `mmap(NULL, span)` takes whatever it is handed.
The pin also goes into the global placement table, so a `fork` and an
`execve` place the library identically rather than only the first process
being right. `gex_pin_report` says where each one landed, because a pin that
silently does not take means the guest branches into whatever is at the
address the module was built for.

**The interpreter.** `ld.so` is not placed by `mmap` at all -- gex's own
loader puts it at `INTERP_BASE` -- so it cannot be pinned, and the build was
placing it in the mmap arena instead. The build now places it where the
loader will. Two constants had drifted apart as well: `gexc` had its own
`INTERP_BASE` of `0x2800_0000`, which is *inside* the arena, against the
abi's `0x0300_0000`. They are one constant now. Matching by `PT_INTERP` path
was not enough to notice: dash's is `/lib/ld-linux-aarch64.so.1` and the same
object arrives again through libc's `DT_NEEDED` as
`/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1`, so the comparison matched
neither and the interpreter was translated for an address it never occupies.

**The slots.** Each object was translated by its own `translate_image` call,
and each call started a fresh `PageTable` -- so every module in the closure
claimed dispatch slots from the same place. libc's blocks and the
executable's blocks were in the same slots, and the program branched into
whichever module had been installed last. `Options::first_slot` threads the
high-water through, so the closure is allocated as if by one allocator.

Measured, `dash -c 'echo hello'` on the small rootfs, blocks translated *at
runtime*:

    seed module, no closure                   15,816 blocks   594 ms
    closure's own image module alone          15,708 blocks   567 ms
    whole closure, ordinary discovery         10,326 blocks   357 ms
    whole closure, --exhaustive                    0 blocks     0 ms

Zero. Not "nearly none" -- the guest takes no dispatch miss at all, and the
runtime translator is never entered. That is the thing this project is for,
and it had never happened before.

The cost is the build: exhaustive translation of dash's closure is 346,888
blocks and 75 MB of wasm against 69,313 blocks and 15 MB for ordinary
discovery. Binary size does not matter here and sustained runtime does, so
the trade is the right way round -- but 75 MB for three objects says what
#93 already suspected, that whole-object translation is the wrong
granularity once the closure is apt's nineteen objects rather than dash's
three.

What is not done: the closure is only installed for the process that
`load`s. A forked child inherits its parent's translations as it always did,
but an `execve` of a *different* program in the closure gets nothing, and
that is the case apt is made of.

### Across a fork and an execve

Extended. `execve` does not rebuild the guest wasm instance -- it reloads the
image into the same memory -- so the closure's functions are still sitting in
the dispatch tables afterwards, and only the *directory* pointing at them was
cleared. `CLOSURE_DIRS` replays it, and a forked child installs the closure
modules itself, because a child adopts its parent's memory (and with it the
directory) but its dispatch tables belong to its own instance and start
empty.

Measured on `dash -c 'echo $(ls /bin | wc -l)'` with dash's exhaustive
closure, `ls` -- a program the closure was *not* built for, sharing only its
libc -- translates 369 blocks where a cold process translates several
thousand.

The command does not complete, and the reason is not this: `wait4` returns
ECHILD, which is #69, and the closure exposes it by making the child finish
before the parent asks. The six-case substitution suite passes with this shim
when no closure is loaded, so nothing here regressed the ordinary path.

### Two bugs the closure work turned up

**A pinned object's span was not passed on, so the next one landed inside
it.** `k_place_set` seeds the global placement table from the closure's
manifest, and it was told the span was zero -- on the reasoning that the
reservation which follows carries the length anyway. It does, but too late:
the counter moved one 64 KiB alignment past each pinned base, so the *next*
object placed automatically was handed an address inside the pinned one.
libc's own reservation then found 0x5000000 occupied, took what the allocator
gave it instead, and every module built for libc was wrong. `ls` segfaulted
on it, and the pin report said exactly what had happened:

    [gex] pin MISPLACED: /lib/aarch64-linux-gnu/libc.so.6
          wanted 0x5000000, landed 0xde42000

The manifest now carries `base span path`, and the placement table is told
both.

**A process that trapped never became collectable, so its parent waited
forever.** #69. `exit_group` goes through `do_exit`, which publishes to the
process table; a guest that *traps* -- a lifter gap, a bad access, a panic in
the shim -- never reaches it, and its record stays unreaped with no status.
The parent parks in `wait4` on a child that will never report, and the run
ends with nothing runnable and no message. `gex_abandon` publishes a SIGKILL
status from the Worker's own crash handler, which is the closest true thing:
the process did not choose to stop. A crash in a child now ends the run with
an error instead of hanging it.

With both, `dash -c 'echo $(ls /bin | wc -l)'` on dash's exhaustive closure
completes, and the blocks translated at runtime are

    pid 1  dash          0
    pid 2  dash (fork)   0
    pid 3  ls        3,281
    pid 4  wc        1,329

Zero for both processes the closure was built for, and for the two it was
not, only their own text and the libraries outside it -- libselinux and
libpcre2, which are in `ls`'s closure and not in dash's.

## Where apt update actually stops

The closure work was meant to land on apt next. It cannot yet, and the reason
is not translation.

A 6.5 MB rootfs was built for it -- apt-get, its nineteen-object closure,
`/usr/share/dpkg`, and a local `deb [trusted=yes] file:/repo ./` repository so
no proxy is needed -- and `apt-get update` on the *plain seed module*, with no
closure involved, does this:

    [gex] pid 3: 419ms translating 15733 blocks
    [proc] abandon 4: the Worker stopped without an exit
    gex: process 4 failed: the gex runtime trapped inside the shim
      (memory access out of bounds, guest pc=0x55f29c4, inside syscall 79)

pid 4 is `/usr/lib/apt/methods/file`; syscall 79 is `newfstatat`. The trap is
in the shim, not the guest. `GuestMem` bounds-checks every access against its
own view, so the suspect is `gex-kmem`: `push` and `pull` are bare
`memory.copy 0, 1` and the engine traps if the range is outside the process
memory. A stat on the shared filesystem goes through `rf_path_meta` to
`k_fs_path_meta`, which pulls the path out and pushes the metadata back.

apt then reports `Get:1`, spins on `net+timeout`, and idles at 0% CPU
forever. That second half should now be fixable for free: `gex_abandon`
publishes the method's death, so the parent's wait has something to end on.

This is #96, and it is what stands between the closure and the program this
project keeps measuring itself against. Worth saying plainly: apt update's
cost was recorded as guest CPU in libapt (#85), and that may well be true,
but it cannot be measured until the run finishes at all.

One more thing the attempt taught, which matters for the closure: apt-get's
own closure does not cover its methods. The `file` method needs libseccomp
and apt-get does not, so a closure built per executable is incomplete for any
program that spawns helpers -- and apt is entirely made of helpers.

### What apt's closure is worth, measured

`apt-get update` still does not finish, but `apt-get --version` does, and it
is not a toy: it loads every one of the nineteen objects, initialises libapt
and prints the index-type table, which is most of what a run pays before it
fetches anything.

    apt-get --version, seed module          32,033 blocks   948 ms
    apt-get --version, whole closure        16,743 blocks   683 ms

Half the blocks and a third of the time, and every pin lands:

    [gex] pin ok: libapt-private.so.0.0  wanted 0x5000000, landed 0x5000000
    [gex] pin ok: libapt-pkg.so.6.0      wanted 0x50a0000, landed 0x50a0000
    ... seventeen of them, all ok

Exhaustively, it is zero:

    apt-get --version, seed module          32,033 blocks   948 ms
    apt-get --version, image module alone   31,693 blocks  1151 ms
    apt-get --version, closure, ordinary    16,743 blocks   683 ms
    apt-get --version, closure, exhaustive       0 blocks     0 ms

Nineteen objects, all seventeen pins landing, the right output, and the
runtime translator never entered. apt is a much bigger program than dash and
it makes no difference: what the build covers, the run does not pay for.

The build is 1,724,482 blocks and 678 MB of wasm, against 348,056 blocks and
80 MB with ordinary discovery. Binary size is not what this project is
optimising and the trade is the right way round, but 678 MB to run
`--version` is the clearest possible statement of #93's point: whole-object
translation is the wrong granularity. Most of those blocks are in libraries
apt loads and barely enters -- libsystemd, libgcrypt, libzstd -- and the
measurement to make next is which fraction of each object a real `apt update`
actually reaches.

### What the hunt for the crash left behind

Three things, all worth keeping whatever the crash turns out to be.

`GuestMem` checked every guest pointer against `view().len()`, and in the
browser that view is a slice over the whole address space a guest pointer may
name rather than over the memory that exists. It now reads `memory.size`
live, on every access, because the bound *moves during a syscall*: `mmap`
grows the memory and then zeroes what it just mapped.

The kernel's cross-memory copies are checked. `push` and `pull` are bare
`memory.copy 0, 1` and the engine traps rather than returning, so a bad
pointer from a guest killed the Worker inside a syscall while it held the
kernel lock. `memory.size 1` says how big the calling process's memory is,
which is the one thing that makes an `EFAULT` possible instead.

And the trace decodes `newfstatat`'s path, which is what turned "a trap in
syscall 79" into "a trap stat-ing `/usr/bin/xz`" -- a file that is not in the
rootfs, which is how the search got as far as it did.

### What apt's method actually dies of

`gex_dump_state` answers it. On the way out of any trap the Worker now prints
the guest's registers and, for each of them, which mapping it points into:

    gex: pid 4 thread 0 sp=0x3fff390 x0=0x2 x1=0x0 x2=0x2 x30=0x511cb18
    gex:   sp=0x3fff390 in 0x3800000..0x4000000 [stack]
    gex:   x0=0x2 in UNMAPPED

`x30` is `libapt-pkg + 0x7cb18`, and the instruction before it is a call
whose argument comes from `ldr x0, [sp, #0xa8]`. So libapt loads a pointer
out of a stack slot and frees it, and the pointer is **2**. `__libc_free`
dereferences a chunk header sixteen bytes below it; on Linux that is
`SIGSEGV`, and in wasm it is a trap that takes the Worker with it.

What it is not, each ruled out by measurement rather than argument:

- **Not a leftover stack.** `execve` was handing the new program the previous
  image's stack instead of the zero page Linux gives it -- a real bug, fixed
  here, and `free(NULL)` would have been a no-op. The value is still 2, so
  something computes it.
- **Not the intrinsics.** `GEX_NO_INTRINSICS=1` crashes identically.
- **Not the missing `/usr/bin/xz`.** The full Debian rootfs has no `xz`
  either, so that `ENOENT` is what apt expects; and with a proper flat-repo
  `Release` and `InRelease` in place the crash happens *earlier*.
- **It *is* the browser shim.** This was recorded the other way round an hour
  earlier and the earlier reading was wrong, for a reason worth keeping: the
  rootfs it was measured on was incomplete, and the native host was failing
  in it for a different reason. On a *complete* rootfs the two hosts differ
  cleanly -- native finishes, the browser crashes.

#### And what in the shim it was: the child's heap, over the parent's memory

A pointer whose value is **2**, loaded from a stack slot, is the far end of
memory corruption, and this is the near end of it.

`ARENA_NEXT` is the cursor that both the guest's mappings and the shim's own
Rust heap take from. It is a static of the shim *module instance*, so a
forked child -- which instantiates a fresh shim -- starts it at zero, and
`arena_take` then begins at `ARENA_BOTTOM`. But the child's memory already
holds its parent's entire address space, and every byte of that was handed
out from `ARENA_BOTTOM` upwards. The child's first claim is land the parent
is still using. Nothing catches it, because `MemoryMap::mmap` inserts an
arena-extended range without asking whether it is free: the arena is supposed
to be the one allocator that cannot collide.

Talc usually gets there first, so the shim's own allocator bookkeeping lands
inside the guest's heap, and the guest meets the damage later and somewhere
else -- as a `free` of 2, or as `malloc_consolidate(): invalid chunk size`.

This is also the whole reason the closure appeared to fix it, and why #96 was
filed against the runtime translator. A child running code somebody else
compiled barely allocates, so it never claims a span, so it never collides.
The translator was not doing anything wrong; it was simply the only thing in
the process allocating enough to hit the bug.

Two measurements that pinned it, both worth keeping as tools:

- A forked child checksums every region it was handed and reports the ones
  that do not match. All 87 guest regions verify byte for byte; only the
  register contexts and the dispatch directory differ, and the child rebuilds
  both. That ruled out the copy, which had been the standing theory.
- After the fix the child's memory goes from 161 MiB to 177 MiB at the same
  point in the run -- exactly one 16 MiB Talc chunk. That is the heap that
  used to be placed inside the parent's mappings, now taking fresh space.

Fixed by taking the floor from the memory the process actually has rather
than from a static that resets. `apt-get update` then runs to completion
under the Node host and exits 0.

`LAST_SYSCALL` cost most of the three days on its own. It was set on syscall
entry and never cleared, so the crash reported "inside syscall 99" --
`set_robust_list`, whose handler is `Ok(0)` and reads no memory at all. It
was merely the last call the child had made before returning into translated
code. A diagnostic that cannot be wrong in a quiet way is worth more than one
that is usually right: it now says "after" unless the call is still on the
stack.

## apt-get update works

    apt-get update: exit 0, 27879ms, 1 index file(s) written

On the native host, against a local flat repository: four URIs fetched, the
lists written, the index parsed. `tests/real-programs/apt-update.sh` is that
run, and it is now part of the gate.

What it took was not translation. It was a rootfs that is actually complete.
apt is made of helpers -- `file`, `copy`, `store`, `gpgv` -- and a rootfs
assembled by hand from apt-get's own `DT_NEEDED` closure is missing what
*they* need: `libseccomp` for the methods, `/etc/passwd` and `/etc/group` for
the privilege drop, and the binaries the fd-scanning and exec paths reach
for. Every one of those failed as something else -- a trap in `newfstatat`, a
method exiting 100 after `getdents64`, a `free` of a wild pointer -- and none
of them said "your rootfs is short of a file".

The lesson is cheap to state and was expensive to learn: **measure a package
manager against a whole machine, not against a closure.** The closure is the
right unit for *translation*; it is the wrong unit for a filesystem.

## Where a rootfs stops fitting

#91, fixed. The kernel's memory is where the rootfs lives -- the filesystem
belongs to the kernel and every bundled file is copied into it -- and it was
capped at 1 GiB by `--max-memory` in the kernel build and by the `maximum` the
host passes. A complete Debian arm64 image did not fit, and the failure was a
bare `unreachable` out of `rust_oom`.

Both are 4 GiB now, which is all a wasm32 memory can be. It costs nothing:
`maximum` reserves address space rather than pages, and the memory still
starts at 128 MiB and grows as files arrive. A 396 MB rootfs loads.

That leaves exactly one thing between `apt-get update` and a browser, and it
is the `free(2)` above.

### Diffing the two hosts, syscall by syscall

Now that native finishes and the browser does not, the two can be diffed
directly. Both were traced on the same rootfs and apt's `file` method -- pid 4
in both -- was aligned call by call, ignoring `mmap`/`munmap`/`mprotect`/`brk`
because placement legitimately differs.

**The first 148 calls are identical**, name, path and return value. That is
the whole of `ld.so`, libgcrypt's initialisation, the capability drop, and
apt's handshake. Then the method reads apt's configuration message and gets
5,534 bytes natively against 5,520 in the browser -- fourteen short.

Fourteen bytes looked like a pipe losing data, which would have explained
everything. It is not. Dumping both blobs with
`-o Debug::pkgAcquire::Worker=1` and diffing them line by line, the only
difference is

    CommandLine::AsString=/tmp/apt-small/usr/bin/apt-get ...   (native)
    CommandLine::AsString=/usr/bin/apt-get ...                 (browser)

which is `/tmp/apt-small`, exactly fourteen characters, and an artefact of
invoking the native host with a host path and the browser host with `argv0`.
The configurations are otherwise byte-identical. The method receives the same
input on both hosts and crashes on only one.

Two more things the diff settled:

- The `/usr/bin/xz` stat is not the trigger. Native performs it **eight
  times** and carries on; the browser does it once and dies.
- The crash moves. With `-o Debug::pkgAcquire::Worker=1` -- which changes
  nothing but the length of a string -- it stops being a trap at the `xz`
  stat and becomes one inside `read`, at a different address, with a mapped
  pointer. A fault that moves when an unrelated allocation moves is
  corruption, not a wrong branch.

So the difference is in how the browser *executes* what both hosts agree on.
The hosts share the lifter and the kernel; what they do not share is the
backend. Native goes through Cranelift, the browser emits wasm from tier-1.
That makes a tier-1 codegen bug the standing suspect, and the closure is the
instrument: `gexc` runs the emitter in a normal process, where
`GEX_NO_LOOP_FUSION`, `GEX_NO_FLAG_LIVENESS` and `GEX_REGION` can be turned
off one at a time and the resulting closure run in the browser. A closure
built `--exhaustive` also answers a prior question -- whether the fault is in
runtime translation at all -- because a run against one translates nothing.

### The statically recompiled apt does not crash

That last experiment answered it, and the answer is the best one available.

`apt-get update` in the browser host, against apt's nineteen-object closure
built `--exhaustive`:

    [gex] pid 3:  28ms translating   280 blocks     (was 15,733, and a crash)
    Get:1 file:/repo ./ InRelease [419 B]
    Ign:1 file:/repo ./ InRelease
    Get:2 file:/repo ./ Release [419 B]
    Get:3 file:/repo ./ Release.gpg
    Ign:3 file:/repo ./ Release.gpg
    Get:4 file:/repo ./ Packages [587 kB]
    Reading package lists...

Nine processes, every pin landing, no trap. The `file` method -- which had
died in `free` on every previous browser run -- fetches all four URIs and
apt goes on to parse the index. The same host, the same rootfs, the same
apt: the only change is that the code was translated ahead of time instead
of at runtime.

So the fault is in runtime translation, and the statically recompiled code
is *correct where the JIT's is not*. That is worth stating plainly, because
it is the project's own thesis arriving from an unexpected direction: static
recompilation has been argued for on speed, and here it is the thing that
makes a real program run at all.

It also narrows the remaining bug to something the runtime translator does
and `gexc` does not, on identical input, through the same lifter and the same
emitter. The obvious candidates are what differs between the two paths rather
than anything in codegen itself: block discovery from a cold start against
whole-object discovery, the dispatch slots a miss allocates, and the module
boundary a runtime translation draws around a handful of blocks.

## What `apt-get update` actually spends its time on

Not what #85 says. It is recorded there as guest CPU in libapt, and that was
a guess; this is the measurement.

**The cost does not scale with the index.** Same rootfs, same everything, only
the number of packages in `Packages` changing:

    300 packages    26,376 ms
    1,000 packages  26,148 ms
    3,000 packages  26,805 ms

Ten times the data for four hundred milliseconds. Whatever apt update costs,
parsing is almost none of it -- so the profile that pointed at
`debListParser::ParseDepends` and `pkgTagSection::Find` was pointing at the
only part that *is* work, which is a small part.

**It is translating the same libraries ten times.** `--verbose` counts each
process separately, and apt is made of processes:

    14,531 blocks in   253 compilations
    19,894 blocks in   525
    19,754 blocks in   524
    18,634 blocks in   508
    33,957 blocks in   838
    33,976 blocks in   876
    35,365 blocks in   902
    14,531 blocks in   253
    14,531 blocks in   253
    76,081 blocks in 2,824      <- apt-get itself
    ------------------------
    281,254 blocks in 7,756 Cranelift compilations

The `file`, `copy` and `store` methods are separate processes and each one
translates its own copy of libapt-pkg, libstdc++ and libc from a cold start.
`14,531` appears three times because three of them do exactly the same work.

That is the twenty-six seconds, and it is why the closure result matters
beyond correctness. The same run in the browser against apt's exhaustive
closure translated

    pid 1     0     pid 2     0     pid 3   280     pid 4 1,817
    pid 5   238     pid 6 1,789     pid 7   453     pid 8 2,029

-- about 6,600 blocks in total against 281,254, **forty-two times less**,
because every process installs the modules the build already made instead of
lifting the library again.

Two consequences worth stating.

**#85 is wrong as filed** and should say this instead: apt update is slow
because gex translates libapt once per process, and apt has ten processes.

**Whole-object recompilation is worth measuring properly, and it loses.**
`gexc run --recompile-objects`, which statically recompiles each object *as
it is mapped* rather than ahead of time:

    apt-get update, translating on demand    30,262 ms
    apt-get update, --recompile-objects     134,477 ms

Four and a half times worse, because it pays Cranelift for every block of
every object and apt enters a few thousand of them. The value of static
recompilation is not the granularity, it is that the work happens *before the
run*. Doing the same work during the run is the worst of both.

### The whole update, in a browser, against the closure

The complete Debian rootfs, apt's nineteen-object closure built
`--exhaustive`, and `apt-get update` from `InRelease` to `Reading package
lists`:

    ten processes, 12,576 blocks translated in total

against **281,254** for the same work on the native host. Twenty-two times
less, and every process ends `code 0`. pid 1 -- apt-get itself, which does
most of the work -- translates **nothing at all**.

The wall clock is not the comparison to make yet: most of it is V8 compiling
a 678 MB module, which is load rather than run, and load is not what this
project is optimising. What is comparable is the work removed, and it is
nearly all of it.

The 12,576 that remain are the interesting number now.

### The union closure, and why it did not help

The obvious explanation for those 12,576 was that apt-get's closure is built
from *apt-get's* `DT_NEEDED`, and its methods need objects apt-get does not.
`gexc build --closure --with PATH` was built to test it: several roots, one
placement, one deduplicated set.

It found exactly one missing library -- `libseccomp`, for the methods'
sandbox -- and rebuilding with it changed nothing:

    closure of apt-get alone          12,576 blocks over ten processes
    closure of apt-get + its methods  12,540 blocks over ten processes

So the hypothesis was wrong, and the per-process figures say what is actually
left:

    pid 1  apt-get        0        <- the closure covers it completely
    pid 2  dpkg       2,004
    pid 3               269
    pid 4             1,812
    pid 5               233
    pid 6             1,784
    pid 7               448
    pid 8             2,024
    pid 9             1,983
    pid 10            1,983

Every one of those is an *executable's own text*. pid 1 is the only process
running a program the closure has an image module for; every other process
has `execve`d something else -- dpkg, or a method -- and is lifting its
`.text` from cold. The libraries are all covered; the programs are not.

That is a design limit rather than an oversight, and it is worth stating
where it comes from. The closure has one image module, at `PIE_BASE`, chosen
when the process is loaded. A helper is a separate executable that wants that
same base, so it cannot simply be another object in the set: two objects at
`PIE_BASE` and the second one's dispatch entries land on the first one's
pages. `--with` therefore takes a helper's *libraries* and not the helper.

Closing it needs one of two things, and both are real features:

- **Place executables like libraries.** They are PIE, so they can go anywhere;
  what stops it is that gex's loader puts the executable at `PIE_BASE` itself
  rather than through `mmap`, so a pin does not reach it -- the same reason
  `ld.so` had to be special-cased.
- **Swap the image module at `execve`.** The host instantiates one guest
  module per process; letting a process adopt a different prebuilt image when
  it execs is the general answer, and also what would let a closure cover a
  whole shell session rather than one program.

Worth the effort at roughly 12,000 blocks against 281,254 -- a twentieth of
what is already saved -- but not before the run is confirmed end to end.

## A forked child cannot say what code it is running

The demo was reported slow -- seconds per `ls`, however many times you ran it
-- while the same shim under Node converged to almost nothing. Both hosts run
the same `gex_browser.wasm` and the same glue, so the disagreement was the
finding, not the numbers.

Getting at it needed a browser that stays up. `web/test/site.mjs` boots
Chromium, serves the bundle, unpacks a rootfs tar into the guest filesystem
and then runs *one* command: 295 seconds for three `ls`, against 20 under
Node. `web/test/harness.mjs` pays that once and keeps the page alive, taking
commands over HTTP against a rootfs streamed out of a real directory -- 38.7
seconds for the same three, and about 1.2 seconds a command after the first.

With it, one run said what a fortnight of guessing had not:

    pid 2  9375 blocks  hit=0  miss=0  noident=28  served=0  stored=28
    pid 4  8009 blocks  hit=0  miss=0  noident=27  served=0  stored=27

Nothing is ever found again. Splitting `noident` into its two causes gave
`writable=0, nocode=28`: the mappings are not writable, they simply have no
code identity. Naming them:

    no code identity for 0x1017000: vma 0x1000000..0x101e000 name "[forked]"
    no code identity for 0x50bd000: vma 0x5000000..0x518c000 name "[forked]"

A forked child's address space arrives as one anonymous span. The fork
protocol carries `(start, end, prot)` per region and nothing else, so the
names and the code identities never travel; the child calls the lot
`[forked]`, cannot identify any page in it, finds nothing in the shared
module store, and lifts its parent's libraries from cold. Every `ls`, every
`wc`, every apt method.

It also explains a shape that made no sense: 335 blocks per miss in the
browser against Node's 52. A child with one undifferentiated span has no
per-object structure for discovery to work against.

The fix carries the map rather than the spans -- one blob holding each
region's name, protection, and the file and offset its code came from, built
by `gex_vma_blob_read` and rebuilt by `MemoryMap::adopt`. `origin` does not
travel: it says a region can be reconstructed by re-reading a file, which is
a claim about mappings this process made and not one to inherit.

Worth noting what this does *not* fix. Translation was already a small part
of the wall clock -- 4-8% of an `ls`, and 184ms of the 118 seconds an
`apt-get update` takes against a closure. Sharing more of it cannot buy back
time nobody is spending there.

## Why every `ls` in the demo costs a second

Reported from the demo: running `ls` takes seconds, every time, however many
times you have run it. Measured, three `ls` in one shell:

    pid 2  (ls #1)  574 ms  17,814 blocks   have=407 hit=6  miss=6
    pid 3  (ls #2)  563 ms  15,725 blocks   have=407 hit=12 miss=0
    pid 4  (ls #3)  441 ms  15,725 blocks   have=407 hit=12 miss=0

The same 15,725 blocks, every time. There is a mechanism meant to prevent
exactly this -- a system-wide store of translated modules, keyed by page and
by the identity of the code on it -- and it fires 18 times against 1,295
dispatch misses. One and a half per cent.

Two things are wrong, and the second is why the first cannot simply be fixed.

**The reuse path is behind the wrong guard.** It is consulted only when the
page has no dispatch base:

    if h.jit.page_table.lookup(page).is_none() { ...ask the store... }

and `have=407` counts the misses where it *did* have one. Those 407 are where
the time goes: a page whose base exists but whose slot for this particular
entry point is empty, because discovery found some of the page's blocks and
not this one. Every one of them translates instead of asking whether somebody
has already done it.

**Slots are allocated per process, so serving a foreign module is unsafe.**
`PageTable::next_slot` is an `AtomicU64` owned by one process's table, started
from the image module's high-water mark. Two processes running the same
program therefore hand out *the same slot numbers* to different code. A
module built by process A fills slots `[X, Y)`; process B may already have its
own blocks there, and B's directory entries for its own pages still point at
`X`. Installing A's module makes a branch into B's page execute A's code.

The eighteen serves that happen today are that hazard, running rarely enough
not to have been caught. Opening the guard would make it frequent.

So the order of work is fixed by the second problem: slot allocation has to
become globally unique before reuse can be turned up. That is what the parked
`shared-slot-allocator` branch was for, and it is the same conclusion its
notes reached from the other end -- it made the allocator global first and
found a pipe wedge on the third process, which is where it stopped.

One change went in from this investigation and is worth recording as *not*
having helped: the page identity used to hash the extent of the mapping
(`v.end - v.start`) along with the file and offset. `ld.so` builds a library's
mapping in four steps and every one of them splits a vma, and the RELRO
`mprotect` happens after some pages are already translated -- so the extent is
not a fact about the code and changes during a process's life. Removing it is
right on its own terms. It moved nothing: `hit` stayed at 12, blocks stayed at
15,725. The binding constraint is the guard, not the key.


## A miss that serves sixteen modules and retries the same branch

Added 2026-08-09, from the first trace of `apt-get update` taken inside a
browser rather than under Node.

The loop, verbatim from the trace:

    miss at pc=0x503a7d0
    serve 16 module(s) of the object at pc=0x503a7d0
    miss at pc=0x503a7d0
    serve 16 module(s) of the object at pc=0x503a7d0
    ... nineteen times ...
    serve 7 module(s) of the object at pc=0x503a7d0
    miss at pc=0x503a7d0
    translated 272 blocks in 11 ms

306 misses at one address. Roughly five thousand modules taken out of the
shared store, compiled and instantiated, to discover that none of them held
the block -- which the translator then produced in eleven milliseconds.

The cause is a mismatch between what a translation unit *is* and what the
serve path assumed it was. A unit holds what was reachable from the block
that faulted **in the process that built it**. Arriving at the same object by
a different route, its stored modules mostly do not contain your address. So
serving a batch and returning to the guest is not progress: the branch misses
again, and the next miss takes the next batch.

Taking the modules was never wrong -- this process wants them and will branch
into them shortly. Going back to the guest *expecting the branch to resolve*
was wrong. The fix is one condition: if none of the served modules covers the
faulting page, lift the block in the same turn rather than trapping and
retrying. One trap instead of nineteen.

### Why it stayed hidden

A forked child used to arrive with every module its parent had already
instantiated, so there was nothing left in the store to drain and the loop
never ran. It appeared the moment children stopped doing that -- which makes
it look like a regression in the change that exposed it, and it is not. Any
process meeting an object it does not have pays this, and in a ten-process
`apt-get update` that is all of them.

The general lesson is the one this file keeps relearning: a counter that says
"served 2540 modules" reads like the sharing working well. It was the sharing
failing repeatedly and being retried.


## "apt hangs" was a child crashing, and the timeout was the symptom

Added 2026-08-10, after eight bisect runs that all reported the same thing
and none of which was read properly.

Every failing run ended `harness: 120002ms` -- the command hit its cap -- and
that was taken as a hang. It is not. Further up the same log:

    gex: process 11 failed: the gex runtime trapped inside the shim
         (memory access out of bounds, guest pc=0x50bbd20, inside syscall 99)
         in /lib/aarch64-linux-gnu/libc.so.6 +0xbbd20
    [proc] abandon 11: the Worker stopped without an exit

A forked child faults on guest memory inside `set_robust_list`, apt waits for
a method that is never going to answer, and the harness gives up two minutes
later. The run that passes -- 82 seconds, the committed tree -- contains no
such line at all, so the crash is the whole difference and the wall-clock
figure carries no information beyond "something died".

Two habits caused the waste, both worth naming:

  * The greps were `rc=` and `harness: [0-9]+ms`. Neither can ever match a
    crash. A filter that cannot express the failure you are hunting will
    report every failure identically, and eight runs looked like one bug.

  * A timeout was treated as a measurement. It is the absence of one. The
    useful signal was three lines above it and arrived in seconds, which
    also makes it a far cheaper thing to bisect on than a cap.

The general rule the harness now follows: when watching for an outcome, match
the failure signatures as well as the success ones. Silence and a timer
expiring look identical, and they mean completely different things.


## The closure in a browser: translation gone, instantiate is everything

Added 2026-08-10. The first measurement of a statically recompiled closure in
a page, and it needed a new harness mode to take at all -- a closure pins
every object it reaches starting at 0x5000000, which is where a shell's own
image goes, so booting `dash` and handing it apt's pins killed the shell
before it printed a prompt. `harness.mjs program` runs one ELF as pid 1
instead.

`apt-get update` against a local repository, ten processes either way:

                       runtime translation      closure
    blocks translated      70k - 230k each      233 - 2004 each
    ms translating          ~5000 total         224 total
    modules installed      1900 - 3448 each     57 - 82 each
    ms installing          ~30000 total         16167 total

Two things follow.

**The feature works.** Translation falls by two orders of magnitude. 224
milliseconds of lifting for a whole `apt-get update` is the thing this
project is for, and it had never been shown in the host that matters.

**And it moves the cost rather than removing it.** Install halves and is now
essentially the entire run. It did not fall in proportion to the module
count: 57 modules cost about what 3,448 did. A closure's modules are enormous
-- `libgcrypt` is 318 MB of wasm on its own -- so `instantiate` scales with
bytes as much as with count. The page already compiles each library once and
hands every Worker a `WebAssembly.Module`, so compilation is shared; what
remains is per-process instantiation, which is not.

Which promotes #93 from a suspicion to the obvious next step. The closure
recompiles whole objects and apt reaches a small fraction of each, and
trimming it to what is reached is the only lever that touches the term that
now dominates: merging cannot help with nineteen modules, and a faster lifter
cannot help with 224 milliseconds of lifting. The prize is bounded by the
table above -- most of sixteen seconds in an eighty-second run.

## The apt number moves 16s by itself, and four conclusions rest on it

Identical committed code, measured twice: **82.1s**, then **98.1s**. Nothing
changed between them but the hours. This is a shared host -- two KVMs, an
OpenSearch, a Docker daemon -- and a browser run of `apt-get update` is at the
mercy of all of it.

Everything this session concluded about passive element segments was drawn
from single runs against the 82.1s figure:

| what was changed | one run | what was concluded |
|---|---|---|
| passive segments + host fills each | 114.5s | "the fill-request trap costs 32s" |
| ...gate the trap until a module defers | 112.5s | "wrong; it is the per-segment crossing" |
| ...one `gexfillall()` per module | 114.4s | "wrong; it is compiling the helpers" |
| ...helpers only above 4096 blocks | 117.0s | wrong again -- and it *cannot* be |

The last row is what gives it away. Every runtime unit in an apt run is about
twenty blocks, so the 4096-block gate turns the emitter change off entirely:
that configuration produces the same bytes on the runtime path as the
baseline, and it still read three seconds *worse* than the run that supposedly
proved the helpers expensive.

So the effect being chased is smaller than the noise, and three of these
diagnoses were noise wearing a hypothesis. None of the three is disproved
either -- they were never tested.

**What to do about it.** A single run cannot resolve anything under about
twenty seconds here, which is a fifth of the measurement. Either

* alternate A/B/A/B in one script, which cancels drift over the ten minutes
  four runs take -- cheap, and enough for a change this size; or
* stop timing the whole run and count the thing the change is supposed to
  affect. `installMs` per process is already reported and is a sum of
  `instantiate` calls, not wall clock, so it is far less exposed to what else
  the host is doing. A change that means to cut instantiation should be judged
  on instantiation.

The second is the real answer, and it applies well beyond this: every table in
this file that compares two configurations by one wall-clock run each is worth
exactly as much as the four rows above.

## The fill never fired: a packed entry matched against a bare slot

`gexfillat(entry)` is how a miss asks a deferred module for the slots of the
page it wants. It was handed what `page_table.lookup` returns -- a *directory
entry*, a table index packed above a within-table slot -- and range-tested it
against the segment's `offset`, which is the slot half only, and only of the
segment's first translated block. Not the page base unless that block happens
to start the page.

Two different quantities, so the test essentially never matched. Nothing was
ever filled; the branch that asked missed again; and each of those misses fell
through to serving the object, sixteen modules at a time.

The tell was never in the clock, and this is the part worth keeping:

|  | wall | module installs |
|---|---|---|
| before | 93,770 / 93,378 ms | 52,708 |
| with the bug | 118,963 / 114,786 ms | **179,538** |
| fixed | 86,309 / 86,650 ms | 50,073 |

The install count moved 3.4x. The clock moved 25%, on a machine where the same
committed code had already read 82.1s and 98.1s on different hours -- which is
why three earlier diagnoses of this same regression were wrong. Counting the
thing the change acts on found in one run what four timed runs had missed.

A segment now carries the entry of the page it covers and the test is
equality: exact, immune to another table holding the same slot number, and one
instruction where the range test was three.

Deferral is also structural now instead of a size guess. Only a closure
library defers -- installed at process start, before the guest has asked for
anything, mostly never reached. A unit the runtime translator built exists
*because* a branch missed on it, so deferring that guarantees a second miss on
the address that caused the first. Above the block gate such a unit still has
passive segments and is written with one `gexfillall()`.

Net, alternating A/B/A/B in one sitting: **93.6s to 86.5s**, `rc=0`, one index
written, zero crashes. The spread within each arm was under half a second.

What this does not yet show is the thing demand filling was built for. apt's
closure predates passive segments, so the run above still translates at
runtime. The closure is where instantiate was 1.2-2.2s *per process* for code
the process never reaches -- that measurement is next, and it needs the
closure rebuilt with the current emitter.

## The closure is the fast path, and demand filling works everywhere but the browser

With the emitter's passive-segment gate lifted, `apt-get update` against a
19-object `--exhaustive` closure runs in Chromium in **55.7s** against the
runtime-translation path's **81.7s**. Static recompilation wins by a third,
which is the claim this project exists to make, and it is now measured rather
than assumed.

Of those 55.7 seconds, **29.0** are instantiation. That is what demand filling
is for: a closure library is instantiated once per guest process and almost
none of it is ever reached.

The mechanism is built and it is correct. Under `run-node.mjs` -- the same
glue, deferral on, the same passive closure -- apt completes with `rc=0`, all
ten processes, the 587 kB index fetched and read, and **zero blocks
translated**. Every branch found its slot.

In Chromium the same artifact dies: SIGILL after two processes with deferral,
a stall after three without. Two processes is the first fork, and the browser
is the only host that gives each guest process its own Worker, its own memory
and its own tables. A forked child adopts the parent's dispatch directory --
so the child *knows* page X is at entry E -- while its own copy of the module
is deferred and unfilled, and `FILL_ASKED` in the parent has already recorded
that page as asked. A branch into it is an unwritten slot, which is an illegal
instruction. That is #106 and it is one experiment from being settled.

Three things this cost, worth not repeating:

* The measurement had to be `--exhaustive`. A closure built without it covers
  so little that the run falls back to translating 225,722 blocks -- against
  17,414 for the exhaustive one -- and then trips #96 and exits -1. The
  artifact was blamed for the code twice.
* `gexfillall` was suspected of filling short. `tools/segcheck.py` settles that
  kind of question offline in a second: it parses a module and reports segments
  against `table.init` count. Every library is exact -- 99/99, 413/413,
  429/429, 355/355.
* Reproducing under node first would have been faster than any of it. The node
  host is a debug adapter and not what this project optimises, but it runs the
  same glue, and "works there, fails in the browser" is worth more than four
  browser runs.

## Demand filling pays: instantiate down 46 percent on the closure

The closure path is what this project is for, and instantiation was most of
it: 29 seconds of a 55-second `apt-get update`, nearly all of it writing table
slots for code the process never executes. Deferring those writes now works.

Alternating, one shim, only the artifact changing:

| | wall | install | blocks translated |
|---|---|---|---|
| active segments | 54,889 / 57,125 ms | 29,102 / 31,658 ms | 17,462 |
| deferred | 51,198 / 52,081 ms | 17,100 / 15,862 ms | 17,436 |

Instantiate falls from 30.4s to 16.5s. Translation is unchanged, so nothing is
being lifted that the closure already held -- which is the check that matters,
because every failed version of this cost exactly that.

Four defects stood between the idea and the number, and each one made the idea
look wrong:

1. `gexfillat` matched a *packed directory entry* against a segment's bare
   within-table offset. Two different quantities, so it never fired once. The
   symptom was 179,538 module installs against 52,708 and 23 seconds, on the
   runtime path, which got blamed on passive segments being slow.
2. The *host* decided what to defer. A forked child inherits its parent's
   modules through another path entirely and filled the same libraries
   eagerly, so nineteen of twenty processes had nothing deferred while every
   miss they took still asked. The module decides now: only one built passive
   exports `gexfillat`, so the test is its presence.
3. A page whose module had not been installed *yet* was written off for ever.
   That is the ordinary case for a child, which installs progressively while
   the guest runs. Failures are now forgiven when the next deferred module
   arrives.
4. The ask sat below `maybe_recompile_at`, so a process recompiled whole
   libraries it already held before asking whether it held them. The most
   expensive path in the miss handler, taken to avoid the cheapest.

The fourth was invisible because the whole-object path added to `jit.blocks`
and never to `jit.nanos`: "2,639,578 blocks in 986ms" is 2,675 a millisecond
against 41 elsewhere. Not a fast translator, an untimed one. That is the third
time today a counter that did not add up was the whole story -- after the
82.1s baseline that had drifted to 98.1s, and the harness that waited eleven
minutes on a result id nothing would send. **A number that is impossible is
worth more attention than a number that is merely bad.**

### And the same counter was hiding the runtime path's real cost

With the whole-object path finally timed, the ordinary runtime-translation
`apt-get update` -- no closure -- reports:

    translate 91,466ms   install 29,992ms   3,381,964 blocks   80,252ms wall

It used to report 13,720ms of translation. The other 78 seconds were whole-object
recompiles that added blocks to the counter and nothing to the clock. Summed
across 26 processes that overlap in parallel Workers, so it is not 91 seconds
of wall -- but it is three times the install cost that this file has spent
weeks treating as the main event.

That reorders the list. **#93 -- measure what fraction of each object apt
actually reaches, and recompile only that -- is now the top runtime-path
idea**, not a speculative one: recompiling a whole library to satisfy one
miss is the single largest measured cost in the run. The closure work just
finished is the same insight arrived at from the other side: it wins by never
writing slots for code the process does not reach.

## A shell can boot on a closure now, and then execve throws it away

`harness.mjs once --closure` had never worked. Three things were wrong and
each hid the next.

**The build flag.** The closure was built with a shared memory while the
browser runs every process on a plain one -- the shared shim is for guest
threads, which are not Workers yet. Every library failed to instantiate with
`mismatch in shared state of memory, declared = 1, imported = 0`. The note on
file said the pins collided with the shell's placement; they never did.
Rebuilt `--no-shared-memory`, a 77 MB dash closure boots a shell and answers
`echo` and `ls /bin` in 153ms.

**The transfer.** 694 MB of `WebAssembly.Module` in one `postMessage` is
`DataCloneError: Data cannot be cloned, out of memory`. A `Module` is
structured-cloneable and the engine shares the compiled code, which is why
the design sends modules rather than bytes -- true, and it does not make the
clone free. One library per message crosses fine, and the Worker waits for
the count it was promised before running the guest. Installing them *after*
the guest starts was tried first and broke the case that already worked: the
shell begins executing against a layout whose libraries have no slots, and
the 1.65s dash boot became no prompt at all. The clone needed splitting; the
sequence did not.

**And what is left.** `apt-get update` now runs under a shell from a closure
-- rc=0, one index, zero crashes -- and is *slower* than not using one:

| | wall | blocks translated |
|---|---|---|
| runtime translation | 79,622 ms | 3,121,058 |
| dash+apt-get closure | 112,904 ms | 3,584,956 |
| ...plus the five methods | 121,980 ms | 3,431,098 |

Adding ten more objects to the closure moved the block count four percent,
which is how the last theory died. The log says what is actually happening:
a dash-only closure prints `exec: 394 closure pages kept` when the shell execs
something, and in both apt runs **that line never appears at all**. The
libraries are installed at boot, the shell execs `apt-get`, and the directory
entries pointing at them are gone. Every process then translates libapt from
nothing.

Which is also why the two configurations that work, work: pid 1 of a dash
closure never execs before the prompt, and the 51.6s one-shot path runs a
single program with no exec in it. Neither has ever exercised the thing that
is broken. See #107.

### And then it is faster: 60.8s against 79.6s

The child had the closure's code and no record that it was a closure.

`execve` clears the dispatch directory -- the new program's pages are somebody
else's code at the same addresses -- and keeps exactly one thing: the pages
`CLOSURE_DIRS` names, whose modules are still in the tables. That static is
per-Worker and only `gex_add_closure_module` fills it, and only the closure
install path calls that. A forked child gets its parent's modules through the
fork record and installs them by the ordinary path, so it had every library
and no idea any of it was a closure's. The shell exec'd `apt-get`, the
directory was cleared, and twenty-five processes translated libapt from
nothing.

Giving the adopt path the same install as a fresh process:

| | wall | blocks translated | lifting |
|---|---|---|---|
| runtime translation | 79,622 ms | 3,121,058 | 81,628 ms |
| closure, before | 121,980 ms | 3,431,098 | 89,516 ms |
| **closure, after** | **60,803 ms** | **58,826** | **1,488 ms** |

`exec: 3689 closure pages kept`, where that line had never appeared once.
rc=0, one index written, zero crashes.

**Static recompilation is now the faster path in the configuration the demo
site uses**, and not only in a one-shot benchmark that never execs anything.

What remains is instantiation: 37,122 ms of the 60.8 s, the closure being
instantiated in twenty-six processes. Demand filling already cuts that 46 per
cent on the one-shot path and should apply here unchanged -- which would put
apt in a browser somewhere near 40 seconds, against the 82 this file opened
with.

### Instantiate a library when a branch reaches it: install down 57 percent

The slots were written on demand and the instances were not. A process built
all twenty-nine of apt's libraries at boot -- 37 of a 61 second run -- to
reach a handful.

Installing one now registers its metadata, grows the tables to its slot count
and stops. The directory points at its pages, so a branch resolves and faults
where it should, and the fill builds the instance then. The shim answers
`gex_closure_owner(page)` because a page is what a miss knows.

| | wall | install | blocks translated |
|---|---|---|---|
| eager instantiate | 60,803 ms | 37,122 ms | 58,826 |
| **lazy instantiate** | **57,130 ms** | **15,880 ms** | 58,826 |

The block count is identical to the digit. That is the number to check on any
change of this kind: every earlier version of demand filling bought install
time by translating more, and this one buys nothing back.

**Where `apt-get update` in a browser stands, both under a shell:**

| | wall | blocks translated |
|---|---|---|
| runtime translation | 79,622 ms | 3,121,058 |
| statically recompiled closure | **57,130 ms** | **58,826** |

Of the 57 seconds, 15.9 are install summed over 26 processes and 1.5 are
translation. Install has stopped being the question. What the guest actually
spends its time on is next: #86 (decompression) and #72 (memcpy inside libc).

### What is left is apt parsing its index, not the runtime

The browser could not be asked what the guest was doing -- `GEX_HOT` existed
only for the Node adapter. It can now (`--hot`), and with install and
translation down to 17 seconds of 57, the answer is the only thing left that
matters:

    357  libapt-pkg  debStringPackageIndex::debStringPackageIndex
    312  libapt-pkg  vector<HashString>::_M_realloc_insert
    271  libxxhash
    247  libapt-pkg  debListParser::ParseDepends
    109  libapt-pkg  pkgTagSection::Find

apt parsing a 3,000-package index and hashing it. **Not decompression and not
memcpy inside libc**, which are #86 and #72 and were the two standing
candidates for where a run goes. Neither is where it goes.

Two things to know before using these numbers. `gex_hot_blocks` is a
Misra-Gries table, not a counter: a block landing on an occupied slot
decrements the incumbent instead of counting itself, so colliding hot blocks
suppress each other and only the ranking is meaningful. And profiling costs
about six per cent -- 60,583 ms against 57,130 ms.

This changes what the next question is. The time is real work rather than an
emulation pathology, so what matters is the multiplier over native, and
nothing here measures that: the rootfs is aarch64 and the host is x86, so a
ratio needs real hardware or qemu-user. Without it there is no telling forty
seconds of guest time from four seconds of work done ten times too slowly.

## The bounds guard costs up to 54 per cent

`GEX_UNSAFE_NO_GUARD=1` exists to price the guard and had not been priced on
the browser. It is expensive. Same host, same session, nbench ratios against
QEMU measured in each run:

| KERNEL | with guard | without | |
|---|---|---|---|
| STRING SORT | 2.86x | **4.40x** | +54% |
| BITFIELD | 1.95x | 2.41x | +24% |
| HUFFMAN | 1.61x | 1.99x | +24% |
| LU DECOMPOSITION | 8.16x | 9.76x | +20% |
| FOURIER | 17.90x | 20.30x | +13% |
| NEURAL NET | 18.08x | 20.48x | +13% |
| NUMERIC SORT | 1.12x | 1.25x | +12% |
| ASSIGNMENT | 1.01x | 1.08x | +7% |
| IDEA | 1.14x | 1.16x | +2% |
| FP EMULATION | 1.01x | 1.01x | 0 |

The note in `tier1.rs` says removing the guard "bought nothing", and that is
true of **memory64** -- the engine emits its own check, so the trade was even.
It is not true of the guard itself, and the two had collapsed into one claim.
Dropping it is worth more than any single change measured this week.

`GEX_UNSAFE_NO_GUARD` is still not shippable: an address above `GUEST_LIMIT`
would alias low memory silently instead of faulting.

### Why it costs that much, and where the safe version is

Eight instructions per access -- `local.set`, `local.get`, `i32.wrap`,
`i32.const`, `local.get`, `i64.const`, `i64.lt_u`, `select` -- and the scratch
local is the expensive part, not the arithmetic: it forces a value through
memory-shaped state that V8 would otherwise keep in a register and reason
about.

Making it cheaper instruction-by-instruction is not the answer; several
encodings were considered and none beat eight. **Eliminating it where it
cannot fire is.** The guard exists only to catch an effective address that
does not fit in 32 bits: for anything below 2^32 the `i32.wrap` is the
identity and the engine's own bounds check already faults, because the
memory is exactly `GUEST_LIMIT` bytes.

So the question is which accesses can provably not exceed 2^32:

1. **Constants already skip it** -- `emit_address` folds them.
2. **Stack-relative accesses are the bulk of the rest**, and they can be made
   provable rather than proved: mask `sp` (and `x29`) to `GUEST_LIMIT` on
   every *write*. Writes to the stack pointer are rare; reads through it are
   everywhere. After that `[sp, #imm]` is a constant offset from a value known
   to be in range and needs no guard at all.
3. Anything else wants real value-range tracking, which is #15's territory.

(2) is the one to do, and the measurement above says what it is worth.

### ...but not the guards on the stack, which were already free

The obvious way to bank that 54 per cent is to stop guarding accesses that
cannot fail. `sp` can be clamped where it is *written* -- steered to
`POISON_ADDR` if it ever leaves the guest's range, so a wild stack pointer
still faults rather than aliasing -- and then every `[sp, #imm]` is provably
in range and needs nothing. Writes to `sp` are rare; reads through it are
everywhere.

Built, measured, reverted. It removed 29 per cent of the guard bytes
(926,109 to 655,061) and made nbench slightly *worse*:

| KERNEL | guarded | sp-clamped |
|---|---|---|
| BITFIELD | 1.95x | 1.80x |
| HUFFMAN | 1.61x | 1.42x |
| FOURIER | 17.90x | 16.77x |
| FP EMULATION | 1.01x | 0.88x |
| LU DECOMPOSITION | 8.16x | 8.65x |
| STRING SORT | 2.86x | 2.82x |

The comment already in `tier1.rs` says why, and it was right: *the guard is
pure arithmetic on a value that does not change, which is exactly what an
optimiser eliminates by itself.* A stack-relative guard is loop-invariant and
V8 hoists it, so those bytes cost nothing at run time -- while the clamp added
to every `sp` write lands in every function prologue and epilogue, where it is
real.

**So the 54 per cent is in guards on pointers V8 cannot reason about**: heap
traffic, chased structures, anything whose base changes each iteration. Those
are the ones to eliminate, and eliminating them needs to know a *dynamic*
value is in range, which is #15's territory and not a peephole.

Worth remembering as a shape: "fewer emitted instructions" and "less work"
are different claims once the engine has its own optimiser, and this file now
has two experiments saying so.

## Bigger regions are not the answer either, and one knob is not monotonic

The other 24 per cent of the module is block-boundary cost -- 15 per cent
register traffic, 5 safepoints, 4 dispatch -- and regions are what remove it,
so the obvious move is to form more of them. Only 366 of nbench's 27,594
blocks are inside one.

Two sweeps, against a baseline of 145 regions over 366 blocks:

| | regions | blocks in them | boundary |
|---|---|---|---|
| `GEX_REGION=8` (default) | 145 | 366 | 24% |
| `GEX_REGION=32` | 113 | 282 | 24% |
| `GEX_REGION=32 GEX_REGION_NESTED=1` | 316 | 2,761 | 23% |

**Raising the limit forms fewer regions.** That is not a tuning result, it is
a defect: `grow` explores up to `limit` blocks and then rejects the whole
region if it finds a back edge that does not reach the entry. A bigger budget
reaches a *second* loop that a smaller one never sees, so a region that formed
at 8 fails outright at 32. It should shrink the set to the one loop it can
form rather than give up -- the narrowing pass immediately above already knows
how.

**And 7.5x more region coverage moves the boundary cost by one point.** With
nesting allowed, 2,761 blocks are in regions instead of 366, and the result is
mixed: IDEA 1.14x to 1.30x, NUMERIC SORT 1.12x to 1.19x, STRING SORT 2.86x to
3.00x, against BITFIELD 1.95x to **1.22x** and NEURAL NET 18.08x to 17.42x.
Five better, four worse, one flat.

Nesting is also not shippable, and the code says why: a region polls for
signals and thread switches at its entry only, so a guest cycling inside an
inner loop never reaches a poll and never yields. The flag exists to price the
shape, and the price is "not obviously worth it".

So the 24 per cent is not mostly recoverable by folding more blocks together.
What remains of it is register traffic at genuine boundaries, and attacking
that means keeping guest registers in wasm locals across calls -- which is
#15, and a different piece of work from region formation.

### The non-monotonicity is load-bearing

Fixing it made things worse, which settles the question.

`grow` was changed to drop the block heading a second loop and re-narrow,
instead of rejecting the whole region. That restores monotonicity -- at the
default limit it formed **223 regions over 598 blocks** against 145 over 366 --
and nbench got worse on five kernels of ten:

| KERNEL | rejecting | shrinking |
|---|---|---|
| NUMERIC SORT | 1.12x | **0.84x** |
| FP EMULATION | 1.01x | **0.80x** |
| BITFIELD | 1.95x | 1.75x |
| HUFFMAN | 1.61x | 1.47x |
| ASSIGNMENT | 1.01x | 0.94x |
| LU DECOMPOSITION | 8.16x | 8.38x |
| NEURAL NET | 18.08x | 18.32x |
| STRING SORT | 2.86x | 2.98x |

Reverted. The regions the defect was refusing are regions not worth forming,
so the refusal is doing the job `worth_forming` was written to do -- its own
comment says as much: *the small accidental regions the forward walk found
were the ones worth forming*.

That is three experiments now saying the same thing from three directions:
raising the limit, allowing nesting, and fixing the rejection all form more
regions and all cost more than they save. **Region coverage is not the lever
on the 24 per cent.** Anyone returning to this should treat "only 366 of
27,594 blocks are in a region" as the intended state and not as a shortfall.

The remaining honest target is the register traffic itself -- keeping guest
registers in wasm locals across a call rather than writing them to the
context and reading them back -- which is #15 and is a change to what crosses
a boundary, not to how many boundaries there are.

## zstd: one deadlock, and the translated code is right

`zstd.sh` is the only real-program test failing, with `every thread is blocked
on a futex`. It predates this session -- bisected to 5694f33 with the kernel
wasm rebuilt for each arm, since the shim and the kernel are separate
artefacts and mismatching them gives a confusing import-type error instead.

The interesting part is what it is *not*. With identical flags gex writes
243,531 bytes where QEMU writes 269,300, which reads like wrong output until
the two are compared:

    cmp: EOF on gref.zst after byte 243531, in line 968

No differing byte before the end. gex's output is a byte-exact **prefix** of
QEMU's: the compression is correct and the tail is lost because the process
deadlocks before flushing. One defect, not two, and nothing wrong with the
lifter.

Also worth having: `-T1 --single-thread` still prints the message. So the
parking is not the worker pool failing to make progress -- with one thread the
work finishes and something still ends up parked, late, between the last write
and exit. Teardown of a pool created regardless of the flag is the best guess.

Two lessons for the next investigation of a hang:

* Compare with *matched arguments* before concluding anything about
  correctness. A size difference between two runs with different flags says
  nothing, and it nearly bought a codegen hunt that was not needed.
* A byte-exact prefix is a different failure from a wrong byte, and `cmp`
  tells the two apart in one command.

## What a process in the browser actually costs, measured end to end

Every number below is from the demo site, and each one contradicts something
that was believed before it was taken. The workload is ten `apt-config dump`
in one page -- twenty processes, no network, the same shape as apt-key's tail.

    pid 21 (the tenth):  797ms translating 25,006 blocks
                        2393ms installing 815 modules
                             (1550ms compile, 484ms instantiate, 317ms grow)

And the parts no counter had ever covered:

    worker exists, bundle loaded      25-28ms
    fork image copy, 15.4 MiB         39ms
    adopt (was 356ms)                 66ms

### Instantiation is not the cost, and never was

"Installing" was one number and is three. Instantiating is the *smallest* --
about 110ms for a process's translated modules, and **2-5ms** for the nine
megabyte shim plus the guest module together, which every Worker does. A
`WebAssembly.Module` is structured-cloneable and the engine shares the code
behind it, so an instance is only its own tables and globals.

What looked like instantiation cost was **growing the dispatch tables**: 269ms
per process, allocating and stub-filling 1.3 million entries before the guest
ran an instruction. That came from making every forked child grow to its
parent's whole slot total, which was a wrong fix for #114 -- the real cause
there was `enter` failing to mask the table index. Deferred adoption skips it
entirely and a child now starts in 66ms.

### Byte-identical modules would buy nothing: V8 has no content-keyed cache

Compiling the same 6.7 MiB module four times, under V8:

    56ms, 52ms, 55ms, 46ms

and a copy with one byte changed: 63ms. There is no dedupe. Chrome's wasm code
cache is tied to *streaming* compilation from a URL and persisted in the HTTP
cache; it does not apply to `WebAssembly.compile(bytes)` on freshly generated
bytes inside a Worker. So #99 -- making every process emit the same bytes --
cannot pay for itself through compile, however deterministic the slots are
made. Worth knowing before building deterministic slot assignment, which is
days of work.

### Compile is per *module*, not per byte

    6.7 MiB in one module           50ms
    815 small modules, one process  1550ms      (~1.9ms each)

Nearly all of it is fixed overhead. That points at fewer and larger modules --
but raising `PER_MISS` from 16 to 64 moved the count only from 815 to 793 and
the wall clock not at all, because most of those modules are not served
batches. They are units *this process translated itself*.

### The open question: 25,000 blocks lifted with the object already in the store

That is what is left. A process reports `reuse have=273 hit=241 miss=0
served=958` -- it finds what earlier processes published, installs nearly a
thousand of their modules -- and still lifts twenty-five thousand blocks. The
two facts together say the served units do not contain the addresses being
branched to, which is what the miss path's own comment says of them: a unit
holds what was reachable from the block that faulted in the process that built
it, so arriving at the same page by another route gets nothing from it.

Whole-object recompilation is what used to cover that, at 78,000 blocks a
process. It is now done once and recorded (`k_obj_translated`), so later
processes no longer redo it -- three `ls` went from fifteen seconds to five --
but they fall back to the miss path rather than picking the finished object
up. Making a process that meets a fully translated object take *all* of it,
rather than sixteen units at a time that mostly miss, is the next thing worth
measuring.

### It is not the network

With no HTTP anywhere, `apt-config` plus its forked `dpkg` still cost about
ten seconds an iteration. An earlier reading of the apt logs -- lots of
`nothing runnable, wanting the event loop` -- suggested waiting on Wisp, and
that was wrong.

## `/bin/true` costs 1.5 seconds and nine thousand blocks

The floor under every process, measured on the demo site with the apt closure
loaded. `/bin/true` does nothing whatever between `execve` and `exit`:

    pid 3: lived 1966ms,  435ms translating  9800 blocks,  260ms installing
    pid 4: lived 1774ms,  337ms translating  7895 blocks,  220ms installing
    pid 5: lived 1568ms,  250ms translating  9645 blocks,  189ms installing

Nine thousand blocks cannot be its own text -- `/bin/true` is about thirty
kilobytes. It is `ld.so` and libc, and **both are in the closure**. So the
closure is not reaching forked children, and that is worth more than anything
else on this list: it is the same libc that all eighty-five of apt's processes
re-translate.

Two things it is *not*, both checked rather than assumed:

- Not `gex_reset_directory` discarding the closure's entries. It runs before
  `#installClosure`, so `CLOSURE_DIRS` is empty when it would read them, and
  `#installClosure` writes its own entries afterwards regardless. Re-applying
  them there changes the block counts by exactly zero -- 20961, 9800, 7895 in
  both runs, byte for byte.
- Not the pins. The closure's libraries are placed where its manifest says and
  `exec: 1566 closure pages kept` shows those pages surviving `execve`.

Also unaccounted: about seventy per cent of a process's life. Adding up
everything measurable -- worker ready 26ms, fork image copy 39ms, adopt 66ms,
translating, installing -- reaches 500-900ms of a 1.5-2.0 second process, and
that is with the guest doing nothing. Whatever the rest is, it is not
translation, not compilation, not instantiation, and not the network.

### Closures cannot currently be combined

`cat`'s closure places libc at `0x5000000`; apt's places it at `0x5520000`.
Placement is a bump allocator within one closure, so two closures disagree
about where the same library lives, and a translated block has its addresses
baked in -- their modules are not interchangeable.

That is what stands between the current single-program closure and covering
the programs an `apt-get update` actually runs: `dpkg`, `apt-config`, `sed`,
`cat`, `base64`, `find`, `sort`. `gexc --pin FILE` already exists to impose a
layout, so a union closure over a set of programs is buildable rather than
blocked -- assign every object in the union a base once, then build each
program against that file.

## The closure is a net loss for a short-lived child, and by how much

Three `/bin/true` on the demo site, with the apt closure and without it:

                        blocks   translating   lived
    with the closure      9800         435ms   1966ms
    without               13031        368ms   1378ms

It is being consulted -- a quarter to a third of the blocks go away, and bash
drops from 34,015 to 20,961 -- and the process is *six hundred milliseconds
slower for it*. The reason is one line of the child's own narration:

    adopt 3: directory rebuilt: 0 entries kept   (+1ms)
    adopt 3: 720 modules reachable, 0 slots      (+399ms)   <- installClosure
    adopt 3: running                             485ms in

Every forked child re-registers all eighteen libraries, and that costs 399ms
against the ~100ms of translation it saves. An `apt-get update` forks
eighty-five times: thirty-four seconds of registering a closure that is
already described by the memory the child was handed.

### Why it is re-registered at all

A deferred child rebuilds its dispatch directory, because the slots its parent
*translated* belong to modules the child has not installed. That clears the
closure's entries too, and `#installClosure` puts them back -- which is where
the 399ms goes.

Preserving them across the rebuild is the obvious fix and does not work as
written: `CLOSURE_DIRS` is a per-Worker static filled by
`gex_add_closure_module`, and a freshly forked child's shim has never called
it, so there is nothing to preserve. Re-applying it there changes the block
counts by exactly zero, which is how that attempt was caught.

What would work is carrying the closure's page list across the fork, the way
`pages` and `shared` already travel, so the child can keep those entries
instead of rebuilding them from eighteen metadata sections. The entries are
already in the copied memory; only the knowledge of *which* of them are the
closure's is missing.

## What it would take for `/bin/true` to cost under 200ms

The target. It currently costs 1400-1900ms in the demo, and every term is now
measured rather than guessed.

    translating 9,800 blocks   ~330ms
    growing the tables         ~310ms
    installing                 ~210ms
    adopt, the rest             ~56ms
    fork image copy              39ms
    worker exists, bundle in     26ms

### The 9,800 blocks are coverage, not mechanism

`/bin/true` as pid 1, against a closure built **for it**, under the Node host:

    [gex] load: 1 pins, 2 closure modules to install
    [gex] pin ok: /lib/aarch64-linux-gnu/libc.so.6 wanted 0x5000000, landed 0x5000000
    [gex] run ended: code 0

and not one block translated. The whole run is fifteen lines.

So the closure machinery is right and the artefact was wrong. Apt's closure
and true's closure both contain libc -- 61,664 blocks of it for true -- but a
non-exhaustive build holds only what is *reachable from its own program*, and
the 9,800 blocks are libc paths that `apt-get` never walks. Building one
closure for apt and expecting it to serve `dpkg`, `sed`, `cat` and `/bin/true`
was the mistake.

The fix is a closure over the *set* of programs a demo runs. `gexc --pin FILE`
already exists to impose a shared layout, which is the blocker: placement is a
bump allocator within one closure, so `true`'s puts libc at `0x5000000` where
apt's puts it at `0x5520000`, and a translated block has its addresses baked
in. Assign every object in the union a base once, build each program against
that file, and their library modules become interchangeable.

Pointing the page at a closure built for another program is worth avoiding
loudly: `/bin/true`'s pins place its image at `0x1000000`, which is where the
page's `bash` already is, and the run dies with `memory access out of bounds`
inside a syscall. A manifest should probably name the program it was built
for, and the loader refuse one that does not match pid 1.

### And growing the tables lazily

310ms of every process is sizing the dispatch tables for the entire closure --
1,607,680 slots -- before the guest runs an instruction. That defeats the
laziness the closure already has: it defers *instantiating* the eighteen
libraries and then grows for all of them anyway.

The coupling to break: a page's directory entry has to be written for the miss
path to find its owner, and a branch into a page whose slot is past the end of
a table traps rather than missing. `gex_closure_owner` already answers "which
library covers this page" without the entry being there, so a page can be left
out of the directory until its library is filled.

Together those are ~850ms of the ~1500, and what remains -- 120ms of fork
copy, Worker start and adopt, plus what the guest actually executes -- is
inside the target.

### Every executable lifts the whole dynamic linker again

`/bin/true` on the demo site translates 4,312 blocks and attributes all of them
to a region named `[image]`. The name is the tell. Two places load an ELF, and
both hand the interpreter's segments the *executable's* identity:

```rust
for seg in program.segments() { ... }        // exe.segments.chain(interp.segments)
h.kernel.mem.insert_loaded(..., Some((path.to_string(), seg.vaddr)));
```

`Program::segments` flattens the executable and its interpreter into one
iterator, so `ld.so`'s text at `INTERP_BASE` is recorded as
`("/bin/true", 0x3000000)` under `true` and `("/bin/ls", 0x3000000)` under
`ls`. The shared module store keys on exactly that pair. Two processes running
*the same linker at the same address* therefore never agree that it is the same
code, and every distinct executable in a run lifts the entire dynamic linker
from scratch. A shell that runs forty commands pays for it forty times.

`gex_load` is worse than wrong, it is absent: it uses plain `insert`, so
process 1's image *and* its interpreter carry no code identity at all. `gex_load`
is handed bytes and no path so the image genuinely cannot be named -- but the
interpreter can, and the interpreter is the one object every other process in
the run also loads.

`Program::placements()` yields `(segment, Option<&interpreter path>)` and both
load sites use it.

### A pin that does not take was invisible in the only configuration that matters

`gex_pin_report` is called from `worker.mjs` after `gex.run` returns. In a shell
demo process 1 is the shell and it does not return, and adopted children never
call it at all -- so the report that says "the module was built for one address
and the code runs at another" printed in the isolated Node run and never once on
the demo site. Days of "is the closure even being used" had an answer that was
being computed and thrown away.

`report_new_pins` now runs from the syscall entry beside `check_host_magic`, on
the same terms: a load and a compare, once per syscall, saying each pin once.
A misplacement is loud regardless of tracing, because the guest is about to
branch into code built for a different address.

### The closure was unreachable, and every part of it checked out

A miss asks the host to fill a page only when `FILL_POSSIBLE` says a module of
this process can answer. That flag was set in exactly one place --
`#installModule`, when a module with passive segments is *instantiated* -- and
a closure library is deliberately never instantiated until a fill asks for it.

Nothing instantiated, so nothing armed. Nothing armed, so no miss asked.
No miss asked, so nothing was instantiated. Every branch into libc and into
`ld.so` fell straight through to the runtime translator while a closure
covering both of them whole sat in the process doing nothing.

It survived this long because each part of it verifies:

- the pin lands -- `pin ok: libc.so.6 wanted 0x5520000, landed 0x5520000`;
- the module claims the pages -- every address lifted in libc and in ld.so is
  on a page the module's own `gex.meta` names, checked entry by entry;
- the blocks are in the module -- libc is 355 pages and 295,294 blocks, and
  the linker 39 pages and 30,627 for a `.text` only 28,813 addresses long;
- `--exhaustive-for` matched: `libc.so.6`.starts_with(`libc.so`) is true, and
  the module is 67.9 MB.

Four theories died against those facts before the flag came into view. What
finally pointed at it was counting the lifts that happen on a page some
closure library already claims -- there should be none, and there were 16,380.
That counter (`[closure] page unfilled`) stays.

Measured, `bash -c /bin/true` on the demo site:

    pid 1       34,998 blocks -> 18,618    6712ms -> 4987ms   765 modules -> 343
                libc 10,239 -> 0, ld.so 6,141 -> 0
    /bin/true   10,308 blocks ->  4,307    1950ms -> 1568ms    85 modules ->  18

### A closure has one image, so build it for the program that is pid 1

`resolve_all` makes only the first root's image an object: two images would
both want `PIE_BASE` and the second's dispatch entries would land on the
first's pages. The demo's closure was built for `apt-get` while its pid 1 is
`bash`, so bash was the one program in the run with no module at all -- and
once libc and ld.so were reachable, bash was *everything* that was left:
18,595 blocks in pid 1 and another 4,106 in every child it forks.

Built with `bash` as the first root and apt, dpkg, apt-key, gpgv and
`/usr/lib/apt/methods` as extra roots, the closure is 33 objects and 190 MB
against 82, and covers the union of what the demo executes. The extra roots
contribute their libraries, which is where their code actually is: apt's
`file` method is 67 KiB against libseccomp's 180.

The manifest now names the program its image module was built from, and
`loadClosure` is given the path pid 1 will run and checks the two match. The
failure it prevents is silent -- the guest starts inside another program's
code and exits without making a syscall, which reads as a program that decided
to do nothing.

### The three costs that were left, and what they turned out to be

With the closure finally reachable, `bash -c /bin/true` on the demo site had
three costs left and none of them was translation.

**The image module was the guest module.** A closure's first object is emitted
`Linkage::Own` -- it declares the memory and the tables everything else
imports -- so in a browser it replaces the seed, and every process
instantiates it. `bash` whole is 42 MB and 212,000 functions, and a forked
child on its way to `execve` apt's http method paid 256ms for a shell it never
enters. The seed already is an `Own` module that translates nothing, so
`gexc --shared-image` emits the image as an ordinary `Shared` library:
registered for its pages, instantiated by the first branch that lands in one.
Worker start 293ms -> 14ms.

**Registration published.** Writing a directory entry commits the table to
being long enough to hold the slot it names, so registering all thirty-three
libraries grew every process to the closure's 2,850,816 slots: 405ms in pid 1,
524ms in a child, 22 MB of table each. Publication is now separate and happens
in `fillSlot`, for the one library a branch has asked about. Four libraries
get published in a `/bin/true`, not thirty-three.

**Runtime slots forced it anyway.** The global slot counter started at zero and
was advanced past the closure's reservation, so the first block a process
lifted sat above 2.85M and the table had to be that long regardless -- and
every process lifts something, if only its own program's text. The space is
now partitioned: closures own tables 0..3 with their numbers baked in at build
time, the run translates into tables 4..7 from a counter starting at
`RUNTIME_SLOT_BASE`. Growth became per table to match, since one running total
across the virtual space turns "table 4's base plus 2,048" into ten million
entries in each of tables 0..3.

    /bin/true   lived 2014ms -> 965ms
    adopt        715ms -> 81ms, of which 30ms is a debug-only checksum
    grow         405-524ms -> 155ms once, then 0-1ms per library

What is left in the child, measured: 155ms growing, 67ms installing fourteen
modules, 36ms for the parent's image copy, 26ms lifting `true`'s own 201
blocks, 11ms of Worker start. Roughly 375ms of 965 accounted for; the rest is
not yet attributed and is the next thing to instrument.

### An image is not a library: what `--shared-image` broke

`execve` keeps the closure's directory entries, on the reasoning that "a
closure library covers the object it was built from whatever the process goes
on to run". That is true of a library, which is pinned to an address of its
own. It is false of the *image*, which sits at `PIE_BASE` and is replaced by
the very call that keeps it -- and since `--shared-image` the image arrives
here as a library like any other.

So after `bash` forked and the child exec'd `ls`, the directory still pointed
the pages at `0x1000000` -- now `ls` -- at bash's translated blocks. `ls | wc -l`
died with `code 132, pc=0x1003f44`: SIGILL, inside `ls`, executing a shell.
Then `wc` waited on a pipe nobody would write to and pid 1 waited on `wait4`,
which is what "1024 turns with nothing runnable" was reporting.

`/bin/true` hid it, twice over: it exits 0 whatever it executes, and the
marker the test greps for is printed by bash rather than by the command. A
check that passes without the guest doing the right thing is the same defect
this file already records twice.

`apt-get update` failed the same way and looked entirely different -- two
processes, a stalled pipe, no output at all -- which is why it read as a
scaling problem. A `ls | wc -l` reproduces it in ninety seconds; the apt run
takes fifteen minutes to say less.

Everything below `INTERP_BASE` is the executable and its break, so that is
what `execve` now drops.

### apt's deadlock is one child crashing inside a syscall

`apt-get update` stalls two processes in, with `apt-get` blocked on `read` of a
pipe holding nothing and pid 1 blocked on `wait4` behind it. The reason is not
scale, not the pipe, and not anything about the closure.

    [gex] run ended: ABANDONED, code 0, pc=0x0, in syscall 56
    [gex] pid 3: lived 2418ms, 80ms translating 1844 blocks [dpkg@0x1000000=1826 ...]

pid 3 is `dpkg`, which apt runs first. It does not exit: it abandons the wasm
stack inside `openat`, with no status and no resume address. So `exit_process`
never runs, its end of the pipe stays open, and the reader waits for an
end-of-file nothing will send.

Three facts narrow it a long way:

- `trap()` clears `IN_SYSCALL`, and the report says `in syscall 56`. So the
  guest did not leave through the shim's own redirect. It died *inside* the
  `openat` handler.
- `InSyscall`'s `Drop` clears the flag on a normal return, so the handler did
  not return either.
- The shim's panic hook writes `gex: panic in the shim:` to fd 2 and nothing
  did. So it is not a Rust panic.

A wasm trap that is neither `trap()` nor a panic is a fault: an out-of-bounds
access, an indirect call type mismatch, or a stack overflow. `openat` on this
path goes through the remote filesystem into the *global kernel*, which runs
on a per-Worker stack slice handed out by `#nextKernelStack`, and a slice that
is too small traps exactly like this with nothing said.

Two things made this take as long as it did, and both are now fixed:
`GEX_STDERR=all` printed only on a *passing* run, so every failing run showed
twenty lines of terminal and none of the runtime's own account; and
`run ended: shim trapped out, code 0` was printed identically for a program
that finished and one that died with no status set -- the flag that knows the
difference, `gex_exited`, has existed all along and was not being read.

### apt reaches the network

With a crashed process taking itself down and a forked child's pipe ends
recorded against the child, `apt-get update` gets past `dpkg` and downloads:

    Get:1 http://deb.debian.org/debian bookworm InRelease [151 kB]

`dpkg` still faults -- `run ended: ABANDONED, code 0, pc=0x0, in syscall 56`,
followed now by `pid 3 crashed; 1 pipe ends released, status 134` where it
used to say `0`. What changed is that the run survives it. (An earlier reading
of this run said no process crashed; that grep ran before the stderr dump was
written, and the dump is the only place a failing run says anything.) The
parent
reopens a forked child's pipe ends in the global kernel and was recording them
under its own pid, so `k_pipe_forget` had every process releasing ends that
belonged to somebody else, and a descriptor could be shut under a process that
was still using it.

The stall has moved from pipe 0, which is `dpkg`'s, to pipe 9, which is the
http method's, after the method has reported the size and before it reports
the download done. A much later failure than two processes and no output.

What the run is waiting on, in its own words:

    2.0=net([5 shared pipe 1 read, 0 buffered want0x1 have0x0])
    run ended: exited, code 100

`apt-get` is polling a method's pipe for readable and getting nothing, and a
method has exited 100 -- which is what apt's methods return when they fail.
So two defects remain and they are separate: `dpkg`'s fault inside `openat`,
which is now survivable, and a method that gives up.

### The fault follows the process, not the syscall

`/var/lib/dpkg/arch` does not exist in the demo rootfs. Creating it first --
`touch /var/lib/dpkg/arch; apt-get update` -- does not stop dpkg faulting, but
it *moves* the fault:

    ABANDONED, code 0, pc=0x0, in syscall 56   (openat)   without the file
    ABANDONED, code 0, pc=0x0, in syscall 57   (close)    with it

Same process, same last path, different syscall. So the syscall is not the
cause: dpkg is dying at whichever call it happens to be in. That rules out the
`openat` handler as a site and points at the process itself.

It is not the adopted image either. The per-region survey says `1 of 88
regions differ` for every one of apt's children, and the one that differs is
`0..300000`, the shim's own low memory, which is expected and documented. The
copy is faithful.

Two things did improve. With the file present apt reaches ten processes rather
than three, and the run keeps going after dpkg dies -- so the missing file is
worth putting in the rootfs on its own merits, quite apart from the fault.
What is left in that run is methods ending oddly: `exited, code 100`, `code
111`, and `code 130` at `pselect6`.

### The image pages were a real bug and not dpkg's

`?closure=0` produces no crash at all; with the closure, dpkg dies. That says
the fault is in what the guest is handed, and the obvious candidate was the
image library: it claims `PIE_BASE`, every program lands there, and `#publish`
wrote its pages on the first branch into a claimed page even in a process
running something else. That is fixed and it was worth fixing.

It is not why dpkg dies. With the pages refused -- dpkg's attribution is
`dpkg@0x1000000=1826` and nothing else, so it is lifting its own text as it
should -- the fault is unchanged:

    run ended: ABANDONED, code 0, pc=0x0, in syscall 56
    pid 3 crashed; 1 pipe ends released, status 134

So something else the closure supplies is wrong for dpkg, and the libraries
are the only thing left: dpkg loads libmd, libselinux, libpcre2, libzstd,
liblzma, libbz2 and libz, all pinned and all shared with programs that do not
crash. The next thing to check is whether dpkg loads an object the closure
does *not* cover into an address the closure pinned for something else --
`pin MISPLACED` would say so and does not appear, but a library placed by the
bump allocator over a pinned range would not be a pin at all.

### A thirty-second repro for the apt crash

apt forks `dpkg` before it touches the network, so the proxy is not needed and
`debian-cmd` will do:

    GEX_STDERR=all GEX_WALL=110 \
      GEX_QUERY='cmd=apt-get%20update%3B%20echo%20GEX-CMD-DONE&debug=1&recompile=0' \
      node web/test/site.mjs debian-cmd

It reproduces every symptom in about thirty seconds against seven minutes for
the networked run, and it shows more than the networked one did: two children
crash, not one, and some processes end `code 134, in syscall 131` -- a guest
calling `abort` -- rather than abandoning the stack.

Worth writing down as a method, not just a command. Every question asked of
this failure so far has cost a full `apt-get update`, and most of them did not
need the network, the mirror, or the download. The expensive part of the run
was not the part under investigation.
