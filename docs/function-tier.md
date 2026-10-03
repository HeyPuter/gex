# The function tier: GIR → C → clang → wasm, inside gex modules

Stream S3 of the pewasm merge (`docs/merge-plan.md` §4, §6 M2, §8). This note
fixes the mechanics before the bulk of the code: how clang-built code shares
gex's memory and tables, how it reads the context, how it keeps tier-1's
memory semantics, and how it leaves and re-enters code it does not own.

Two phases:

* **Phase A (blocks).** Every tier-1 block becomes one C function with the
  uniform `(ctx)` signature, chained with `musttail`. Same GIR, same
  write-backs, same safepoints as tier-1; only the code generator differs.
  It proves the plumbing and prices clang against gex's own emitter on
  identical input.
* **Phase B (functions).** One C function per recovered guest function (S2's
  function IR): registers and flags as C locals across blocks, written back
  only where §4.1 of the merge plan requires.

## 1. Where the code lives: spliced into the tier-1 module

The function tier does not produce a second kind of module. For a translation
unit (an image, or later a closure object) it:

1. builds the ordinary tier-1 module from the blocks, exactly as today
   (`ModuleBuilder`, any `Linkage`);
2. emits C for the same blocks (Phase B: for the functions that own them),
   compiles it with `clang --target=wasm32` and links it with `wasm-ld` into a
   standalone module whose imports are gex's host functions (`gex.syscall`,
   …), the memory (`env.memory`), and one synthetic function,
   `gexc.dispatch : (i32) -> ()`;
3. **splices** that module into the tier-1 module (`gex-cgen::splice`, on
   `wasmparser` + `wasm-encoder`'s reencoder, both already in the lockfile):
   * clang's function types and bodies are appended, with function indices
     remapped; its host imports are resolved by `(module, name)` against the
     tier-1 module's own imports, and its memory against memory 0;
   * `gexc.dispatch` becomes a local function that does exactly what tier-1's
     `emit_dispatch_tail_call` does (records `LAST_TARGET`/`LAST_SOURCE`,
     two-level directory lookup, `br_table` over the eight dispatch tables,
     `return_call_indirect`), reading the target from `ctx::PC`;
   * each block the C side owns is retargeted: its tier-1 body is replaced by
     `local.get 0; return_call $c_fn`, and the element-segment entries naming
     it are pointed at `$c_fn` directly, so dispatch pays nothing extra.
     Trampolines (x86-64 shared slots) and `enter` reach it through the stub.

The result has *the same* imports, exports, tables, element layout, data
segments and `gex.meta` as the tier-1 module it came from. So both hosts
install it with no change at all: wasmtime (`shims/native`, via
`gex_xlate::translate_image` when `Options::cgen` is set) and the browser (a
`gexc build --cgen` module is loaded like any `gexc build` module). A `Shared`
module keeps importing the tables and the memory; an `Own` one keeps defining
them. Blocks the C side does not own (Phase B: interior entries, anything
S2's function IR does not cover) keep their tier-1 bodies in the same module,
which is what "both tiers coexist per address" (merge plan §2) means
concretely.

Alternatives rejected:

* *A second module the host instantiates beside the tier-1 one.* Every host
  (wasmtime, the browser shim, the Worker-per-thread instantiation, the
  demand-fill machinery) would need to learn about it, and every C→tier-1
  transfer would be a cross-instance tail call.
* *wasm-ld producing the whole gex module.* It cannot express gex's element
  layout (per-page segments at slots fixed by the `PageTable`, passive
  segments with `gexfill*` helpers) or its eight tables.
* *Dispatch written in C* (`__builtin_wasm_table_get` on `__funcref` tables).
  Clang lowers a funcref call through a private table, not a
  `return_call_indirect` into the dispatch tables. The synthetic import keeps
  the C side free of table plumbing.

The splice **fails loudly** (a build error naming the function) if the linked
C module has anything the splice cannot place: a data segment, a global
access (clang's `__stack_pointer` included, §4), a table of its own, a
`call` (not `return_call`) to a C block function or to `gexc.dispatch` in
Phase A, or an import it cannot resolve (e.g. a `memmove` libcall).

## 2. The context, through a header generated from gex-abi

`gex-cgen` writes `gex_abi.h` from `gex_abi` itself (a Rust function, so the
header cannot drift): every `ctx::*` offset, `ctx::SIZE`, `GUEST_LIMIT`,
`POISON_ADDR`, the dispatch constants, the import signatures with
`import_module("gex")`/`import_name(...)` attributes, and the memory
helpers below. A unit test compiles the header and checks a few offsets
through `_Static_assert` against values printed from Rust.

C functions take `int ctx` (wasm `i32`, the same ABI tier-1 and the shims
use) and address fields as `*(u64u *)(GEX_CTXP(ctx) + GEX_CTX_PC)`, where
`GEX_CTXP` casts to `char *`. Offsets are constants on a pointer, so they
fold into the access's memarg offset. The flags follow tier-1's layout: one
byte each, four packed into one `i32` store when all four are cached.

## 3. Guest memory: tier-1's semantics exactly

Guest VA is the linear-memory offset, and a C pointer *is* a linear-memory
offset in wasm32, so a guest access is a plain C access through a pointer
typed `__attribute__((aligned(1), may_alias))`. Compiled with
`-fno-strict-aliasing -fno-delete-null-pointer-checks` (guest address 0 is an
address, not UB) and `-ffp-contract=off`.

The effective address follows `tier1::emit_address` case by case:

| GIR access | tier-1 | C |
| --- | --- | --- |
| constant address | folded; `≥ GUEST_LIMIT` → `POISON_ADDR` | the same constant |
| `base + off`, `0 ≤ off ≤ 0xffff` | guard(base), `off` in the memarg (33-bit sum, cannot wrap) | `(char *)G(base) + off`, where `G` steers to **`GUEST_LIMIT`** |
| `0xffff < off ≤ max_static` (AArch64 only) | as above | `e = G(base) + off` in 64 bits; `e ≥ 2^32` → `POISON_ADDR` |
| anything else (negative, x86-64 displacements > 64 KiB) | guard(base + off) | `G(base + off)` |

The one deliberate difference is in the middle row: C steers a bad base to
`GUEST_LIMIT` rather than `POISON_ADDR`. With `POISON_ADDR`, correctness would
depend on clang folding `off` into the memarg (it does, for `inbounds nuw`
GEPs, but that is an optimisation and not a promise); an unfolded `i32.add`
would wrap `0xfffffff0 + 16` to address 0 and read low memory instead of
trapping. From `GUEST_LIMIT`, `off ≤ 0xffff` plus any access size stays below
2^32 and above the memory's end, so the access traps whether or not the add is
folded. Both trap; the host sees the same fault. Bulk operations (`MemCopy`,
`MemFill`, `StrOp`, `MemCmp`) have no static offset and use `POISON_ADDR` as
tier-1 does, keeping its length clamp bit for bit (the clamp's behaviour on a
poisoned base depends on the constant).

Ordering: plain GIR accesses are plain C accesses — clang may reorder and
merge them exactly as far as V8/Cranelift may reorder tier-1's plain wasm
accesses (guest data races stay races, PLAN §5.8). `Order::SeqCst` and the
RMW/CAS expressions use `__atomic_*`/`__sync_val_compare_and_swap` on
naturally typed pointers, which lower to the same wasm atomics tier-1 emits,
misalignment trap included. `Fence` is `__atomic_thread_fence(SEQ_CST)`.
`MemCopy`/`MemFill` are `__builtin_memmove`/`__builtin_memset`, lowered to
`memory.copy`/`memory.fill` under `-mbulk-memory` (a residual libcall is an
unresolved import, so the splice fails loudly rather than miscompiling).

Floating point goes through `__builtin_bit_cast`; min/max, saturating
truncation, nearest and the SIMD ops use the `__builtin_wasm_*` builtins and
`wasm_simd128.h`, which map one-to-one onto the instructions tier-1 emits.
FMA reuses tier-1's exact algorithms (`fma.rs`): round-to-odd for `f32`,
guarded Dekker for `f64` with the `gex.fma` import as its fallback. Nothing
is left to libm.

## 4. clang's shadow stack

Guest memory *is* the linear memory, and `wasm-ld` places its stack (and
`__stack_pointer`'s initial value) at low addresses — inside gex's reserved
region, where the thread contexts live. A spill there would corrupt a
context silently. So:

* the emitted C never takes the address of a local, never uses arrays,
  structs by value, varargs or `alloca`; every value is a scalar or a `v128_t`,
  and wasm locals are unlimited, so LLVM has no reason to touch the shadow
  stack;
* this is **checked on every build**: the splice rejects any C function that
  reads or writes any global (the only globals `wasm-ld` defines are
  `__stack_pointer` and TLS bookkeeping) and any module with a data segment,
  naming the offender. No build can ship a function that uses the region.

Fallback if a future pass needs a real C stack (a large promoted frame, say):
wasm globals are per instance, and gex instantiates the module once per thread
(Worker per thread), so `__stack_pointer` is already per thread; it only needs
an initial value naming a reserved per-thread region. That region is a
`gex-abi` change (carved beside each context or below each guest stack) and
goes through the coordinator. Not built; the check above keeps it from being
needed silently.

## 5. Syscalls and safepoints from C

The C side calls the same imports with the same arguments at the same points
as tier-1:

* **Syscall** (`Term::Syscall`, `HostFunc::Syscall`): write back every dirty
  cached register, store `ctx::PC = next`, call `gex.syscall(ctx, svc_pc)`,
  forget every cached register (reload lazily), continue at `next`.
* **Safepoint**: the `ctx::SAFEPOINT_COUNTER` countdown on backward direct
  branches and on every indirect branch, calling `gex.safepoint(ctx, pc)` when
  it expires, with the registers written back first. Not read back
  afterwards, as in tier-1: the host only changes guest state on a path that
  does not return (it traps out with `StopExecution`/`resume_at` and
  re-enters through `enter`).
* Other clobbering host calls (`illegal`, `breakpoint`, `sysreg_write`) the
  same way; `cache_op`, `sysreg_read` and `fma` do not flush.

In Phase A no C frame ever nests (every transfer is a tail call, which the
splice verifies), so fork, execve, thread migration and signal delivery see
exactly tier-1's flat stack.

In Phase B C frames nest (a guest call is a wasm call). The host's existing
escape — trap out of the whole wasm stack and resume from `ctx` — is then
correct **provided every frame's state is architectural at every point the
host can act on**. Phase B guarantees that without landing pads. Registers
and flags are C locals across the whole function (loaded at entry, reloaded
after every call and clobbering host call), and write-backs follow one
invariant.

### 5.1 The write-back invariant

> **At every host-visible point — a syscall, a safepoint, a clobbering host
> call, a trap, a return to dispatch — all *live* architectural state is
> exact in the context. Flags that no continuation reads are unspecified,
> exactly as tier-1's dead-flag elimination already allows.**

Concretely (`func.rs`):

* Only registers that may differ from the context are stored (clean-spill
  elision: `dirty_in`, a forward may-analysis; a call, syscall or clobbering
  host call makes everything clean again, since everything is reloaded).
* Every exit that leaves the function (return to dispatch, tail call,
  syscall, unknown or indirect call, unresolved jump, switch default) and
  every safepoint write back **every dirty register**, flags excepted: the
  flags are written only where they may be read (`live_in`, a backward
  liveness over the function; at a safepoint, the loop head's live set).
* A call to a function-tier callee writes back every dirty non-flag
  register, plus the flags the callee's summary reads or the continuation
  reads — unless the callee is **trap-free** (`trap_free`): S2's summary
  says it cannot unwind, leave pending or reach unknown code, and it has no
  syscall, no safepoint (no backward edge, no tail call), no clobbering host
  call, no exit from the tier, no indirect call and no recursion, and calls
  only trap-free functions. Nothing the host observes or resumes from can
  happen before a trap-free callee returns, so its callers store only what
  it reads plus what their continuation reads. 15% of the known call sites
  of the Tier 0 kernels (AArch64) qualify, 19% of the functions.
* A summarised callee's `preserved` registers are not reloaded after it
  returns as expected.
* A guest `ret` that goes where the caller expects writes back every dirty
  non-flag register, and the flags only if the caller's continuation reads
  one (§7, predicted returns).

The one theoretical exposure: a signal handler reading its `ucontext`, or an
NT `CONTEXT` capture, can see stale dead flags. Tier-1 shows the same today.

Validation (`crates/gex-cgen/tools/lockstep-gate`, part of Gate B and of
every later pass's gate) compares against tier-1 syscall by syscall twice:
(a) the default build with the flags left out (`lockstep --ignore-flags`),
and (b) `GEX_CGEN_LIVE=0`, which writes every flag back at every exit,
compared exactly. A divergence in (a), or in a non-flag register in (b), is a
bug. `GEX_CGEN_MUTATE=IGNOREUNWIND` (every summarised callee treated as
trap-free) and `=NORETCHECK` (every return treated as expected) are the
mutations that show the rules are needed.

Landing pads (S7) become necessary exactly when a pass keeps live state in
locals across a call that is not trap-free (callee-saved registers, promoted
stack slots); the `may_unwind` summaries then decide where.

## 6. Tail calls

Every function the C side registers or chains to has the C type
`void f(int ctx)`, i.e. wasm `(i32) -> ()`, identical to tier-1's block type,
so `__attribute__((musttail))` is always legal (clang requires identical
signatures) and lowers to `return_call` under `-mtail-call`. Unknown or
computed targets store `ctx::PC` and `musttail` into `gexc_dispatch(ctx)`,
which the splice turns into the dispatch function of §1. (Phase B's bodies
share one internal signature of their own, `int f(ctx, depth, need, ret)`,
so `musttail` between them is equally legal; only `F_entry` has the uniform
one.) Observed with clang
22: a cycle of `musttail` among functions of one translation unit becomes a
loop after inlining, so Phase A already gets some loop formation for free (the
context traffic stays, since the stores are to memory).

## 7. Calls into code the function tier does not own

The proposed rule: "a call or jump to non-function-tier code writes back all
state and tail-calls into dispatch, so the caller's continuation runs in
tier-1". The continuation-in-tier-1 half is right; the tail-call half breaks
once frames nest.

Take H (C) calling F (C) with a wasm call, and F calling unknown G. If F
tail-calls dispatch, F's frame becomes G's tier-1 chain, but H's frame (and
the root below it) stays on the wasm stack. G returns into F's interior
(tier-1), F returns into H's interior (tier-1), and the program carries on in
tier-1 with H's frame stranded underneath; tier-1 chains never return, so it
is never popped. If the loop that calls F now runs in tier-1, every iteration
enters F through dispatch, nests, calls G and strands another frame: unbounded
wasm stack growth. So the rule is:

> **Leaving the function tier unwinds the C nest by returning.** The leaving
> function writes back all state, sets `ctx::PC` to the target, and *returns*.
> Every C call site checks after the call (the §4.2 return check: `ctx::PC` is
> the return address the caller set up, `SP` is back where it was); on a
> mismatch it returns too, with nothing to write back (under §5's baseline
> scheme it wrote everything back before the call and has executed nothing
> since). The nest root then tail-calls dispatch on `ctx::PC`.

Each function-tier function therefore has two forms:

* `int F_body(ctx, depth, need, ret)`: the code. C callers call it with a
  wasm call. `ret` is the return address the caller set up (in x30, or the
  slot the x86-64 `call` pushed); `need` says whether the caller's
  continuation reads a flag; `depth` is §7.1's.
* `F_entry(ctx)`: registered in the dispatch table with the uniform signature
  (§4.1.2). `F_body(ctx, 0, all, ~0); musttail gexc_dispatch(ctx);` — it is
  the nest root: whatever made `F_body` return (a real return, `longjmp`, an
  exit to tier-1), `ctx::PC` says where to go next, and the root polls and
  goes there flat.

**Predicted returns.** The return check lives in the callee, which knows
both halves of it: a guest `ret` whose target is `ret` with SP back at its
entry value (plus the popped slot on x86-64) is the return the caller
expects. The callee writes back every dirty non-flag register (and the flags
if `need` asks) and returns **1**: the caller carries on without reading the
context. Any other exit — an unexpected return target, a tail call or jump to
code the tier does not own, a switch default, an unresolved jump, the depth
guard — writes back everything, sets `ctx::PC`, and returns **0**; every C
caller then returns 0 too, with nothing to write back, and the root
dispatches on `ctx::PC`. A return stores no PC and does not poll: every
dynamic cycle still passes a loop back-edge, a known tail call (which polls)
or the root (which polls).

This is pewasm's pending-jump trampoline (`recomp_jmp_pending` +
`recomp_run_pending`) with the pending flag replaced by the callee's
prediction, and it is §4.2's return check, moved to where it is cheapest:
one compare in the callee, one test of the result in the caller. It handles
`longjmp` (the `ret` into setjmp's frame is not the one expected, so every
frame on the way down returns 0 and the root resumes there in tier-1), `ret`
used as a computed jump, and coroutine stack switches the same way.

Guest tail calls (`b G` to another function's entry) are C `musttail`
`G_body(ctx, depth, need, ret)`: G returns to F's caller with F's
prediction. Indirect calls (`blr`) compare the target against the module's
function entries (a `switch`, lowered to compares or a `br_table`) and call
the matching `_body`; anything else leaves as above. That is the slow path
pewasm's inline dispatch-cache probe and profile-guided devirtualization
later speed up; it is priced before being optimised.

Alternatives considered: tail-calling dispatch in place (strands frames, as
above); a nested wasm call into tier-1 that waits for it to return (tier-1
never returns); a Wasm exception to unwind the nest (zero cost on the normal
path, but the return check is needed anyway for `longjmp`, and C cannot catch
Wasm exceptions without C++ EH — S7 measures exceptions against the explicit
flag for its landing pads, and may switch this path over if it wins).

### 7.1 The recursion-depth guard

Nesting follows guest call depth inside the function tier, so without a
bound deep guest recursion would be limited by the engine's stack rather than
the guest's 8 MiB: 100,000 frames of the soundness corpus's `deep_recursion`
exhaust wasmtime (`max_wasm_stack`, 512 KiB: ~5,000 frames of that function)
and V8 (~984 KiB: "Maximum call stack size exceeded" at ~5,500). So the
nest's stack use is bounded, with the same leave-by-returning mechanism:

* `F_body`'s `depth` parameter is an estimate, in bytes, of the
  engine stack the C frames below it in this nest use. The root passes 0;
  a call passes `depth + W(F)`, where `W(F)` is the
  caller's frame estimate (`64 + 3 ×` the bytes of the registers it keeps in
  locals; clang's wasm locals measured up to ~2.5× those bytes on coremark,
  and the rest covers the engine's spill slots and frame header). A guest
  tail call (`musttail`) passes `depth` unchanged: the frame is replaced.
  An indirect call through the entry `switch` adds that helper's frame too.
* `F_body`'s first statement, before any register is loaded: if
  `depth > BUDGET − RESERVE − W(F)`, set `ctx::PC = F` and return 0. Every
  caller of a function with a guard wrote everything back before the call
  (§5.1: a function with a guard is not trap-free), so this is exactly "the
  caller left for F" (§7): the caller returns 0, and so does every frame of
  the nest; the root
  dispatches on `ctx::PC = F`, which enters `F_entry` and starts a new nest
  at depth 0. The frames that were unwound finish in tier-1 when their
  callees return into them (their return addresses are real guest state).
* It needs no context field and no `gex-abi` change: the depth lives in a
  wasm parameter, so it is per nest by construction. The StopExecution
  unwind discards it with the rest of the wasm stack, and a nest the host
  starts afresh (the root after `resume_at`, a nested guest loop) starts at
  zero, with its own budget on top of whatever stack its caller used.
* Trap-free functions (§5.1) have no guard: their callers need not have
  written everything back, so they must not leave. They are not recursive,
  and a trap-free function's whole subtree must fit in `RESERVE` (64 KiB of
  estimate), which every guarded function keeps free below the budget.
* `BUDGET` is 384 KiB of estimate (`GEX_CGEN_STACK_BUDGET` overrides it).
  The estimate is conservative against both engines: `deep_recursion`'s
  function (AArch64) is estimated at 304 bytes and uses ~100 (wasmtime) and
  ~180 (V8), so a nest holds ~1,290 of its frames where either engine holds
  ~5,000.
  Cost: one compare on entry, one add per call; no memory traffic.
  `GEX_CGEN_STACK_BUDGET=<huge>` is the mutation: `deep_recursion` then
  exhausts the engine's stack again.

Recursion deeper than the budget therefore runs as a sequence of nests of
bounded depth, each unwound by returning, with the unwound frames' remaining
work in tier-1.

### 7.2 Loops are outlined: there is no OSR for wasm

V8 (and SpiderMonkey) never move a running wasm frame onto optimised code:
dynamic tiering compiles a hot function with TurboFan in the background and
installs it for the *next* call. A loop that runs for the whole of one call
of a function entered once — a `main` with the kernel inlined into it, a
game's main loop — stays in Liftoff for the whole run. Measured on gx64
`array` (kernel-us): tier-1 244 ms, function tier 316 ms with the default
flags; 135 ms and 100 ms with `--no-liftoff`. ga64 `matmul`: 389 ms against
191 ms. (Tier-1 escapes it partly: its loops are separate functions that are
re-entered, though each still runs a whole loop in one call.)

So every **outermost natural loop** also gets a function of its own,
`static __attribute__((noinline)) int l_<F>_<k>(ctx, depth, need, ret, sp0)`,
over the same registers (loaded on entry). The body keeps its inline copy of
the loop, and the first 1024 trips run there; only when the poll countdown
expires at the loop's back-edge does the body hand over:

```
<write back every dirty register>; safepoint;
do r = l_F_k(ctx, depth + W(F), need, ret, sp0); while (r == LOOP_CHUNK);
if (r <= 1) return r;          /* returned (1) or left (0), as F would */
<reload>; switch (r) { case LOOP_EXIT + b: goto L<b>; ... }
```

Short loops in frequently called functions therefore pay nothing (an eager
version that always called the loop function cost fib 1.32x and gx64 ack
1.89x on CPU time), and long ones get re-entry.

* The loop's own back-edge ends a **chunk** when the poll countdown expires:
  everything written back, the safepoint as usual, `return LOOP_CHUNK`. Every
  1024 back-edges is therefore a fresh call, which picks up optimised code
  once the engine has it; the cost is one write-back and one reload per
  chunk.
* Leaving the loop writes back every dirty register and returns
  `LOOP_EXIT + b`, where `b` is the block the body resumes at.
* A guest `ret` inside the loop is the function's predicted return (§7): the
  loop function has `ret`, `need` and the entry SP, and returns 1 or 0
  exactly as the body would. Every other exit (tier leave, depth guard in a
  callee) returns 0 through it.
* A guest tail call inside a loop is an ordinary call there (a loop
  function's signature is not the bodies', so no `musttail`).
* Inner loops stay inside their outer loop's function, polling as before.
* Functions with irreducible control flow (a backward edge whose target does
  not dominate its source) are not outlined: outlining needs the header to
  be the only way into the loop.
* `GEX_CGEN_OUTLINE=0` turns it off.

## 8. Build and flags

* `gexc build --cgen …` and `gexc run --cgen …` set `gex_xlate::Options::cgen`
  (part of the cache key, since `Options` is hashed through `Debug`).
  `translate_image` then routes through `gex-cgen`. Runtime translation stays
  tier-1.
* Toolchain: `$GEX_CLANG`/`$GEX_WASM_LD` or `clang`/`wasm-ld` on `PATH`,
  clang ≥ 20 for wasm32. Missing tools are a build error, never a silent
  tier-1 fallback.
* `clang --target=wasm32 -O2 -mtail-call -msimd128 -matomics -mbulk-memory
  -mnontrapping-fptoint -msign-ext -fno-strict-aliasing
  -fno-delete-null-pointer-checks -ffp-contract=off -ffreestanding -nostdlib`,
  one translation unit per slice of the block list so clang runs in parallel;
  `wasm-ld --no-entry --import-memory --shared-memory --allow-undefined
  --export=<every C entry>`. Content-addressed object caching (pewasm's
  `build.py`) comes later.
* `GEX_CGEN_KEEP=<dir>` keeps the generated C, objects and the linked module
  for inspection.

## 9. Not handled yet (Phase B, before S7)

* Landing pads and `gex_unwind` (S7). The write-back invariant (§5.1) does
  not need them, only passes that keep live state in locals across calls
  that are not trap-free.
* Interior entries, undiscovered code, runtime-modified code: tier-1 (the
  splice leaves those blocks' tier-1 bodies in place).
* Invalidation when a function's pages change (the x86-64 `guard` bytes):
  a guarded block is not given to the function tier.

## 10. Where Phase B's functions come from

`translate_image` runs S2's recovery (`gex_xlate::func::recover`) and its
call-graph summaries (`summary::analyze`) on the image and hands gex-cgen the
memory-form functions and the summaries. Jump tables are in-function
`switch`es (their default leaves). Functions whose entry tier-1 serves with a
host builtin (`intrinsic`: memcpy and friends) are re-formed from that
tier-1 block, so C callers get the builtin. A function sharing a page with a
block that has a self-modification guard stays tier-1. Only entries tier-1
also has a block for are registered in dispatch; the others are reached by C
calls only. No ELF bytes reach `translate_image`, so `.eh_frame` does not
seed recovery there yet (symbols, relocations and call targets do).
`GEX_CGEN_RECOVER=standin` keeps the earlier stand-in (`recover.rs`: symbols
and call targets, no jump tables), for comparison. `GEX_CGEN_STATS=1` prints
what the emitted functions do at their exits.

Profiling (`GEX_PROFILE=1`, `gexc run --record`) and the function tier are
refused together, loudly: the C code carries no profiling hooks.

## 11. Whole images under the NT layer (PE, M7)

Wine's loader maps every image of a Windows program itself, so its code
arrives through the runtime translator. With `gexc run --cgen` on a PE
program, every image the NT layer maps also gets a **function-tier unit**:
Wine's DLLs (compiled when the prefix is staged) and the program's own
images alike. Dispatch and tier-1 are then the fallback for code generated at
run time (LuaJIT) and code recovery missed, not the boundary between images.

* **Unit.** The whole image's recovered functions (S2's `recover_pe`), as C,
  compiled and linked once into a module of its own (`gex_cgen::image`,
  format `gex-cgen-image 3`), position-independent (`-fPIC
  -fvisibility=hidden`, `wasm-ld --experimental-pic -shared`;
  docs/merge-plan.md §9.8.1). Every function is one `F_body`, `f_<entry>`;
  there are no roots and no exports per function (wasm caps a module at
  100,000 exports; noita.exe has 100,147 functions). Each translation unit
  takes its bodies' addresses (an exported `gex_refs_<first>`, never
  called), so wasm-ld puts every body in the one element segment that
  fills the process's function table from the imported `__table_base`; the
  build reads which body each slot holds from the element segment and the
  name section and appends that as the `gex.funcs` section (and beside the
  module as `c.entries`). Functions clang gives a shadow-stack frame are
  stubs that re-enter the table at their own address, which the host serves
  with something else.
* **Calls.** Inside the image a call is a wasm call to the callee's body.
  Anything else -- `call [IAT]` into another image, a vtable, a callback,
  a direct call to code the image does not hold -- stores its target in
  `ctx::LAST_TARGET`, probes the process's probe cache (`gex_ftab_idx`:
  one load at a constant address for a constant target) and
  `call_indirect`s the function table: a function pointer in a
  position-independent module *is* a table index. A miss is index 0, the
  resolve stub, which asks the host (`gex.lookup`). Jumps out of a function
  (tail calls, switch defaults, jumps recovery left unresolved) are tail
  calls through the table under the function's own `need` and `ret`, so
  whatever runs there returns where the function would have.
* **Installed at map time.** The NT layer calls a hook after mapping an image
  (`gex_kernel::nt::section::set_image_mapped_hook`); the host compares the
  image's executable bytes in guest memory with the unit's and refuses loudly
  on a mismatch, reserves a range of the function table for the unit's
  element segment, maps each entry to its slot, and publishes it
  (`shims/native/src/ftab.rs` `publish`); each store instantiates it with
  `__table_base` at that range as it replays the table's log. Tier-1 still
  runs what no unit holds; its dispatch slot for every entry holds the
  runtime module's `bridge` (`gex_cgen::runtime`), which calls the function
  with no predicted return and dispatches on what it leaves in `ctx::PC`
  (an x86 slot covers four bytes: a neighbour that no function serves is a
  slot conflict, handed to tier-1's miss path). Code no unit holds, called
  from a unit, is the table's index 1: tier-1, run nested until the guest
  returns to the caller (the call-out).
* **Code rewritten after install** (Detours patching noita.exe's prologues):
  the host sets the page's bit in `gex_abi::PATCHED_PAGES` when a retired
  page no longer hashes to what its unit was built from
  (`nt_cgen::on_retire`), before it clears the probe cache entries there.
  Every function that is not trap-free tests its own entry page on entry
  and, when set, does the call sequence above to its own address: the
  lookup sees the bit, retires the unit's slot and serves the new bytes
  (tier-1 now; the function JIT or the region tier later), and the probe
  finds that from then on. A trap-free function's callers test it instead,
  where they can write everything back first. tests/nt/detour.c is the case
  (`GEX_CGEN_MUTATE=NOPATCHCHECK` is the mutation). Tier-1's own direct
  jumps check their page's directory entry against the one they were
  translated for (gex-emit `emit_page_check`).
* **Keyed on the image as placed.** `nt_cgen::image_key` hashes the format,
  the sources that decide a unit (`gex_cgen::image::INPUTS`) and the image's
  relocated segments at the base it was mapped at. A unit is built for the
  base the image is actually mapped at.
* **Wine's DLLs at fixed bases.** 616 of Wine 11.15's 834 i386 DLLs prefer
  0x1000_0000 (the x64 ones 0x1_8000_0000, above the guest), so the first-free
  placement made a DLL's base follow each program's load order. `gexc
  cgen-prefix` (run by tools/nt-stage-wine.sh) writes `PREFIX/gex-layout` --
  a fixed base for each core DLL (tools/nt-cgen-core.txt: what tests/nt and
  Noita map), Wine's own unique bases kept, the rest packed below them -- and
  builds each DLL's unit at its base. The process reads it once at boot,
  refuses a stale one, reserves its architecture's ranges (`find_free` avoids
  them while anything else fits), and places a core DLL at its base when the
  range is free; one that is not is said under GEX_TRACE and counted
  (`layout_miss=` on GEX_NT_STATS).
* **Two stages, bounded memory.** Stage 1 recovers the image and writes its
  functions in GIR's text form plus a line of summary per function
  (`write_recovery`). Stage 2 reads them back one at a time, emits C in units
  of ~512 KiB, compiles each with a precompiled `gex_abi.h` and at most `jobs`
  clangs, caching objects by the hash of the unit, the header, the flags and
  clang's version, and links (`build_c_module`). Functions clang gives a
  shadow-stack frame become stubs that leave for tier-1. Stage 1's memory is
  recovery's (10 GB on noita.exe).
* **Built ahead of time, or for the next run.** `gexc cgen-image <image>
  --pe-base B` and `gexc cgen-prefix` build ahead of time. An image mapped
  with no unit runs on tier-1 while a capped child (`systemd-run -p
  MemoryMax=$GEX_CGEN_MEMORY_MAX`, default 12G) builds one.

**Next step, not built: base-independent units.** A unit's C holds absolute
guest addresses (entries, branch targets, relocated data addresses), so it is
built per base, and an app image whose base follows its load order needs a
build per order. With the image's `.reloc` information the lifter knows which
immediates are relocated; emitting those as `base + offset`, with `base` a
parameter the host supplies at install (a wasm global, or a constant folded
at link time per base from one object set), would let one build serve any
base, and make the staged layout a performance choice rather than a
correctness one.
