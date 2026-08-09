# Runtime feedback — a draft plan

What the guest actually does at run time is the one thing neither the decoder
nor discovery can know, and gex currently learns it, acts on it within the
run, and then throws it away. This is a plan for keeping it: a profile that
the runtime translator consumes while the program runs, and that the static
recompiler consumes on the next run to emit less code and better code.

Companion to `PLAN.md`; nothing here changes §5's pipeline, it adds an input
to it.

---

## 1. Where the project actually is

Relevant state, not a status report:

* **Tier-1 is the whole compiler.** One wasm function per guest block, a tail
  call per edge, guest registers in locals for the length of a block. Tier-2
  in the PLAN §5.5 sense does not exist; what exists is two transforms that
  reach into tier-2's territory from below — self-loop fusion and cross-block
  flag liveness — plus single-predecessor superblock folding, and a `region.rs`
  that forms loop regions by forward growth and narrowing.
* **Calls are the cost, and the cost is the tail call.** Straight-line 4.9x
  native, direct calls 10.5x, indirect calls 18.4x. 100x faster than QEMU on
  vectorised float, 1.2x on Lua's dispatch loop.
* **An inline cache did not help** — +4% on a monomorphic `ret`, −6% on Lua.
  The dispatch arithmetic was never the cost.
* **Whole-object static recompilation costs four times what the demand path
  costs.** Measured on the gears demo: 1,487,251 blocks against 352,080, to
  reach the same two places. It went the other way on the X-server test, 30 s
  to 12 s. The current arbiter is `RECOMPILE_AFTER_MISSES = 24`.
* **Slots are a real resource.** A run reached V8's ceiling of ten million
  dispatch slots and died, because two processes each claimed a full set for
  the same Mesa.
* **A profile already exists and is off by default.** `gex_cache::{read,write}_profile`
  records addresses the runtime translator reached, merged across runs, capped
  at 2^18, keyed by the whole image. It is off because it changes the
  translation and therefore the cache key, so it costs a full recompile on the
  second run and bought nothing measurable after it — 230 extra blocks out of
  51,000 on Debian bash.
* **The native host has a back-edge sampler** (`--sample`), a hash map keyed
  by safepoint pc, reported at exit. It is a debugging instrument, not an input
  to anything.
* **Nothing persists in the browser.** No IndexedDB, no OPFS, no compiled-module
  cache. Every page load pays for everything.

Two conclusions follow. First, gex is already a feedback-driven system — the
demand path *is* a profile-guided recompiler, it simply discards its answer at
`exit`. Second, the existing profile failed for a reason that is specific to
the native host and does not transfer: on native, translation is cheap and
Cranelift is expensive, so any change to the input is a 30-second penalty. In
the browser, translation happens at run time regardless and the penalty is
already being paid; the profile's job there is to make the run pay it *less*.

---

## 2. Prior art, and what each one is actually good for

Not a literature review — a list of the specific mechanisms worth taking.

**DEC FX!32** (x86 on Alpha, 1997) is the closest match to what is being asked
for and nobody has built it since: emulate the binary, write a per-binary
profile database to disk, and have a *background* static optimiser turn that
profile into native code that the next run uses. Successive runs of the same
program got faster without the user doing anything. The parts worth taking are
the shape — profile is a separate artifact keyed by binary identity, the
optimiser is offline, the emulator is always the fallback — and the honesty
that the first run is slow and that is fine.

**Windows XtaCache** (x86 on ARM64, the modern descendant) adds the thing FX!32
lacked: the cache is keyed by file identity and shared across processes and
users, and the background service does the compile when the machine is idle.
gex already has the identity key — `page_identity` hashes file, offset and
mapping length — and already shares slots across processes on it.

**HP Dynamo / DynamoRIO** gives the trace-selection heuristic that actually
works without a full profile: NET, *next executing tail*. Count only branch
targets that are backward branches or trace exits; when a counter crosses a
threshold, take the very next sequence of executed blocks as the trace. It is
cheap because it instruments a small fraction of edges, and it is good because
the tail that follows a hot header is overwhelmingly the hot tail. This is
directly applicable to `region.rs`, which currently guesses.

**HotSpot** contributes three things. Counters are non-atomic and racy on
purpose — a profile does not need to be right, it needs to be cheap.
Instrumentation lives in a *disposable tier*, so steady-state code carries
none. And receiver-type profiles at call sites drive guarded devirtualisation:
compare against the profiled target, direct-call on hit, fall back on miss.

**HHVM** is the same idea with the pieces named clearly: *profiling
translations* are emitted first, run a bounded number of times, and are then
replaced by *optimised translations* built from `ProfData` — region selection,
block layout and specialisation all come from that one structure. Its retire
policy is worth copying: a profiling translation is retranslated once, and the
profile data for that region is then freed.

**Android baseline profiles** are the closest living system to the static end
of this plan, and the most worth copying. A profile of the *startup path* is
collected — on a developer's device, in CI, or aggregated from real users as a
"cloud profile" — shipped inside the application, and used by the ahead-of-time
compiler at install time to compile just that path. Everything else stays
interpreted or JITted until it proves itself. Three transferable decisions: the
profile is an ordinary build artifact rather than a runtime state, it is keyed
to a specific build of the code, and it is about *what runs first* rather than
*what runs most*.

**Binaryen's `wasm-split`** is the same idea already implemented for wasm, and
it is what the static end's output should look like. It instruments a module to
learn which functions actually ran, then splits it into a primary module and a
secondary module fetched lazily, leaving stubs behind for the functions it
moved. Emscripten uses it for exactly the startup problem gex has. gex's
version is easier: `wasm-split` has to synthesise a stub per moved function,
and gex's dispatch table already provides one for every address in the program.

**Apple Rosetta 2** confirms the topology. Translation happens ahead of time at
install, output is cached per binary, and a JIT covers what AOT could not —
including the guest's own JITs. It is the existence proof that "AOT with a
demand-translated fallback" is a shipping design and not a compromise.

**LLVM BOLT / Propeller / AutoFDO** are the static-consumer end. What they do
with a profile, in order of payoff: basic-block layout by edge weight, hot/cold
function splitting, function reordering, indirect-call promotion, and
eliminating the PLT indirection where the target is stable. BOLT's most
transferable lesson is negative: sample-based profiles are noisy and *stale*,
and every consumer must degrade to "no profile" rather than to "wrong answer".

**Transmeta CMS and NVIDIA Denver** are the ones that speculate hardest —
region formation with rollback on a misspeculation. gex has no rollback and
should not acquire one; the relevance is only that they confirm the ranking:
indirect branches and region shape are where the money is.

**QEMU** is here as a control. It does chained TBs and nothing else, has no
profile, and gex is already 6.5x faster than it. Its absence of feedback is why
`HQEMU` — QEMU plus a trace-hotness detector feeding an LLVM backend — exists,
and HQEMU's result is the number to hold in mind: trace-level region formation
on top of a per-block translator was worth roughly 2-3x on compute-bound code
and near nothing on branchy code. That is exactly the profile of gex's own
straight-line-vs-indirect table.

What nobody in that list is doing, and gex must, is **producing wasm for a
second compiler**. The output is not machine code; it is input to V8. That
changes what a profile is for in one important way: code that is never emitted
is code V8 never compiles, and V8's compile time is nearly all of gex's
startup. On SQLite, translating is 1.8 s and compiling the result is 31 s. So
*pruning* is worth more here than it is in any of the systems above, and it is
worth more than layout.

---

## 3. The invariant

**A profile is never a correctness input.** Every consumer degrades to "no
profile" rather than to a wrong answer, and a deliberately corrupt profile must
produce a program that runs correctly and slowly.

This is not a slogan, it is a classification. Each consumer is one of:

* **Safe** — the profile chooses *what* to translate or *where to put it*. A
  wrong answer wastes work. Seeds, pruning, layout, module packing, slot
  allocation, recompile triggers. Discovery's own executability check already
  drops addresses that stopped being code.
* **Guarded** — the profile chooses a fast path, and the code checks. Indirect
  target promotion, PLT folding, trip-count-driven unrolling. A wrong answer
  costs a compare.
* **Unsound without proof** — the profile is used as an *edge* in an analysis.
  Cross-block flag liveness over observed indirect targets is the one that
  matters, and it is the one to leave out of v1. An observed target set is not
  a complete target set, and flag liveness needs completeness. It becomes
  available only if the site is guarded so that an unobserved target cannot
  enter the optimised code at all — at which point it is a guarded consumer and
  no longer interesting on its own.

Enforcement: `GEX_PROFILE_SHUFFLE=1` permutes and corrupts every profile record
before it is consumed, and the whole integration suite must pass under it. This
is the same trick as `tests/integration/programs/flags.S`, which hangs when the
flag analysis is deliberately broken — a test whose value is that it fails.

---

## 4. What to measure

Ranked by what gex's own measurements say it is worth, not by what is easy.

### 4.1 Reachability — which blocks were ever entered

The cheapest and the most valuable, because it is what pruning needs and
pruning is what buys the thin static output.

Two grades. **Grade A is free**: for anything the demand path produced, "has a
translation" is already recorded in `Jit::translated`, and it is the closure of
what was entered under discovery. Snapshotting it costs nothing at run time and
it is already 4x thinner than whole-object on gears. **Grade B** distinguishes
*entered* from *reachable-from-entered* and needs one store per block entry in
an instrumented tier — worth it only once Grade A is exhausted.

Representation: a bitmap over the object's text, one bit per 4 bytes. Mesa's
rasteriser is 471,205 blocks; a bit per instruction word over a 20 MiB text is
640 KiB, and it compresses to nearly nothing because it is sparse and clustered.

### 4.2 Indirect branch target sets

Per `BR`/`BLR`/`RET` site: up to four observed targets with counts, plus an
overflow flag. This is the top *throughput* item because indirect calls are
18.4x native against straight-line's 4.9x, and it is the only structural CFG
information that neither discovery nor `.eh_frame` can supply.

The important thing to say up front, so nobody re-runs the failed experiment:
**this is not the inline cache that was reverted.** That cache stored the last
`(target, slot)` pair in the *context* and still ended in a call; it saved a
shift, a load and a select, and those were never the cost. A profiled target is
a compile-time constant, the guard is a compare against an immediate, and the
hit path is either a `return_call` to a known function — which removes the
table dispatch and lets V8 see the edge — or, when the target has one hot
predecessor, the target's *body emitted inline*, which removes the call
entirely. Removing the call is the only thing the measurements say works:
single-predecessor superblock folding gave 1.19x on matmul for 1.4% more wasm,
and that is the same mechanism with static predecessor information instead of
profiled.

Also: a `ret`'s target set is the caller list, which is the call graph that
`.eh_frame` and symbols half-give and that cross-function region formation
needs.

### 4.3 Edge counts, especially back-edges

Every back-edge already has a safepoint, so there is a place to hang a counter
with no new control flow. Consumers: the region cost model, the self-loop
fusion live-in decision, the recompile trigger, and block layout.

`region.rs` states the open question exactly — "a cost model that grows the
real loop and then decides is the thing to try, not more coverage" — and edge
weights are what a cost model needs. The natural-loop construction found 7-11x
as many blocks and ran slower because a region pays its live register set on
every entry and exit; with weights, growth stops at the first cold edge instead
of at an arbitrary depth. This is Dynamo's NET, and NET is cheap precisely
because it only counts backward-branch targets and trace exits.

### 4.4 Call-graph edges

`bl` site → callee, with counts. Drives function-level layout, module packing,
and eventually partial inlining of small hot leaves. Free-ish: a `bl` is a
direct branch, so the edge is static; only the *count* is new, and it can be
derived from block counts rather than measured separately.

### 4.5 Branch bias

Taken ratio for each `b.cond`. Cheap (one counter per block, plus the block's
own count) and it feeds layout: the cold side goes behind the fall-through, and
a strongly-biased branch whose cold side is a call to something never entered
can leave that side on the miss stub entirely rather than emitting it.

### 4.6 Value profiles, narrow and targeted

Not general value profiling — four specific things gex already has a reason to
want:

* **`memcpy`/`memset` length distribution** at the shim's fast path. A
  distribution concentrated under 64 bytes argues for an inline specialised
  copy rather than a host call; gears' remaining `memcpy` is already called out
  in `syscalls.rs`.
* **FPCR observed values.** The fast FP path assumes round-to-nearest and
  falls back if the guest writes a non-default mode. A profile that says the
  mode was never non-default across many runs lets the static output commit to
  the fast path and put the check behind a guard at the write site rather than
  at every operation.
* **Syscall histogram.** Already half-collected by `gex_recent_syscalls`. Says
  which shim paths deserve attention; not a codegen input.
* **Loop trip counts** at back-edges — a distribution, or just a mean and a
  saturating "always ≥ N" bit. Decides whether preloading a register into a
  local across a fused back-edge pays, which is currently decided by a rule
  (live-in only) that was arrived at because preloading the full read set made
  matmul *slower*.

### 4.7 Where the model was wrong

Dispatch misses per page, blocks that ended in SIGILL, blocks where
`Builder::has_inexpressible` fired, and translation invalidations from a guest
JIT. This is not a codegen input; it is the feedback that tells the *compiler
authors* which 317 undecoded instructions are actually in anyone's way, and
which pages a guest JIT keeps rewriting so the static recompiler stops trying
to own them.

---

## 5. How to collect it without paying for it

Three constraints shape this: sustained runtime performance is what counts,
there is no sampling timer inside wasm (a `setInterval` cannot fire while the
main thread is translating), and there is no hardware branch history.

**Instrumentation lives in a disposable tier — "tier-1p".** Identical to tier-1
plus counters. A block is first translated as tier-1p; when its counter crosses
a threshold, the block is retranslated *without* counters and its dispatch slot
is repatched. Steady-state code carries zero instrumentation, which is the only
form of this that respects the performance constraint. gex already has every
piece of the mechanism: blocks are translated on demand, modules are queued for
JS to instantiate, and a slot is patched to point at new code. This is HotSpot's
tier 3 → tier 4 and HHVM's profiling → optimised translations.

The threshold is also the region trigger, so one counter serves both.

**Counters live in a second `WebAssembly.Memory`.** Multi-memory is shipped in
V8 and supported by wasmtime. A side memory means the guest cannot see or
corrupt the profile, it consumes none of the guest's address space (which is
already carved up to `0xFFFF_0000`), and as a `SharedArrayBuffer` it is shared
across the Workers of a process for free. The alternative — carving profile
space out of the reserved region below `RESERVED_END` — is smaller work and a
worse boundary; take multi-memory unless it turns out V8 charges for the second
memory in a way that shows up.

**Counters are non-atomic and racy.** Two Workers incrementing the same u32
lose counts. That is fine and it is what HotSpot does; a profile is a hint. An
atomic RMW per back-edge across Workers is not affordable and would change what
is being measured.

**Counters saturate and stop.** A u16 that saturates is enough to decide "hot",
and once a block is deinstrumented it costs nothing anyway.

**The native sampler is promoted, not replaced.** `--sample` already collects
back-edge hotness on wasmtime. Since a profile is about the *guest*, not about
the host, an expensive instrumented run on the native host produces a profile
the browser can consume without ever paying to collect it. This is a build-time
tool — `gexc profile ./app` — and it is the cheapest way to get a good profile
for a program that ships. Caveat to state plainly: a native run and a browser
run of the same program do not necessarily take the same paths, and where they
diverge the profile is merely stale, not wrong.

---

## 6. Where it lives

A new crate, **`gex-prof`**, owning the record types, the merge, the decay, the
serialisation, and nothing else. Both hosts write it; both compilers read it.
It replaces `gex_cache::{read,write}_profile`.

**Keyed by object identity, not by image.** The existing profile is keyed by a
hash of the whole placed image and records only addresses inside the
executable's own text — the comment says why: a shared library lands wherever
the interpreter put it. That restriction is what makes the current profile
nearly useless for exactly the workloads that need it, since on the gears demo
essentially all the code is Mesa and libc. The fix is the one `page_identity`
already implements for slot sharing: key on (file, offset, length), and
**record every address as an offset from the object's base**. A libc profile is
then portable across processes, across runs, and across load addresses, and two
processes mapping the same Mesa share one profile.

**Merge is additive with saturation.** Counts add. Target sets union, keeping
the top four by count and setting overflow when a fifth appears. Bitmaps or.
Decay is exponential per run (halve, then add) so a profile from a hundred runs
ago cannot pin a decision the program has stopped making.

**Two version fields, not one.** The *guest* version — object identity — governs
whether raw counts are still meaningful; they are, forever, because they
describe the guest and not gex. The *translator* version —
`gex_cache::build_fingerprint` — governs whether *derived decisions* are still
valid. Conflating them is why a decoder fix once made a fixed binary reuse a
broken module for an hour. Raw counts survive a translator change; derived
plans do not.

**Persistence in the browser is IndexedDB**, alongside — and this is a separate
win worth doing at the same time — the compiled `WebAssembly.Module` objects
themselves, which V8 will structured-clone into IndexedDB and return with their
code cache intact. Nothing persists in the browser today. A profile that
survives a page reload and a compiled module that survives a page reload are
the same piece of plumbing, and the second is probably worth more than
everything else in this document.

**Snapshot ABI**: `gex_profile_snapshot(ptr, max) -> len` and
`gex_profile_load(ptr, len)`, symmetric with the existing `gex_add_file` /
`gex_get_file` shape. JS owns storage; the shim owns the format.

---

## 7. Consumers

### In-run, the JIT

* **Replace `RECOMPILE_AFTER_MISSES = 24`** with a real hotness signal. Twenty-
  four misses in an object is a proxy for "this library is in use", and it is a
  bad one: it fires on breadth, not on heat. Block counts plus miss counts say
  whether the object is a library the run keeps returning to (recompile it) or
  one it walks through once (leave it on demand). This is the decision that is
  currently costing 1.49M blocks against 352k on the demo that matters most.
* **Region formation by NET.** Grow from a hot back-edge target along the
  actually-executed tail; stop at the first edge below a fraction of the
  header's count. Compare against the current forward-growth-and-narrow on
  nbench, Lua, zstd and SQLite, which are the four the existing region numbers
  were taken on.
* **Guarded indirect promotion** at monomorphic sites, and inline folding when
  the target's hot predecessor is unique.
* **Deinstrumented retranslation** of hot blocks into a fresh small module, via
  the existing pending-module queue.

### Across runs, the static recompiler

Its own section — see §8. It is the larger half and it wants a different
collector, not the same counters read later.

---

## 8. The static end: a capture, not a profile

### 8.1 The asymmetry that decides the design

The two ends look like one problem and are not. The JIT end needs
instrumentation that costs nothing, because it runs in the code whose speed is
the point. The static end runs **once, offline or in a background tab, on a
workload nobody is timing** — it can afford a fifty-fold slowdown and it should
spend it.

So: one *format*, two *collectors*. Building the static capture out of tier-1p's
cheap saturating counters would be taking the JIT end's constraint and paying
it at the static end for no reason, and it would discard exactly the
information that only an expensive collector can get — full indirect edge sets
rather than the top four, ordering rather than counts, memory contents rather
than addresses.

### 8.2 What a run knows that no static analysis can recover

In payoff order for gex specifically, which is not the order a general
recompiler would give.

**1. The loaded world.** Which objects were mapped, at what addresses, in what
order, including everything reached by `dlopen`. This is the largest item and
it is not about optimisation at all — it is about the static recompiler having
anything to work on. Today only the executable is translated from its ELF,
because only the executable is loaded by gex; the interpreter maps everything
else at run time from guest code, so for a dynamically linked program *almost
nothing is statically recompiled*. PLAN §6.4 answers this by asking the user to
author a `dlopen` manifest by hand — `gexc build --with-so 'plugins/*.so'`. A
capture writes that manifest, correctly, from a run.

**2. The post-relocation read-only image, and the resolved GOT.** After the
loader has finished, every PLT's GOT slot holds its final value and every
vtable and jump table is filled in. `pass::const_fold_with_memory` already
exploits this at run time — that is how a PLT's `br x17` becomes a known
target — and the comment on it says exactly why the static path cannot:
"Static translation is not [willing to read memory], but runtime translation
is." A capture makes it willing. The effect is turning indirect calls into
direct ones across the whole loaded world, and gex's own table prices that at
18.4x native against 10.5x.

**3. Indirect target sets as CFG edges.** Not for promotion — that is the JIT
end's use — but for *discovery*. SQLite's VDBE is a computed-goto interpreter,
recursive descent cannot follow a jump table, and the consequence was 10,000
blocks translated on demand on every single run. Recorded targets are seeds and
edges; the code becomes statically reachable at all.

**4. Which executable bytes are code.** About three quarters of the addresses
in a static musl text do not begin a decodable instruction, and
`translate_object`'s scanning path already skips those — but of the remainder,
most are the middle of a function some earlier block already covers. Scanning
finds two hundred thousand seeds where `.dynsym` finds a few thousand. A
capture finds the right few thousand, and finds them for a stripped object that
has no `.dynsym` at all.

**5. Which pages the guest wrote and then executed.** Self-modifying code and
guest JITs. The static recompiler must not claim those pages; today it has no
way to know which they are.

**6. The entered set, and the order of first entry.** The order is the
browser-specific one and §8.5 is about it.

**7. The observed syscall set.** The shim is wasm too. A deployment whose guest
never opens a socket, never touches virgl and never forks is carrying code for
all three. This is the one item on the list that thins the *host* rather than
the guest, and it is the only one that needs no new collector — the syscall
histogram is nearly there already.

### 8.3 Why gex can speculate harder than a normal recompiler

The dispatch table plus PLAN §5.5's "both tiers coexist per-address" is an
unusual amount of freedom, and it changes the soundness classification of §3.

If the capture says no indirect branch ever entered the interior of function
`F`, the static recompiler can emit `F` as one region: no dispatch slots for
its interior blocks, registers in wasm locals across the whole body, real wasm
control flow. Should a branch nevertheless arrive at an interior address, it
finds the miss stub, gets a tier-1 duplicate of that block translated on
demand, and runs correctly from context state — because a region flushes guest
state to the context at every exit and is only ever *entered* at its header.
The cost of being wrong is a duplicate block and a translation, not a wrong
answer.

That is worth stating plainly because it is the architectural property that
makes this whole line of work cheap here and expensive elsewhere: **in a fixed-
code recompiler, a wrong assumption about entry points is a miscompile; in gex
it is a cache miss.** Transforms that BOLT has to prove, gex can guess.

The one place this does not apply is any analysis whose result is baked into
*other* blocks — cross-block flag liveness over profiled indirect edges is
still unsound, because the tier-1 fallback block would inherit a flag state the
region decided not to compute. §3's classification stands for that one.

### 8.4 Where the capture comes from

Three topologies, and they are not exclusive.

**(a) Offline, on the native host.** `gexc capture ./app -- args` runs the
program with maximal instrumentation and writes an artifact; `gexc build
--capture app.gexprof` consumes it. Best output, no runtime cost anywhere, and
it needs the workload to be known in advance — which for gex's own demos it
exactly is. The gears demo runs one program, the Debian demo runs `apt update`.
This is the topology that serves the priority list, and it is the one to build
first.

**(b) In the tab, across visits.** The demand path already produces most of the
capture as a side effect; persist it in IndexedDB and let the next page load
statically recompile from it. Self-improving with no developer involvement,
which is Windows' XtaCache and Android's on-device profile collection. Second.

**(c) In the tab, in a background Worker, during the same run.** gex already
has the pending-module queue and Workers. Removes translation pauses mid-run,
which the `--recompile-objects` help text identifies as the actual thing that
option buys ("the absence of translation pauses part way through a run, not
speed"). Third, and possibly never.

**Collecting without instrumenting gex at all.** `qemu-aarch64` is already a
toolchain dependency — `tests/diff-fuzz` shells out to it. A QEMU TCG plugin
gets a callback per translation block execution and per instruction, which
yields the entered set, the block boundaries and the indirect branch targets
for nothing but a small C file. That is the "prior interpreted run" in its
cheapest possible form, and it is a good way to get item 3 of §8.2 before any
of gex's own instrumentation exists. Its limits are worth being clear about:
QEMU's block boundaries are not gex's, its load layout is not gex's, and it
knows nothing about gex's dispatch decisions — so it is good for *what code
runs* and useless for *how gex should compile it*. Items 1, 2 and 5 need gex's
own loader and want collector (a).

### 8.5 The artifact

Explicitly **not a process snapshot.** The snapshot/restore switcher was killed
on purpose and `README`'s "The snapshot that was rebuilt on every syscall" is
the record of why; nothing here should walk back toward it. A capture is a
read-only description of what was invariant, and every part of it is validated
on load rather than trusted.

| record | contents | validity check on load |
|---|---|---|
| object table | identity (file, offset, length), base, size, load order | `page_identity` hash matches |
| entered map | bitmap, one bit per 4 bytes, object-relative | object identity |
| first-entry order | run-length list of object-relative offsets | object identity |
| indirect edges | site offset → target offsets, full set, per object pair | both object identities |
| resolved constants | address → value, only for pages proven unwritten after relocation | value re-read and compared at load |
| SMC pages | pages written then executed | advisory; a page not in the list that turns out to be written just invalidates normally |
| syscall set | numbers used | advisory |

Validation is per object and falls back per object: a Mesa that no longer
hashes the same drops to the demand path while libc keeps its capture. The
resolved-constants table is the only one that needs a per-record check, and it
is cheap — the loader has just written those slots, so it is a compare against
memory it already has in hand.

### 8.6 What only the offline compiler can do

Everything in §7's JIT list is available offline too and better, because there
is no time limit. These are the ones that are *only* available offline, because
they need the whole loaded world at once:

* **Regions and inlining across object boundaries.** A hot loop in the guest
  that calls `memcpy` through the PLT is, with the GOT resolved, a loop that
  calls a known function in a known object — inlinable. At run time each object
  is recompiled alone and this is not expressible.
* **Dense whole-program slot assignment.** The runtime hands out dispatch slots
  incrementally as pages are claimed, which is how the gears run reached V8's
  ten-million ceiling. Offline, the entered set is known and slots can be
  assigned to exactly it.
* **Global constant propagation** from the post-relocation image, rather than
  the block-local fold the runtime can afford.
* **Block deduplication** across the loaded objects — libc's string functions
  appear in more than one place in a rootfs.
* **Temporal module splitting.** The capture records *first-entry order*, so
  the output can be split into modules in the order the run needed them:
  module 0 is everything the first frame or the first prompt touched, and
  nothing else. Time to first output is what a demo page is judged on, and V8's
  compile time is proportional to what you hand it. This is Android's baseline
  profiles almost exactly — a profile of the startup path, shipped with the
  application, used to compile ahead of time only the code startup needs — and
  its wasm-native form is Binaryen's `wasm-split`, which instruments a module,
  learns which functions ran, and splits it into a primary module and a
  lazily-fetched secondary. gex's version is strictly easier than Binaryen's:
  `wasm-split` has to synthesise stubs for the functions it moved out, and gex
  already has one for every address in the program.

---

## 9. What it has to beat

Targets, so the work can fail honestly:

| | now | target |
|---|---|---|
| gears, blocks translated | 1,487,251 static / 352,080 demand | under 250,000 |
| gears, dispatch slots claimed | hit V8's 10M ceiling | under 2M |
| SQLite, wasm emitted | 62 MiB exhaustive / 14 MiB from entry | under 8 MiB with a warm capture |
| SQLite, second-run startup | 32.1 s settling to 0.16 s (native cache) | browser: second load under 3 s |
| dynamic binary, code statically recompiled | approximately none | the whole entered closure of the loaded world |
| gears, time to first frame | whatever it turns out to be | half it, from temporal module splitting |
| Lua interpreter loop | 1.2x QEMU | 1.5x, from indirect promotion |
| matmul, region formation | 1.19x from single-pred folding | 1.35x from profiled regions |

The last two rows are the throughput bet and are the least certain. The first
four are the pruning bet and are close to arithmetic. The fifth is not a
speedup at all — it is a capability that does not currently exist.

---

## 10. Traps

* **Profile churn defeating the module cache.** This has already happened once
  and is documented: turning the profile on cost a full 30-second recompile on
  every second run and bought nothing. The fix is hysteresis — the cache key
  covers the *derived plan*, not the raw counts, and the plan is only
  regenerated when a decision actually changes. A run that merely increments
  counters must not invalidate anything.
* **Non-determinism.** Two runs with the same profile must emit byte-identical
  wasm or nothing downstream ever settles. Racy counters make the *profile*
  non-deterministic between runs, which is fine; the plan derived from a given
  profile must not be. Quantise counts into buckets before deriving anything, so
  small count differences do not move a decision.
* **Measuring against an unsettled cache.** Already burned: the same change
  measured 1.29x and 0.89x. Every number in §9 requires both sides warm and a
  minimum of alternating runs.
* **A capture that overfits its workload.** Mesa captured under glxgears and
  Mesa captured under the X server are not the same capture, and a build that
  ships the first will fall back to the demand path for most of the second.
  This is the cost of topology (a) and it is why (b) exists. Merge captures
  across workloads rather than picking one, and treat a capture's *coverage*
  — what fraction of the run's misses it predicted — as the number to report.
* **The capture becoming a correctness dependency by accident.** The resolved-
  constants table is the dangerous record: it is the one thing in the artifact
  that is a *value* rather than a *hint*, and the whole design rests on it
  being re-read and compared at load. A future change that skips that compare
  for speed is a miscompile, and the comparison must be commented as such
  where it lives.
* **Validation cost at load.** Per object, not per page: a rootfs with two
  hundred shared objects can afford two hundred hashes and cannot afford one
  per 4 KiB page.
* **Instrumentation changing what is measured.** Mitigated by tier-1p being
  disposable, but the *threshold* is a policy that changes which blocks get
  measured at all. Keep it high enough that instrumentation is a startup cost.
* **Memory.** One u32 per block at 1.4M blocks is 5.6 MiB; four indirect
  targets at 12 bytes each per site is bounded by site count. Both fine, both
  worth a cap and a "profiling disabled, budget exhausted" path.
* **A profile for a program that does something different every time.** The
  README's own warning: a recompile that never settles. Decay plus a cap on how
  often a plan may be regenerated per run.

---

## 11. Phases

Two tracks, because §8.1 says the collectors are different. They share P0 and
P1 and then diverge, and the static track is the one that pays first.

This work sits behind the standing priority list — the gears demo confirmed
working comes first — but S0 and S1 touch the gears demo directly and may be
the cheapest things that move it.

### Shared foundation

**P0 — reachability for free.** Snapshot `Jit::translated` at exit as offsets
from object base; persist in IndexedDB keyed by `page_identity`; feed back as
seeds and as a prune mask to `translate_object`. No instrumentation, no new
tier, no format beyond a bitmap. Measure: blocks, wasm bytes, slots claimed,
time to first gears frame.

**P1 — `gex-prof`.** The format: records, merge, decay, the two version fields,
object-relative addressing, and the §8.5 validity checks. Both hosts write it.
Retires `gex_cache::{read,write}_profile`. `GEX_PROFILE_SHUFFLE=1` and the
corruption test land here, before anything depends on the data.

### Static track — the capture, and thinner output

**S0 — the object table.** Record the loaded world: identity, base, size, load
order, including `dlopen`. Consumed by `gexc build` to translate the shared
libraries a run actually used, which is the thing that currently does not
happen at all. Smallest possible version of §8.2 item 1, and it unblocks
everything else on this track.

**S1 — `gexc capture` and `gexc build --capture`.** The offline collector,
arbitrarily expensive: full indirect edge sets, first-entry order, the entered
map, the post-relocation constant table. Topology (a). Target: the gears rootfs
and the Debian rootfs, whose workloads are fixed and known.

**S2 — static pruning and dense slot assignment.** Emit the entered closure and
nothing else; assign dispatch slots to exactly it. The 1.49M-blocks and
ten-million-slots rows of §9.

**S3 — prebinding.** Fold the resolved GOT statically, the way
`const_fold_with_memory` folds it at run time. PLT calls become direct calls
across the loaded world. This is also most of what PLAN §6.4's unbuilt
prebinding asks for.

**S4 — temporal module splitting.** Modules in first-entry order; module 0 is
the startup path. Time to first frame is the measurement.

**S5 — cross-object regions and inlining.** The offline-only transforms of
§8.6. Last, because it needs S1 through S3 in hand and is the most work.

### JIT track — cheap counters, better codegen

**J0 — counters and tier-1p.** Side memory, back-edge and block-entry counters,
threshold, deinstrumented retranslation. First consumer: the object recompile
trigger, replacing `RECOMPILE_AFTER_MISSES = 24`.

**J1 — regions by NET.** The cost model `region.rs` asks for, against the four
benchmarks its current numbers were taken on.

**J2 — indirect targets and guarded promotion.** The throughput bet. Lua is the
benchmark that decides it.

**J3 — value profiles.** Branch bias, trip counts, `memcpy` sizes, FPCR. Each
is small and each is independently justifiable; none is worth doing before J2.

### Alongside, not in sequence

**Browser module persistence.** Structured-cloning compiled
`WebAssembly.Module` objects into IndexedDB is independent of all of the above,
shares P0's storage plumbing, and is plausibly the single largest startup win
available.

**The QEMU TCG plugin.** A small C file against a dependency that is already
installed, producing entered sets and indirect edges before any of gex's own
instrumentation exists. Worth doing early precisely because it is not on the
critical path of anything.

---

## 12. Open questions

1. Does V8 charge anything measurable for a second memory in a module that has
   a shared primary memory? If it does, counters go below `RESERVED_END`
   instead and the guest can see them.
2. Is Grade B reachability (entered vs reachable-from-entered) worth a store
   per block entry, or does discovery's closure from Grade A seeds already
   converge to the same set after two or three runs? Measurable in P0 before
   committing to P2.
3. Can a `ret`'s profiled caller set ever be *proven* complete — every call site
   in the object statically enumerable, no computed function pointers into it —
   often enough to make flag liveness across returns worth building? If yes it
   moves from "unsound" to "safe" and becomes interesting.
4. Does a profile collected on wasmtime predict browser behaviour closely enough
   to be worth shipping as a build artifact, or do the paths diverge enough that
   only a browser-collected profile is useful?
5. What is the right unit for the plan cache — per object, or per (object, set
   of other objects loaded)? Mesa profiled under glxgears and Mesa profiled
   under the X server are not obviously the same profile.
6. How stable is a capture across guest rootfs updates? A Debian point release
   changes libc's bytes and invalidates its capture entirely, even though
   ninety-nine per cent of the entered set is at the same offsets. Is there a
   cheaper identity — build-id plus a tolerance — that survives a rebuild, or
   is falling back to the demand path for one run after every update simply the
   right answer?
7. Does the capture's own size matter? The entered map for a graphics rootfs is
   a few hundred KiB compressed, but the indirect edge table for a full run of
   Mesa could be much larger, and it has to be downloaded before the first
   block is translated. Where is the point at which shipping a thinner capture
   beats shipping a more complete one?
8. Should topology (b) — the self-improving tab — ever upload anything? Google
   aggregates on-device profiles into cloud profiles shipped to every user, and
   the equivalent here is real. It is a product decision with a privacy cost,
   not a technical one, and it should be named as such rather than arrived at.
