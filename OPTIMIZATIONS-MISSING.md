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
    the fact is not propagated); (d) the structural fix — a `memory64` guest
    memory with a 2^33-byte reservation, where the engine's own bound check
    subsumes the select entirely.
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

# Ranked by expected value

1. **`cmp`+`b.cond` fusion** (§1–4) — largest pure-codegen win, touches every
   block.
2. **Return-address stack + indirect inline caches** (§21–22) — PLAN specifies
   both; `ret` is the hottest indirect branch in any program.
3. **Browser code cache in IndexedDB** (§49) — not codegen, likely the largest
   user-visible improvement.
4. **Library-call substitution** (§71) — beats codegen on exactly the kernels
   where gex trails QEMU.
5. **Redundant-load elimination + stack-slot promotion** (§13, §15) — attacks
   the spill traffic regions only partly fix.
6. **Guard elision via range facts or `memory64`** (§16) — five instructions
   off every memory access.
