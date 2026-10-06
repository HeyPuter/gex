# Noita: gex against pewasm, measured the same way

The goal (user, 2026-10-02): gex's clang tier (AOT units, hybrid or
AOT-only) runs Noita's world at least as fast as pewasm's clang build. This
file holds the recipe both are measured with, the numbers, and where gex's
time goes.

## The metric

**World fps at a 4x guest clock.** The world simulation frame counter is
the first field of the game-state singleton at noita.exe 0x122374c (what
Lua's `GameGetFrameNum` returns). Noita caps the world at 60 frames per
guest second, so at real time a fast build only shows 60. With the guest's
monotonic clocks running 4x, the cap is 240 frames per real second, and the
world's frame rate measures capacity. This is pewasm's own throughput
metric (`port/tput.sh`, `pewasm/docs/optimization-plan.md` "Measured
results"), and the source of the ">120 fps" figure: about 104 world fps
after its lock-free critical sections, then x1.15 (devirtualisation, leaf
inlining) and x1.27 (flag elision across calls), on all cores.

Both runs use one physical core, **cores 4 and 10** (a hyperthread pair),
so neither gets more hardware than the other, and nothing else timed runs
there.

### pewasm (reference)

Headless Node, null GL, fixed world seed (`PEWASM_FIXED_TIME`), scripted
menu clicks by frame; the build is `out/noita/build` (Sep 30, `-O2`, ThinLTO,
`MEMCHECK=0`, 96 MB wasm):

```
cd ~/src/noita-wasm
PEWASM_TIME_SCALE=4 taskset -c 4,10 node --max-old-space-size=8192 \
    port/bench.cjs --build out/noita/build --seconds 60 --warmup 30
```

`RESULT` is the mean world fps over 60 s after a 30 s warm-up; `BUSY` is
each thread's busy milliseconds per world frame. Profile:
`node --cpu-prof --cpu-prof-dir=DIR port/bench.cjs ...` then
`port/profile_summary.py PROFILE /dev/null` (functions are named
`sub_<addr - 0x3f0000>` for noita.exe: pewasm places it at 0x10000).

### gex

Native wasmtime, real GL through gex-gl to the host's Mesa, frames composited
to PPM files (the recipe deletes them), scripted menu clicks by frame, a
random world seed (Noita seeds from the wall clock; gex has no fixed-time
knob yet), `GEX_NT_TIME_SCALE=4` (the counterpart of `PEWASM_TIME_SCALE`),
`GEX_NT_PEEK='0x122374c*'` (the world frame, printed each second as `peek=`):

```
S=<scratchpad>/fjit   # run-noita.sh: a copy of abi9/run-noita.sh writing under $S
GEX_NT_TIME_SCALE=4 PREFIX=$S/prefix CPUS=4,10 GEXC=<gexc> \
    $S/run-noita.sh TAG SECONDS [--no-cgen]
```

**Display: null, for parity.** pewasm's recipe runs null GL and shows
nothing; gex's frame dump (`PpmDisplay::publish`, compositing every frame
to a PPM file) was 3.3% of tier-1's CPU. Parity runs pass
`--wayland-out null` (frames presented and dropped; run-noita.sh's
`NULLDISP=1`). GL calls still reach the host's Mesa (about 2%: opengl32 plus
gallium); a null GL is not built.

World fps = the slope of `peek` over a steady window (120 s steps from
world-up). `PREFIX` is a private copy of the staged Wine prefix, so other
agents' runs (which delete the save) cannot collide; `GEX_CGEN_CACHE` a
private unit cache, so another agent's unit build (which holds the cache's
build lock) cannot stall the run.

Profiles: `GEX_PROF=<file> GEX_PROF_START=<s> GEX_PERFMAP=1` (a SIGPROF
sampler in gexc, 1 kHz of process CPU time; `perf` is not installed and
`perf_event_paranoid` is 2), then
`tools/prof-report.py <file> [--modules] --cats <categories.json>`.

## Numbers (2026-10-02, cores 4 and 10)

| build | world fps (4x clock) | world-up |
|---|---|---|
| pewasm `out/noita/build` | 76.2, 83.9 (two runs) | 27 s |
| gex tier-1 (`--no-cgen`, 21012db5 + abi9 aae0e2c1) | 15.5-16.8 (steady, 680-1200 s) | 559 s |
| gex units (aae0e2c1) | no run: noita.exe's unit fails to compile ("Code for function is too large") | |

## Where gex's time goes

### tier-1, steady state (650-1000 s)

| share of process CPU | |
|---|---|
| 75.6% | guest code (tier-1's runtime-translated modules; no units) |
| 8.7% | host waits: `sched_yield` 6.2% (idle threads spinning), futex/semaphores |
| 4.4% | libc (memcpy, malloc) |
| 4.0% | display: `PpmDisplay::publish` (compositing frames to files) |
| 2.4% | the NT layer (syscalls, objects, waits) |
| 1.3% | GL (gex-gl and the host driver) |
| 0.6% | wasmtime trap/unwind (trap-outs) |
| 0.5% | ftab runtime |
| 0.4% | wasm/host trampolines |

So in tier-1 the guest's own code is three quarters of the time; gex
spends about 100 ms of CPU per world frame where pewasm's three busy
threads spend about 18 ms.

### By guest module (tier-1, steady state, stream R)

`tools/modattr.py` maps a GEX_PROF profile to guest images: tier-1 blocks
carry `b_<addr>` names under `GEX_PERFMAP` (gex-emit's name section), units
`f_<entry>`; `--exports DLL@BASE` groups one image by export. 4x clock,
cores 3 and 9, frames still dumped, 650-1000 s (364 s of CPU, world 16.1/s):

| share | module |
|---|---|
| 57.7% | noita.exe (hottest: 0x724720 3.9%, 0x4ac0d0 2.1%, 0x5b01c0, 0x70b110, 0x864060) |
| 11.6% | msvcr120: about 5% ConcRT spinning and locking (`_SpinWait::_SpinOnce`, `_ReaderWriterLock::_TryAcquireWrite`, `_Context::_Yield`, `critical_section`), memset 1.3%, strcpy_s 0.7%, sin/cos 0.9% |
| 12.4% | host libc: `sched_yield` 5.7% (the guest's `SwitchToThread`/`Sleep(0)`: `NtYieldExecution` is a host yield), the rest stripped-symbol mem* and futex paths |
| 8.3% | host gexc: the frame dump 3.3%, the NT layer about 2.5%, wasmtime unwinding 0.5% |
| 5.3% | Wine: ntdll 2.9% (heap, yield, SRW locks), kernelbase 1.4% and kernel32 1.0% (TlsGetValue, SwitchToThread), opengl32 0.8% |
| 1.0% | msvcp120 |
| 1.1% | LuaJIT: lua51.dll 0.3%, its generated code 0.8% |
| 0.1% | SDL2; fmod, fmodstudio and steam_api about 0 |

Synchronisation spinning (ConcRT plus the yields it causes) is about 13%, and
pewasm replaced exactly that (`noita_concrt.c`, lock-free critical sections).
Lua is about 1% here; world-up is not attributed yet.

### Export providers (§9.9), tier-1

4x clock, null display, cores 3 and 9, world fps over 720-1000 s:

| build | world-up | steady world fps |
|---|---|---|
| `GEX_PROVIDERS=0` | 450 s | 14.5 |
| ConcRT provider with a 1 ms `_Yield` (two runs) | 495, 441 s | 15.6, 17.8 |
| ConcRT provider, the guest's own `_Yield` (two runs) | 443, 450 s | 18.1, 18.3 |

The ConcRT provider takes msvcr120 from 11.6% of the CPU to 4.4% and the
host's `sched_yield` out of the profile. `_Yield` stays the guest's
(`SwitchToThread`, a host yield): a 1 ms wait there slowed both loading and
the steady state.

### One function at a time

`gexc cgen-c noita.exe --pe-base 0x400000 [--recovery DIR] --at ADDR`
prints a unit's C for one function. `cmp/cc.sh` (scratchpad) compiles it
with the unit flags; pewasm's is cut from `out/noita/build/app.wasm`
(`wasm-objdump -d`, function `sub_<addr - 0x3f0000>`). Wasm instructions,
the function and (before eaf64e45) its outlined loop:

| noita.exe | pewasm | gex 21012db5 | gex 1827b993 | gex now (native flavor) |
|---|---|---|---|---|
| 0x724580 (worker hot spot) | 11,903 | 37,712 | 28,493 | **10,845** |
| 0x872300 | 1,152 | | 1,238 | **765** |
| 0x70b110 | 162 | | 250 | **197** |
| 0xdd6ba0 | 715 | | 3,867 | **1,245** |
| 0x864060 | 1,004 | | 1,249 | **994** |

What differed from pewasm's C, and where gex now stands:

| | pewasm | gex before | gex now |
|---|---|---|---|
| registers | `uint32_t` locals | `uint64_t`, wrap/extend between operations | `uint32_t` (1827b993) |
| memory access | `MEMR32(a)`: no check (`MEMCHECK=0`) | guard on every address, after the displacement | none natively (the unguarded flavor, in the unit key); the browser keeps the guard (a00fb489) |
| displacement | in the memarg | added before the guard | in the memarg |
| write-back at calls | spill the roots a pure callee's call tree names (`opt_program`), all of them otherwise | every dirty register (§4.1) | the same as pewasm for a callee of the unit with a complete summary that cannot reach the kernel, unwind or leave; registers it never touches stay in locals and are written back on the leave path (fae67c12) |
| reload after calls | all roots, then LLVM drops the dead | every used register | what the continuation reads, minus a preserved set (1827b993) |
| flags | elided across calls and returns (fixpoint) | written back at every call | not passed to callees (`flags_abi`); at returns still written when the caller's `need` asks (a runtime check where pewasm has a fixpoint) |
| loops | inline | inline copy plus an outlined loop function (V8 OSR, §7.2) | inline natively (native flavor) |

Kept on gex's side, off the fast path but still costing instructions: the
per-function patched-page check and depth guard at entry, the return check
(`ret == ret_pc && sp == sp0 + delta`), and the resume dispatch at call
continuations (R's §9 return-0 path). These are most of the remaining
difference on 0x70b110 and 0xdd6ba0.

ABI assumptions changed (for R): calls are taken not to read the
arithmetic flags; registers a pure callee never touches are not in the
context during the call (only in the caller's locals) -- a memory fault
inside such a callee sees the caller's older values, as in pewasm.

Still open: each call writes back every dirty register (§4.1's state at a
host-visible point), where pewasm writes back the callee's inputs; vector
registers reloaded after calls because partial lane writes read them;
loop outlining for V8's missing OSR (not needed natively).

## Runtime: state at the 2026-10-03 wrap-up (stream R, merge/abi9)

Commits after the §9 ftab work (65a3c7d5 .. 602a9497):
- aae0e2c1: units' return check knows `ret N` (stdcall, struct returns),
  as F's de1acfd7 did for the JIT.
- a20c8a92: a unit body resumes only at its call continuations. Resuming
  at every block nearly doubled the largest bodies, and wasmtime refused
  noita.exe's unit ("Code for function is too large").
- 01e88bab, e8a3cc94: `GEX_PERFMAP`, and `b_<addr>` names on tier-1
  modules.
- e8282628: `--wayland-out null`, tools/modattr.py, the per-module
  attribution above.
- 10dc2c01, 7994d41a, 212d4f11, 5c7f0cfb: export providers (§9.9.1), with
  the ConcRT and crt_mem providers and their numbers above.

**The 514 s / 9.4 fps tier-1 baseline** came from a CPU budget, not a
regression. `taskset -c 3,9` gives two hardware threads; S3's 207 s ran on
six (0-2,6-8). wasmtime compiles tier-1's modules on every CPU in the
affinity mask, and Noita's threads share them. On 6 threads this branch's
tier-1 gives 208 s, 14.8 then 19.9 fps, 5.3 GB, which matches S3. On 2
threads merge/main gave 468 s and this branch 509 s, so there is no
regression.

**World-up with providers.** One run gave 539 s against 450 s without
providers. The paired A/B traced it to the provider's 1 ms `_Yield`:
with the guest's own `_Yield` world-up is 443 and 450 s, the same as
without providers. World-up is not attributed by module yet; Lua and
tier-1 translation are the suspects.

**Units: blocked.**
- noita.exe's unit now compiles: 131 MiB wasm, 210 s of Cranelift on two
  threads on a cold run (the compiled form is cached beside the unit).
- But fae67c12 ("whole-image units write back only what a pure callee
  touches") breaks every unit-backed NT run with Wine's prefix units: Wine's
  ntdll unit ends in `NtOpenFile of the NT path ""` under GEX_NT_STRICT.
- Bisected: 1827b993 and a00fb489 pass, fae67c12 fails.
- Repro: build syswow64/ntdll.dll's unit alone into a fresh cache, then run
  crates/gex-ld/tests/pe/bin/x86/hello32.exe with `--cgen`
  (stream R's scratchpad/abi9/bisect-ntdll.sh).
- Not caused by functions left to tier-1: ntdll's build leaves none.
- `GEX_CGEN_WB=all` restores the full write-back, but it is not in the unit
  key, so it needs its own cache.
- Earlier "nt --cgen 48/48" results ran without prefix units under the key
  of the time, so they did not exercise Wine's units. The coordinator gate
  should build prefix units before its --cgen leg.
- So there is no units fps at the 4x clock yet.

**Next, in order:**
1. Fix or gate fae67c12 (F's area). Then units at the 4x clock on 3,9 and
   on 6 threads, with modattr attribution and nest exits per second.
2. Host memcpy, about 3.7%: `__memmove_avx_unaligned_erms`. Find its
   callers (GL uploads, wasmtime's memory.copy libcall, NT copies).
3. Math providers: the IEEE-exact ones first (floor, ceil, modf, sqrt;
   `floor` returns in x87 ST0), then `__libm_sse2_*` under a differential
   test against msvcr120.
4. The heap family, all at once (§9.9.2 step 4).
5. A direct call from units to providers, only if the units profile shows
   the stub's fcache probe (a §9.9.1 deviation).

The browser can drop the guard once its NT layer's guard limit equals the
memory size (no host data above the guest's addresses).

## Stream F's state at wrap-up (2026-10-03)

merge/fjit @ 77f4ef26 (merge/abi9 merged up to 212d4f11). Commits since
the first report, in order: 1827b993 (32-bit GPRs, displacements in the
memarg, 32-bit guard, partial reload, `flags_abi`), a00fb489 (unguarded
native flavor, in the unit key), fae67c12 + 70297e1f (minimal write-back at
pure calls, as pewasm's A*(g); the second fixes the first), the native
flavor's no-outlining commit, 77f4ef26 (depth guard only where the call
tree can grow without bound), and the tools: 5ae8acfe (`gexc cgen-c`,
`GEX_PROF_START`), 90649c55 (`prof-report --modules`).

Gates on these: lockstep tier-1 against `--cgen` 30/30 on gi386 and gx64
in both modes (flags ignored, and `GEX_CGEN_LIVE=0` exact); tests/nt
`--cgen` 48/48, and 50/50 with the prefix's DLL units built by 77f4ef26;
Tier 0 gi386 in node (the browser flavor): cg 2.810 -> 2.700 x emcc.

**Noita units fps: not measured.** The units built before 70297e1f failed
in Wine's ntdll (the write-back bug that commit fixes); the full unit build
for 77f4ef26 (prefix done in 16.5 min, images about an hour more) was
stopped at wrap-up. The hot-function table above is for the native flavor
before 70297e1f and 77f4ef26 (both only remove instructions).

To measure, on cores 4 and 10:

```
S=<scratchpad>/fjit
$S/build-units.sh <gexc> $S/cgcache    # cgen-prefix into $S/prefix, then the 8 Noita images
GEX_CGEN_CACHE=$S/cgcache NULLDISP=1 GEX_NT_TIME_SCALE=4 CPUS=4,10 GEXC=<gexc> \
    $S/run-noita2.sh units 1100        # R's recipe (null display); providers on by default
```

and compare the `peek` slope with pewasm's 76-84. Report noita.exe's unit
share and the rest separately (`tools/prof-report.py --modules`, R's
`tools/modattr.py`).

Next, in order:
1. That measurement, with a profile (`GEX_PROF_START` past world-up).
2. The return check and the resume dispatch at call continuations are
   the remaining entry/exit overhead on small functions (0x70b110: 197
   against pewasm's 162). pewasm has neither (it tests `recomp_jmp_pending`
   after calls); both carry R's §4.2/§9 invariants, so any change goes
   through R.
3. Flags at returns: gex computes them and stores them when the caller's
   `need` asks; pewasm drops them by a whole-program fixpoint. The
   computation stays live in gex (0xdd6ba0's early exits).
4. Vector registers reloaded after calls because partial lane writes read
   the whole register (lane liveness).

## Noita units, measured (stream F, 2026-10-03)

merge/fjit @ 5b559302 (abi9 ea470321 merged), every unit rebuilt under the
current key into a private cache (`build-units.sh`: cgen-prefix 28 min,
the eight Noita images about 50 min). Start-up check: R's
`bisect-ntdll.sh` (ntdll unit + hello32 --cgen) prints its line;
`tests/nt/cgen-prefix.sh` 50/50.

R's parity recipe: null display, `GEX_NT_TIME_SCALE=4`, cores 4 and 10,
world fps the slope of `peek` in 120 s windows after world-up:

| build | world fps (steady) | world-up |
|---|---|---|
| pewasm `out/noita/build` | 76.2, 83.9 | 27 s |
| gex units + providers (default) | 25.1, 25.5, 24.0 (830-1150 s) | 590 s |
| gex units, `GEX_PROVIDERS=0` | 13.5, 13.7, 13.5 (790-1150 s) | 433 s |
| gex tier-1 (earlier, PPM display) | 15.5-16.8 | 559 s |

By module (units + providers, CPU 700-1150 s, `tools/modattr.py`): units
48.2%, host 23.5%, tier-1 17.6%, other wasm 10.8%. noita.exe 54.7%
(f_724580 alone 10.7%), msvcr120 4.6%, Wine core about 5%, libc 13.9%
(`sched_yield` 7.1%: idle workers spinning in Noita's own yield loops; a
memcpy-family symbol, mis-named `_dl_mcount_wrapper`, 4.6%), gexc 7.6%
(NT syscalls), Mesa 1.6%. The 10.8% of other wasm is the ftab runtime
module's `bridge` (6.0%) and its neighbour (4.2%): tier-1 calling into
units. Tier-1's 17.6% is interior blocks of functions units hold
(noita.exe 0x72a5c6.., 0x88c863.., msvcr120 0x15d489c..) -- frames that
resumed in tier-1 after a leave; nest exits ran at 2.3 G over the run.
So about 28% of the time is tier-1 plus tier switching, which is R's
(leaves, resume, call-out), and 7% is the guest's spin-yield.

### Stream F, second wrap-up (2026-10-03)

**f_724580 (10.7% of CPU).** Compiled noita.exe's unit with gex's exact
wasmtime config (scratchpad `tools/wcompile`; the native function has the
perf map's size, 0xd418, so profile PCs map onto it). Most samples sit on
guest data loads (pointer chasing in a grid lookup: pewasm pays the same
misses). About 7% sit on two jump-table dispatches in the hot loops: clang's
fix-up for irreducible control flow, caused by the `gex_resume` switch,
which jumps into loops. Without the switch the function is 9,186 wasm
instructions instead of 10,845, and the 78/40/32-entry dispatch tables go.

**Local resume (WIP commit on merge/fjit, awaiting R).** A failed call
resumes only at its own continuation (`gex_lc_<block>`), which keeps the
CFG reducible. Waiting on R: does anything need entry at a continuation
from outside (SEH, longjmp), and how often does the cross-continuation
case fire. Gated: ntdll + hello32 repro, lockstep (flags ignored) 30/30 on
gi386 and gx64. To do before un-WIP: exact-mode lockstep,
tests/nt/cgen-prefix.sh, a unit rebuild (coordinated), Noita fps.

**World-up (warm units, providers, null display, 4x, cores 4,10):** first
frame at 59 s, world-up at 276 s in this run (433 and 590 s in the two
earlier runs; the 590 s run also compiled noita.exe's 122 MiB unit cold,
205 s). CPU 0-280 s: host 91% (gexc 76%, libc 15%), units 4.4%, tier-1
2.9%. The host share is Cranelift compiling tier-1 modules (regalloc2,
egraph and lowering are the top symbols, together well over half of the
CPU): code the units do not hold (resumed interiors, Wine DLLs without
units, runtime-generated code) is translated block by block and compiled
at start-up. pewasm compiles nothing at start-up (one AOT module): 27 s.
Not fixed, as asked. Levers: units for every DLL the run maps, fewer
tier-1 interiors (R's leaves), Cranelift opt level none for tier-1, a
compiled-module cache for tier-1 (gex-cache) across runs.

**Next for F:** un-WIP local resume after R answers; then the flags-at-
returns fixpoint (about 5% of instructions on f_dd6ba0, nothing on the
hottest functions) and vector reloads, only where hot functions show them.

## Runtime, stream R, second session (2026-10-03)

Commits on merge/abi9:
- ea470321: `tests/nt/cgen-prefix.sh GEXC`, the --cgen gate leg with Wine's
  core DLLs as units under the current key. 50/50.
- 5ec26034: the x64/sync CancelWaitableTimer flake. The test raced the
  10 ms period: as on Windows, a cancelled timer keeps a period that already
  elapsed signalled. Now 10/10 per arch under --cgen.
- 1593fa58: a tier-1 miss no longer copies the set of translated blocks.
  That copy was hashbrown's RawTable clone, 0.65% of tier-1's CPU in
  memcpy. The rest of host memcpy (3.7% in all) is Cranelift and object
  writing for runtime translation, plus Vec growth (ptsamp, direct-caller
  attribution); units should remove most of it.
- 1b27744d: the crt_math provider, plus tests/providers. floor, ceil,
  modf, sqrt and `_libm_sse2_sqrt_precise` are bit-exact against both
  Microsoft's msvcr120 (Noita's) and Wine's in every tested case. Details
  in the commit: errno differs per build, the x87 sqrt double-rounds unless
  the control word is 0x27f, and the SSE2 entry falls back outside the
  default FP state. Run `tests/providers/run.sh GEXC
  [~/src/noita-wasm/Noita/msvcr120.dll]`.
  - Tier-1 A/B, two pairs at 4x on cores 3 and 9: 16.3 and 19.9 world fps
    with these exports skipped, 18.7 and 18.9 with them. That is within
    noise; they were about 1% of the CPU.
  - sin, cos and pow stay lifted. Microsoft's `_libm_sse2_*_precise` are
    table-driven Intel routines that no libm port matches to the bit, and
    units compile them already.
- c3120dd8: GEX_NT_STATS reports trap-out resumes by cause (the NT syscall
  name, or the exception code and pc) and target, every 60 s.

**Leaves (the coordinator's step 1): where they come from.** F's units run
(u7.log) had 2.5 G nest exits. Those are tier-1 → unit calls through the
bridge; with no predicted return, each one is counted.
- The busiest sampled pairs are a msvcr120 ConcRT worker loop (around
  +0x14894) calling kernel32/kernelbase `SwitchToThread` and the syscall
  trampoline, then noita.exe's 0x72a5xx and 0x88c7xx interiors. These are
  long-lived loop frames running on tier-1.
- The same run had 11,081 trap-out resumes. A resume discards every wasm
  frame of its thread, so any loop frame above it stays on tier-1 until it
  returns, and a worker's or the game loop's frame never returns.
- The first job is c3120dd8's cause table on a units run: which syscalls
  (NtContinue after SEH, NtSetContextThread) or exceptions those resumes
  are. Not measured yet: the overlay cache is ready (below).

**F's local-resume proposal: my answers.**
- **(a)** Nothing has to enter a unit body at a continuation from outside
  on a hot path. The resume switch catches three things:
  1. a callee that came back to this call site's continuation with its
     return check failed. F's local test at the site covers it.
  2. a longjmp to a setjmp in this frame, i.e. a different call site's
     continuation. That can leave: the state is all in the context, and
     tier-1 finishes the frame. It's rare.
  3. trap-out resumes. These never reach the switch: the frames are gone.

  So approve local resume. The central switch only serves case 2.
- **(b)** Not measured: no counter on switch hits. The structural answer is
  that the expensive case isn't the switch. It's frames already discarded
  by trap-out resumes, and a continuation entry in the unit can't recover
  them either.
- **The fix for (b):** don't let a demoted frame run on tier-1. When the
  root or the fallback reaches a call continuation inside a function a unit
  holds, compile the rest of the frame as a function: §9.2's region tier,
  via stream F's function JIT, starting at the interior address with the
  same call/return contract. Its loops then run JIT code, and its calls are
  wasm calls, not bridge round trips.
- **Make resumes rarer, too.** Only an exception that unwinds past wasm
  frames needs a trap-out. An exception handled inside the faulting frame
  could resume in place.

**Next, in order:**
1. A units run with c3120dd8, to get the resume causes. Use the overlay
   cache `scratchpad/abi9/ovl`: symlinks to F's cgcache-cg7 plus
   msvcr120, ucrtbase and msvcrt rebuilt under the crt_math provider key.
   Run with `GEX_CGEN_CACHE=$S/ovl`, my prefix (same layout), and
   `GEX_NT_CGEN_BUILD=0`.
2. Region entries for demoted frames (the region tier, from the
   fallback/root path). Coordinate with F, since it's the JIT.
3. Avoid the trap-out for exceptions handled in the faulting frame.
4. Then sched_yield (7.1% in F's run: Noita's own yield loops) and the
   heap.

### Stream F at the machine restart (2026-10-03)

merge/fjit @ 9eacbbd5 (abi9 65f9a719 and the AOT-share line merged).

Since the step-4 report:
- c9100b46: local resume is final. Exact-mode lockstep 30/30 on gi386 and
  gx64; tests/nt/cgen-prefix.sh 50/50.
- cg8 units (local resume, before the IAT fix), 4x, null display, cores 4
  and 10: world fps 27.2-28.6 steady (cg7: 25), world-up 443 s, resumes
  10,843 (all during loading), tier-1 module cache 0/10,821 hits (the
  key includes the build fingerprint, so every new binary starts cold;
  tier-1 compiling cost 327 s of CPU).
- AOT share on cg7 (tools/aot-coverage.py): 73.3% of guest CPU. noita.exe
  60.5% units, 21.3% tier-1 interior, 1.5% missed; msvcr120 2.1%
  interior, ntdll 0.75%, kernelbase 0.69%.
- d11b5464: the IAT slots are zeroed before recovery. In the file they
  hold hint RVAs; read as constant jump tables, every import thunk was
  garbage that "never returns", so 395 noita.exe call sites lost their
  continuation (code after memset/sqrt calls ran on tier-1). New
  recovery: 0 noreturn calls (was 395), 100,215 functions (was 100,147);
  the 38 pewasm-only starts were all behind such calls. Units tail-call
  unresolved jumps (now every thunk) through the function table.
- 101a7477: a switch's default does the same.
- Gates on the binary with all of the above (gexc-cg11): exact lockstep
  gi386 30/30; gx64 exact, flags-ignored mode and cgen-prefix.sh not run
  (stopped for the restart).

Exact next step: finish the gates on `scratchpad/fjit/gexc-cg11` (exact
and flags-ignored lockstep on gx64, tests/nt/cgen-prefix.sh), then
`build-units.sh gexc-cg11 cgcache-cg9` (keep cg7/cg8: R's overlay reads
cg8), then the Noita run (`GEX_CGEN_CACHE=.../cgcache-cg9 NULLDISP=1
GEX_NT_TIME_SCALE=4 CPUS=4,10 GEXC=gexc-cg11 run-noita2.sh u11 1200` with
GEX_PROF/GEX_PERFMAP past world-up) and tools/aot-coverage.py against
cgcache-cg9, splitting interior into what the IAT fix removed and what
remains for R.

## Runtime, stream R: fallback causes (2026-10-03, before the reboot)

merge/abi9 = merge/fjit 9eacbbd5 (F merged R's 65f9a719 and ed6c838b).

**Measured on F's cg7 units, through an overlay cache** (`scratchpad/abi9/ovl`):
4x clock, null display, cores 3 and 9.

- **Trap-out resumes: fixed.** 10,868 before, 20 after (65f9a719).
  - 99% were tier-1 dispatch misses: the miss import translated the block,
    then trapped out of every wasm frame, units included.
  - It now returns `DISPATCH_MISS_REDISPATCH` and the miss stub
    re-dispatches in place.
  - The 20 left are exceptions at load time. F's "NtAccessCheck" labels were
    my c3120dd8 mis-read of LAST_SYSCALL; the correct names are NtContinue
    and NtRaiseException.
  - With `--strace`, the main-thread raises are 0x40010006
    (DBG_PRINTEXCEPTION_C) from OutputDebugStringA called by steam_api at
    startup. The frames below are noita.exe 0x80b90b and 0xdfda10 (the game's
    main), so these trap-outs can demote the main thread's outer frames to
    tier-1 for the whole run.
- **Leaves: about 1.6 M/s.** None were to patched pages, and none were from
  the depth guard. Almost all were unit functions ending in an unresolved
  jump (`jmp [IAT]` import thunks: noita.exe 0xdfe2xx, kernel32+0xf53c,
  msvcr120+0x6cc98, kernelbase+0x67620) that left instead of tail-calling.
  F's d11b5464 and 101a7477 fix this (the IAT hint RVAs, and tail calls for
  unresolved jumps and switch defaults). cg9 has them.
- **AOT share** (the GEX_NT_STATS line, GEX_PROF): 59-65% of guest.
  - Units 57-64%, tier-1 22-28%, runtime 13-14% (the bridge: tier-1 frames
    calling units).
  - Steady world fps: 25.8 before the dispatch-miss fix, 22.7 and 26.7 after
    (seed noise).

**Next:**
1. The same table on cg9. Make an overlay of cg9 plus the three
   provider-keyed units (msvcr120, ucrtbase, msvcrt; about 6 min with
   `cgen-image`). Run `GEX_CGEN_CACHE=$S/ovl GEX_NT_CGEN_BUILD=0 NULLDISP=1
   GEX_NT_TIME_SCALE=4 GEX_PROF=... GEX_PERFMAP=1 run-noita.sh TAG 1000
   --cgen`, then read the "AOT", "resumes by cause" and "leaves among them"
   lines.
2. Exceptions without frame loss. NtRaiseException should dispatch nested
   (no trap-out), and the guest's unwinding should land in its unit frame.
   - Wine's `__TRY/__EXCEPT` lands with `__wine_longjmp` at the `setjmp`
     call's continuation, and MSVC longjmp does the same. That is a
     *different* call site's continuation than the call that failed, which
     F's local resume does not take.
   - So: a resume entry for setjmp-like call continuations only. Recovery
     marks callees that return twice, and only functions calling one get
     the (small) continuation switch.
   - A stopgap with the same effect at startup: a kernelbase
     OutputDebugStringA/W provider that skips the raise. gex never has a
     debugger, and no DBWIN monitor runs in a gex session.
3. Region entries for frames demoted by anything left: the safety net.
4. If depth-guard leaves appear, raise the native budget. The runtime half
   is in (64 MiB wasm stacks); the units' budget constant is F's.

## Stream F: cg9 measured (2026-10-03)

merge/fjit 3fe58dc8 (abi9 78089b0e merged), binary `gexc-cg12`, every unit
rebuilt into `cgcache-cg9` (prefix 23 min, noita.exe 21 min). New since the
restart:
- a73be6f2: pewasm's profile-guided devirtualisation (§8.1: `if (t == K)
  f_K(); else <table call>`) and its small-leaf copies (§3.6: a `static
  inline` copy in each translation unit that calls a leaf of at most 96
  lifted statements, so clang inlines it across the image's units). The
  hottest unit functions were one-instruction virtual getters
  (f_4ac0d0 `mov eax,[ecx+14h]; ret` 4.4% of CPU, f_5b01c0 1.8%), which
  pewasm inlines. noita.exe: 4,305 devirtualised calls from 3,281 hints,
  20,386 calls to leaf copies.
- **The cg9 hints are pewasm's.** `out/noita/icall-profile.txt`
  (PEWASM_PROFILE_INDIRECT) converted with tools/icall-hints.py (pewasm
  places noita.exe at 0x10000). gex's own collector came after
  (d39b63e6: `gexc run --record DIR prog.exe` works for PE programs, and
  icall-hints.py reads DIR/profile.json); the file format is the same.
- b007fe99: tests/nt icall (hot indirect calls; 9.6 s without hints, 4.9 s
  with, same output).

**Gates** on gexc-cg12: lockstep exact 20/20 (gi386 and gx64: ack, calls,
cgoto, eh, sjlj, virtual, fp, simd, interp, qsort), tests/nt --cgen 52/52,
cgen-prefix.sh 52/52.

**Noita, 4x clock, null display, cores 4 and 10** (R's run on 3,9 and G's
on 5,11 at the same time): world-up 200 s (cg8: 443 s; this includes a 104 s
cold Cranelift compile of noita.exe's unit). World fps in 120 s windows from
world-up: 29.6, 37.4, 43.3, 42.4, 44.1, 43.9, 38.6, 35.3, 38.6 (cg8:
27-28.6). R's run on 3,9: 43.3. This is one number for the import fix, R's
ffa2cec9 and devirtualisation with leaf copies together.

**AOT share** (aot-coverage.py, profile 600-1250 s): **99.6%** of guest CPU
(cg7 73.3%, cg8 71.4%); the run's own line: AOT 99.5% (units 97.8%,
providers 1.7%, tier-1 0.4%).

| % of guest | units | tier-1 |
|---|---|---|
| noita.exe | 81.40 | 0 (cg8: interior 22.77, missed 1.53) |
| msvcr120 | 5.87 | 0 (cg8: interior 2.17) |
| ntdll | 4.97 | 0 (cg8: 0.77) |
| kernelbase, opengl32, kernel32, msvcp120 | 1.3-1.8 each | 0 |
| SDL2 | 0.05 | missed 0.26 |
| lua51 | 0.11 | missed 0.11, interior 0.02 |

cg8's noita.exe "missed" was the code after import-thunk calls (every hot
address follows a `call <memset/sqrt thunk>`), which d11b5464 recovers. Its
interior was frames demoted by leaves at those thunks and by loading
resumes; with d11b5464, 101a7477 and R's 65f9a719/ffa2cec9 none is left
(nest exits 0.6k/s, 10 resumes). What remained:
- SDL2: one switch whose table re-resolved to 84 cases, then 1, for ever;
  the recovery fixpoint never settled, so its weak (relocation-only) starts,
  SDL's dynapi `SDL_*_REAL` bodies, were never promoted. 56988e74: a
  re-resolved table only grows. 754 -> 2,553 functions. (pewasm uses
  native SDL and Lua, so neither has a pewasm counterpart.)
- lua51: LuaJIT's VM ops, reached only through its runtime-built dispatch
  table (the Lua exclusion).

**The process uses about one core of two.** GEX_PROF samples at 1 kHz of
process CPU: 643k samples over the 650 s window is about 990 per second.
pewasm's bench uses 1.75-1.85 cores. Per world frame at 44 fps: guest code
about 13.7 ms, host about 8.6 ms. The hot functions' wasm is at parity with
pewasm (0x724580 10,845 against 11,903 instructions; 0x864060 994 against
1,004; 0x70b110 176 against 162). So the gap is waiting (latency, a
serialising lock), not code: handed to R.

**The machine is not quiet.** Two qemu VMs, Steam and the other streams
share it (load average 14 on 12 threads); pewasm's bench on 4,10 during
F's gates gave 49.2 world fps (BUSY ms/frame t0 9.09, t1 11.02, t2 10.90,
t3 0.44, t4 0.49; 1.75 cores), not 76-84. Compare gex with pewasm in paired
runs under the same load.

## Stream F after cg9 (2026-10-03, evening)

**Measuring.**
- **Per-thread CPU:** `run-noita2.sh` (scratchpad) samples
  `/proc/PID/task/*/stat` every 10 s into `TAG.threads`; `threads.py`
  gives each thread's cores and ms of CPU per world frame over a window.
- **Per-thread profiles:** GEX_PROF writes a profile per busy thread beside
  the process one (34cfede2), `<stem>-<thread>-<tid>.prof`.
  `threadwork.py TAG` splits each thread's ms/frame into guest code,
  yield/spin and the rest. pewasm's BUSY counts its `_Yield` sleeps as
  waiting, so compare the real-work column with pewasm's t0/t1/t2.
- **Fixed seed (corrected 2026-10-04, stream E): the fixed-time variables
  do not fix the world.** `GEX_NT_FIXED_TIME=1767225600` (54cb7124) and
  PEWASM_FIXED_TIME pin the date, but Noita does not seed its world from
  it. noita.exe 0x883540: the world seed (stored at 0x1205004) is the
  WORLD_SEED magic number if set, else drawn from the engine's RNG, seeded
  at start-up. Every "fixed seed" run before this note had a random world
  on both runtimes. The pin, the same on both:
  - **`port/seedpin/`**: a mod (`mod.xml`, `init.lua` calling
    `ModMagicNumbersFileAdd`, a `magic_numbers.xml` with WORLD_SEED written
    per run) and the save's `mod_config.xml` enabling it. The game install
    is never written. `NOITA_WORLD_SEED` picks the seed (default
    1767225600; 0 = random).
  - **pewasm:** `port/bench.cjs` (so also `port/tput.sh`) writes them into
    its in-memory FS before start (`/mods/seedpin`,
    `/locallow/Nolla_Games_Noita/save00`) and prints `world seed N`, read
    from the game's memory.
  - **gex, `gexc run`:** run noita.exe from a farm, the install as symlinks
    plus the mod (`port/seedpin/pin.sh farm DIR`, once; unit keys are by
    content, so the units still match), and after the run script deletes
    the save, `pin.sh save <prefix>/drive_c/users/gex/AppData/LocalLow/Nolla_Games_Noita/save00`.
  - **gex, an ejected project:** `pin.sh mod <rootfs>/<game dir>` and
    `pin.sh save <rootfs>/<save00>` (run-ejected.sh does both).
  - **Check:** `GEX_NT_PEEK=0x1205004` prints the world seed as `peek=`.
    "Setting random seed: N" is the engine's start-up RNG, not the world,
    and differs every run with or without the pin.
  - Verified 2026-10-04: world seed 1767225600 on pewasm (two runs), on
    the ejected project (two) and on `gexc run` from the farm (one);
    without the pin, 731381664 (pewasm) and 775215642 (gex).
- **Pairs:** pewasm runs right before and after each gex run, on the same
  cores. On this machine pewasm alone ranged from 40 to 92 world fps
  during the day, with load.
- **Check the AOT line before trusting a run.** A recovery-keyed change
  re-keys every unit, and a newer binary then finds none and runs on
  tier-1 silently.
- **Bases.** Since f050a455, SDL2 (no DYNAMIC_BASE) loads at its linked base
  0x6c740000 and the DLLs after it pack one slot lower. build-units.sh has
  the new bases (BASES= overrides).

**cg10** (h1 binary, 4,10). On the first run five DLLs had no unit at their
new bases (AOT 92.6%): 57.9 world fps, random seed. With all units,
another seed: 41.7 (AOT 99.8%). R, fixed seed on cg10: 74.5 against
pewasm's 92.3, 0.81.
- Per thread (random seed, 57.9 fps): gex-main 0.99 cores (17.05 ms/frame),
  two workers 5.3 ms/frame each.
- pewasm BUSY: t0 5.7, t1 5.9, t2 5.8 ms/frame.
- R: gex-main's excess is `_Yield` spinning on the job counter; the
  critical path is the worker job chain.

**Codegen against pewasm** (wasm instructions, cgen-c, before devirt and
leaf copies):

| fn | gex | pewasm |
|---|---|---|
| 724580 | 10,517 | 11,903 |
| 72a150 | 5,133 | 4,272 |
| 70c6e0 | 5,314 | 4,505 |
| 70bdc0 | 8,552 | 7,623 |
| 70b200 | 1,307 | 1,059 |
| 72b390 | 325 | 143 |

- About 500 of each big function's extra instructions are the 64-bit
  context slots.
- 8327893c: 32-bit loads of i386 GPRs.
- 255e04fa: GEX_CGEN_NARROW_STORES=1, 32-bit stores as pewasm's CpuState.
  In the unit key and off until the runtime makes every writer of those
  slots zero-extend (queued for R). Lockstep diverges without that: the
  Linux layer writes syscall results sign-extended.
- The rest is the call contract: the 64-bit `ret` argument, the return
  check and the local-resume compares at each call site.
- Flags at returns (pewasm's §3.4 fixpoint) measured at 0-1% on the large
  functions: not ported.
- cfc3ed08: leaf copies bounded by guest instructions (32), so
  compare-heavy getters like 0x72b390 are copied and lose their flag math
  when inlined.

**Fixes for other streams:**
- 417b638f (E): an instruction ending a PE section in a zero byte was cut
  short (the rebased lua51 thunk).
- d9e0dd0e (B): leaf copies' outlined loops are named apart.
- 4f2223e4: profile writes raced under Peggle.

**Peggle** (gex's own PE recorder, tier-1, 10.6 fps). Indirect calls:
wined3d.dll 223M, ntdll 123M, opengl32 54M, popcapgame1.exe 13M.
wined3d, ddraw and d3d8 had no layout base and so no unit. 126d8269 adds
the Direct3D set to tools/nt-cgen-core.txt, with `cgen-prefix --append`,
which keeps every existing base (dry run: 18 entries added, none moved).
It is applied to the staged prefix with cg12.

### Stream F at the reboot (2026-10-03, late)

merge/fjit @ 2a256612. In it, since d71ef103:
- abi9 adb0efbc merged;
- e3cc3434: 32-bit i386 GPR stores on by default; R's efcb8bfc
  zero-extends every host writer;
- 8327893c: 32-bit loads;
- cfc3ed08: leaf copies bounded by 32 guest instructions;
- 3c9f6f82: the fjit clock, wasm-safe;
- 126d8269: `cgen-prefix --append` and the D3D set in nt-cgen-core.txt;
- 2a256612: fixes a panic in 417b638f's fetch, which stopped cgen-prefix on
  user32/rpcrt4. Any binary between 417b638f and 2a256612 crashes a full
  cgen-prefix.

**cg11 was abandoned** (that panic). **cg12 not built.** Its gates ran on
`scratchpad/fjit/gexc-cg19` (= 2a256612) until the reboot:
- lockstep exact gi386 30/30, gx64 30/30 (GEX_CHECK_I386_HIGH=1);
- tests/nt --no-cgen 60/60;
- cgen-prefix.sh was mid-run (stopped).

`cgcache-cg12` has only objects from that gate.

The staged layout (~/.cache/gex/wine-11.15/gex-layout) is **untouched**:
the --append step had not run.

**Resume, in order:**
1. Rebuild gexc at merge/fjit HEAD (or reuse gexc-cg19) and rerun
   `scratchpad/fjit/cg12.sh`. It runs:
   - every gate: lockstep, tests/nt, cgen-prefix, the i386-high scan of
     the x86 NT tests under --cgen, providers;
   - then `cgen-prefix ~/.cache/gex/wine-11.15 --append --layout-only`,
     whose diff must have no removed lines (scratchpad
     `append-removed.txt`);
   - then `build-units.sh` (now `--append` on the run prefix) into
     cgcache-cg12.

   About 3 h on a loaded machine.
2. Send the coordinator the new layout's path and sha256 and the cache path
   (for B, E and U).
3. `scratchpad/fjit/abba.sh`: fixed-seed ABBA, cg10 (`gexc-cg10f`, cg10's
   source plus fixed time and per-thread profiles; checked: it finds all
   eight Noita units) against cg12 (`gexc-cg19`), with pewasm before,
   between and after, on cores 4,10. Then `threadwork.py TAG`: each
   worker's real-work ms/frame against pewasm's t1/t2 (about 5.9 ms),
   R's question (codegen, or wake latency).
4. Not started: the coordinator's engine A/B. Run the Tier 0 corpus's
   identical wasm under node and under wasmtime (gexc), per kernel and
   geomean, with wasmtime's config noted (bounds checks, guard pages,
   Cranelift opt level, shared memory), plus a 724580-sized microbench.
5. Peggle: hints are staged in `scratchpad/fjit/cgcache-peg/icall/`
   (popcapgame1.exe at 0x400000; wined3d, ddraw, d3d8 at their appended
   bases), from gex's own recording (`scratchpad/fjit/peggle/rec`, tier-1,
   10.6 fps). After cg12's layout append: units for popcapgame1, bass and
   j2k, then `tests/games/peggle.sh -- --cgen` on 5,11 (coordinate with
   B), against tier-1.

## Runtime, stream R: exceptions without frame loss (2026-10-03)

merge/abi9 ffa2cec9, 78089b0e (F merged both into merge/fjit 3fe58dc8 for
cg9).

- **NtRaiseException dispatched nested** (ffa2cec9).
  - KiUserExceptionDispatcher runs in a nested loop on top of the thread's
    wasm frames, as a user-mode callback does, instead of trapping out of
    all of them.
  - Control that reaches the host with the stack above the raise has left
    the dispatch: a handler's longjmp, a catch or __except continuation,
    or NtContinue. The host sees it at the fallback, a tier-1 miss, a
    safepoint or a syscall's resume.
  - The loop then ends, and the syscall's own `ret` takes the guest there:
    the return address is written just below the target SP. The frames
    below see an ordinary leave.
  - Units only, at most 8 deep. `GEX_NT_EXC_NEST=0` turns it off;
    `--strace` logs each one; GEX_NT_STATS counts `exc_nested` and
    `exc_escaped` on the ftab line.
- **Unit bodies resume inside themselves** (same commit, gex-cgen).
  - Under local resume, `gex_resume` returned 0, so a leave for a
    continuation the frame owns but no call of it returned to (a setjmp's,
    a catch's, a callee's leave for one of our blocks) unwound every frame
    further out onto the fallback.
  - Now, with PC inside the function's code and SP at or below sp0, the
    body tail-calls the table at PC under its own (need, ret). That is
    `ftab_tail`, as for unresolved jumps; the CFG stays reducible.
  - Only the rest of that function runs on the call-out. The call-out's
    awaited return may come back up to 64 KiB above `sp_want` (the rest of
    the interior entry's frame).
- **debug_string provider** (78089b0e): OutputDebugStringA/W in kernel32
  and kernelbase return without raising DBG_PRINTEXCEPTION_C, as pewasm's
  pe_host_debug_string. This removes the steam_api start-up raises that
  demoted Noita's main-thread frames.
- **Gates:** tests/nt/cgen-prefix.sh 50/50; tests/providers (crtmath,
  odstr); lockstep gi386 and gx64, exact and flags-ignored, 30/30 each.
  On seh.exe x86 under --cgen, the frame-handler NtContinue lands at
  guarded_call's recovery label (the interior tail call), and a vectored
  EXCEPTION_CONTINUE_EXECUTION lands at RaiseException's own continuation
  (local resume). Hardware faults (int3, ud2, access violations) still trap
  out through `fault()`.

### The fallback table on cg9, and the main thread (stream R)

**cg9 run** (gexc-cg12 = merge/fjit 3fe58dc8, 4x clock, null display,
cores 3,9, 1150 s; profiled from 700 s):

| | cg9 | cg8 |
|---|---|---|
| world-up | 206 s (cold compiled-module cache) | 443 s |
| steady world fps | 43.3 | 27-28 |
| nest exits | about 1k/s | 3.6 M/s |
| tier-1 blocks | 16k | 248k |
| peak RSS | 4.2 GB | |

- **AOT share:** 99.3-99.5% of guest CPU (units 97.7%, providers 1.7%,
  tier-1 0.5%, runtime 0.1%). Tier-1 is SDL2 functions recovery missed (F),
  LuaJIT's mcode and lua51.
- **Resumes:** 10, all NtContinue at thread starts, where no wasm frames
  are lost.
- **Exceptions:** 2, dispatched nested and escaped.

**Off-CPU, per thread** (/proc state and wchan at 200 Hz):
- Noita is main-thread bound. On cg12 the main thread was 77% CPU plus 19%
  blocked in `drm_syncobj_array_wait_timeout`: the SwapBuffers readback
  waiting on the GPU.
- The workers were about 21% CPU each and waited in futexes most of the
  rest.
- After 519377bb (no readback for the null display) the main thread is at
  99% CPU, runnable 100% of the time, and the process uses 1.62 cores.

**The main thread's syscalls** (GEX_NT_STATS now counts syscalls by
number, and samples 1 in 64 with the first non-Wine return address on the
EBP chain), per second in steady state:

| rate | syscall | from |
|---|---|---|
| about 340k | NtYieldExecution | Noita's job waits, `while (job->pending > 0) { CurrentScheduler::Id() != -1 ? Context::_Yield() : _Thrd_yield(); }` (noita.exe 0x45d051, 0x728301, 0x864760, 0x72391d, 0xd29014, 0xd154b4) |
| about 300k | `__wine_unix_call` | opengl32, one per GL call (noita.exe 0x9d8f56..0x9d9194, 0x446cd7) |
| about 16k | NtQueryPerformanceCounter | |
| about 5k each | NtClose, NtDuplicateObject | |

- About 4,900 yields and 4,300 GL calls per frame, all on the main thread,
  so the fixed cost of a syscall is the lever.
- pewasm's `_Yield` (port/src/noita_concrt.c) is `pe_thread_sleep32(1)`.
  On the main thread that spins up to 1 ms for a host state change; on a
  worker it is a 1 ms cond wait. emscripten's `sched_yield` yields to no
  one. gex's is a host `sched_yield` (8.9% of CPU).

**Fixes, paired runs** (A B B A on 3,9, steady 420-840 s, with a pewasm
bench run on the same cores):

| change | A | B | pewasm |
|---|---|---|---|
| 7f91009e + 519377bb: no trap backtraces, no null-display readback, uploads copied once | 51.5, 47.6 | 77.3, 67.7 (+46%) | 48.6 |

### What a real display would need to skip the readback (pricing only)

The readback stays for displays that show frames. Natively, the host GL
renders into an EGL pbuffer (gex-gl), and gex-wayland's compositor takes
only wl_shm buffers, so a frame has to come back through guest memory into
the GDI surface. Skipping that natively needs:
- gex-gl rendering the default framebuffer into a GBM buffer object, as an
  FBO on an EGLImage, and exporting it as a dma-buf
  (EGL_MESA_image_dma_buf_export);
- gex-wayland taking `zwp_linux_dmabuf_v1` buffers, from the guest
  compositor side to the host output, and compositing them as a
  subsurface over the window's GDI pixels: the frame and caption stay
  wl_shm;
- double buffering and release events, so the GPU doesn't stall on a
  buffer still being scanned out;
- the PPM display keeps the readback (it writes files).

Roughly a few days natively. The browser already avoids it: stream B and
S12's direct-frame path (`gl_target` with SURFACE_ATTACH and PRESENT) hand
Mesa's frame to the compositor on the GPU.

### Fixed seed: the job chain (stream R)

From here every run uses `GEX_NT_FIXED_TIME=1767225600` (pewasm's bench
uses the same date), cg10 units, cores 3,9, ABBA, with pewasm's bench right
after on the same cores.

| change | A | B | pewasm |
|---|---|---|---|
| acbb146e: null-display GL on a GL thread, async SwapBuffers (A = GEX_GL_SYNC=1) | 73.0, 76.3 | 72.1, 76.3 | 92.3 |

So **gex is at 0.81× pewasm** with the seed fixed. Async GL takes
SwapBuffers on the main thread from 6.4 to 2.2 ms/frame without moving
fps. The freed time goes into the main thread's job waits: the frame is
bounded by the job chain, which is main dispatching, workers waking,
workers running noita.exe's job code, and main's `_Yield` loop seeing
`pending` reach 0.

**Main thread, per frame** (ptrace samples on cg10, about 70 fps):
- guest about 40% (noita.exe 30%; ntdll, msvcr120 and opengl32 about 3%
  each);
- the job-wait spin about 28% (`sched_yield`);
- host about 28%: futex 8%, NT syscall path 6%, Mesa 5%, GL encode 3%,
  memcpy 3%.
- Per frame that is about 5.7 ms of guest, near pewasm's t0 of 4.5-5.7,
  plus about 4 ms of spin and about 4 ms of host.
- 5,300 GL calls/frame, all through one unix call each (glDisable 895,
  glActiveTexture 856, glUseProgram 830, ...); pewasm's null GL runs
  none.

**Scheduling** (/proc schedstat per thread, 60 s at about 74 fps, os
yield):

| thread | CPU/frame | runqueue wait/frame | wakes/frame | delay per wake |
|---|---|---|---|---|
| main | 12.7 ms | 0.7 ms | | (spinning; preempted 900/s) |
| tid-2, tid-3 (busy workers) | 4.6 ms | **4.5 ms** | 17.7 | p50 294, p90 470 µs |
| tid-4, tid-5 (idle workers) | 0.1 ms | 4.3 ms | 9.5 (1 ms bounded waits) | about 450 µs |
| gex-gl | 0.9 ms | 1.0 ms | 2.8 | |

- The busy workers wait as long for a CPU as they run. On two hardware
  threads the main thread spins on one: a Linux `sched_yield` only
  yields to a task on the same CPU's runqueue, so workers queued on the
  other one stay queued.
- pewasm's main thread spins too (`main_spin`: up to 1 ms watching
  `host_generation`, which every thread's host call bumps, then sleeps).
  Its workers' bounded 1 ms waits are the same as gex's ConcRT provider.
- Next: A/B the main thread's yield (`GEX_NT_YIELD=os`, `spin`,
  `sleep:N`).

### Stream R state at the reboot (2026-10-03, merge/abi9)

Committed. The tip gate is tests/nt 60/60, cgen-prefix 60/60,
tests/providers ok, and exact plus flags-ignored lockstep gi386 and gx64
30/30. Commits:
- fb7dfd52: APCs nested.
- acbb146e: null-display GL thread.
- b8791ff2: the `sleep:N` yield knob.
- efcb8bfc: i386 zero-extended GPRs and GEX_CHECK_I386_HIGH.
- adb0efbc: resumable faults. Re-keys units; needs a rebuild (F).

**Yield ABBA** (fixed seed, cg10, cores 3,9, gexc built from efcb8bfc,
whose unit keys are cg10's):
- os against spin (y2): os 65.1, 50.8; spin 65.0, 72.4; pewasm 84.4.
  Too noisy (load average 22-26).
- os against sleep:50 (y3): os 74.5; sleep:50 78.7. b2 and a2 were not
  run (stopped for the reboot).
- Scheduling with sleep:50:
  - main CPU 940 → 604 ms/s;
  - busy workers' CPU 342 → 395 ms/s each;
  - their runqueue wait 333 → 207 ms/s;
  - wake delay p50 294 → 147 µs.

**Next step:**
- Rerun y3 in full: `scratchpad/abi9/ab2.sh y3 GEXC GEXC
  'GEX_NT_YIELD=os' 'GEX_NT_YIELD=sleep:50'`, with a binary keyed for
  whatever cache is current.
- If sleep:50 holds, try 20 and 100 µs.
- Then decide the main-thread yield policy with the coordinator. It is not
  pewasm's (main_spin): a yield that frees the CPU on two hardware
  threads.

**Open:**
- **The glcompat query flake that stream E saw on both gexc and gex-rt.**
  Not reproduced here: glcompat x86 and x64 passed every time in this
  session, including after acbb146e. First suspects:
  - acbb146e's GL thread: it only engages with a null display, which
    glcompat doesn't use;
  - a query whose reply is read before its batch has run.
  Reproduce with E's command and GEX_GL_SYNC=1 to separate the two.
- int3 and ud2 as resumable faults: they're HostCall statements.
- The Detours-on-provided-export log and test, once E's
  providers-survive-rewrites change lands.

### Stream F at the second reboot (2026-10-04, early)

merge/fjit @ this commit. Since 01864a5d: abi9 ed951711 merged (docs);
632bb7ba `GEX_IMAGE_WASM` (below); 97f86d21 = B's 3cf08afe cherry-picked
(Src `repr(u8)`, jump-table capacity `hi - lo`; re-keys units, so it rides
cg13; no test for the capacity: observing it needs an allocator hook).

**cg12: built.**
- Gates on `scratchpad/fjit/gexc-cg19` (= 2a256612), GEX_CHECK_I386_HIGH=1:
  lockstep exact gi386 30/30, gx64 30/30; tests/nt --no-cgen 60/60;
  cgen-prefix.sh 60/60; i386-high under --cgen 0 reports; providers ok.
- Staged layout `~/.cache/gex/wine-11.15/gex-layout` appended (D3D set, 18
  lines, 0 removed): sha256 63b13c676f9a3fb9f8e61cedce566d7c205be217e4b176a4f4e4db7a6ee97c2a.
  The run prefix's layout equals it.
- `scratchpad/fjit/cgcache-cg12`: prefix (82 units) and all eight Noita
  images. noita.exe's unit failed silently in build-units.sh (its log only
  shows recovery; cause not seen) and was rebuilt alone: 100,222 functions,
  128 MiB wasm, 4,306 devirtualised calls (3,281 hints), exit 0
  (`scratchpad/fjit/noita-cg12.log`). Check that line after any full rebuild.

**Engine A/B (step 4).** The same gex module under wasmtime (gexc) and V8
(node 26.7). `gexc build --exhaustive [--cgen|--fjit-image]` with a shared
memory; native runs it via `GEX_IMAGE_WASM=PATH gexc run ELF` (632bb7ba);
node runs the same bytes with the memory import's shared flag cleared (the
result is byte-identical to a `--no-shared-memory` build) through
pewasm/bench/gex-run.mjs, single Worker. Cores 4,10, 3 rounds, medians;
metric the guest's kernel-us. Script `scratchpad/fjit/eng.py`, results
`scratchpad/fjit/eng/eng-<guest>.txt`.
- wasmtime 27: Cranelift `Speed`, 4 GiB static reservation plus 2 GiB guard
  (no explicit bounds checks), signal-based traps, tail-call convention
  (SysV callee-saves), no epoch/fuel. V8: trap-handler bounds (none
  explicit), Liftoff then dynamic tier-up, wasm inlining on.

| guest | clang tier (cg) wasmtime/node | fjit-image wasmtime/node |
|---|---|---|
| gi386 | 1.066 (27 kernels) | 1.133 |
| gx64 | 1.100 | 1.139 |
| ga64 | 1.095 | 1.172 |

Process CPU is the other way (cg 0.89-0.92): node compiles every run.
Worst for wasmtime on cg: sand 1.46-1.89, simd, matmul, interp, qsort,
umap, cxxstring; best: tree, cgoto, list (0.68-0.86).
- **Not V8's inlining:** `--no-wasm-inlining` gives gi386 cg 1.065 (was
  1.066); gx64 1.089 on the 18 kernels run before the reboot (1.085 with
  inlining on the same 18). `scratchpad/fjit/eng/noinl-*.txt`.
- **sand's hot function** (wasm func 6477, 96% of samples; Cranelift
  0x3618 bytes, TurboFan 12,732): Cranelift keeps the loop's induction and
  carried values in stack slots, storing and reloading them each iteration
  (`mov %r10,0x30(%rsp)` the hottest instruction, then reloaded twice), with
  the many cold call-out exits (each spilling six v128 and eight GPR context
  slots) raising the pressure. TurboFan keeps them in registers. Dumps:
  `eng/f6477.cl.s` (wcompile, gex's config), `eng/f6477.tf.s`
  (`--print-wasm-code-function-index`), `eng/sand.prof` (PC samples).
- Note: Noita's native units are the unguarded flavor, so the
  `cmp 0xffff0000; cmovb` clamp in these browser-flavor modules is absent
  there.

**Resume, in order:**
1. `scratchpad/fjit/abba.sh` (cg10 `gexc-cg10f`/cgcache-cg10 against cg12
   `gexc-cg19`/cgcache-cg12, pewasm before/between/after, fixed seed, 4,10),
   then `threadwork.py` per tag; check each run's AOT line.
2. pewasm profile for sub_334580: `node --cpu-prof` on port/bench.cjs
   (chain.sh has the command), against gex's f_724580 ms/frame from the
   ABBA profiles.
3. Optional: rerun `ENG_NODE_FLAGS=--no-wasm-inlining eng.py gx64 3
   --configs cg` in full; a wasmtime upgrade A/B on regalloc (sand).
4. Peggle (step 5 of the previous list), with the layout now appended.

### Stream F after the second reboot (2026-10-05 night; working notes, see the final state below)

- master e3917bac merged into merge/fjit (816c4a98; gex-cgen untouched by
  master, so unit keys unchanged: bm-hu2/bm-hui still match).
- **Gating heads handed to the coordinator** (each master + one change):
  - merge/fjit-ung (wt/fjit-ung) 00806c66: N1, the page default unguarded
    (fdbb1d9e re-applied; pe.rs line ~265: `p.set('unguarded', m.unguarded
    ? '1' : '0')` instead of setting it only when true).
  - merge/fjit-inl (wt/fjit-inl) f5ca966f: d54095e5 + leaf inlining on by
    default (`GEX_CGEN_EXP=NOINLINELEAVES` off). Re-keys every unit. NT
    suite --cgen 81/81 with it (logs/nt-inldef-cgen.txt).
- **i1 on edger** (unguarded units, one shim set from ffa941cb+fdbb1d9e,
  seed pinned, 4 cores/8 threads; `scratch/fjit/e/seq-i1.out`):

  | leg | world fps |
  |---|---|
  | pewasm p1 | 47.2 |
  | no inlining u1 (bm-hu2) | 56.4 |
  | inlining i1 (bm-hui) | 62.7 |
  | pewasm p2 | 47.4 |
  | inlining i2 | 60.2 |
  | no inlining u2 | 50.9 |
  | pewasm p3 | 46.4 |

  Means: pewasm 47.0, no inlining 53.65 (14% faster than pewasm),
  inlining 61.45: 14.5% faster than no inlining and 31% faster than
  pewasm. (u is 10% apart between its legs; i's legs are both above both
  u legs.)
- Client-state fast path: c2fa872b completed by b1fe3b3d (glfast covers it:
  a draw from client arrays the layer sees enabled only through
  GL_FAST_ARRAYS, the colour array off, texcoord per client unit, 600k
  calls; with an instrumented gexc under --cgen none of the 600,006 calls
  reached the layer, under --no-cgen all did). NT suite 81/81 with and
  without --cgen (logs/nt-cs-*.txt). c6d82c65 adds `GEX_NT_GL_FAST=nocs`
  (only the client-state calls back in the layer) for the page A/B.
- **cs1 (client-state fast path A/B, edger)**: shims from c6d82c65, bm-hui
  with the new nt_unixcall provider; seed pinned, 4 cores/8 threads;
  `scratch/fjit/e/seq-cs1.out`. pewasm 46.2/48.0/49.7 (47.97); fast path on
  (c) 62.6/61.6 (62.1); client state in the layer (n, `GEX_NT_GL_FAST=nocs`)
  60.9/62.2 (61.55) world fps. On is 0.9% faster than nocs (62.1 vs 61.55),
  inside the noise (n2 beat c2). Kept: it is correct and costs nothing.
- master's merge/fjit-ung and merge/fjit-inl went into integ/2026-10-05g;
  merge/fjit-inl is merged into merge/fjit (3c4a17ea: inlining on).
- **Inlining's cold growth** (5563fda1): the 34 per-`ret` mismatch exits
  were 70% of what inlining added to 0x70bdc0 (measured by replacing them
  with a trap: 13,682 -> 10,725 wasm instructions). They now share one exit
  per pop per function (reads the address back from the stack): 11,583
  instructions, object 28,310 bytes (33,553 before, 23,830 without
  inlining). The fallback calls are as the original calls were. Test:
  tests/nt retskip (leaves that return past their call, `ret`/`ret 4`).
  `GEX_CGEN_EXP=NOSHAREDRETEXITS` restores the old exits.
- **mono_ns** (cs1 profile, all threads ~0.8 ms of clock calls per world
  frame): two sources, `tick` on every safepoint and NT syscall (it read
  the clock to rate-limit the USD update) and the guest's QPC (provider
  `p_qpc`: f_ddc410 on two workers, f_9be020/f_9b9680 on two others).
  545c12b6: a thread reads the clock on one tick in 8
  (`GEX_NT_TICK_EVERY`, 1 = before). QPC left as it is (it has to be
  precise; the arrow-function import is most of a call).
- **Dispatch misses**: gex_dispatch_miss had 582 ms of *self* time in 30 s
  on the main thread (0.31 ms per world frame), called from tier-1's run
  loop. Its executable check scanned every VMA; 545c12b6 uses a binary
  search (`MemoryMap::vma_at`) and adds miss counters to the NT stats line
  (`gex-nt: misses all/behind/returned/dense/fjit/knows/translate`).
- **s1 (edger, seed pinned, 4 cores/8 threads; `scratch/fjit/e/seq-s1.out`)**,
  this tree's units (bm-s: inlining + shared exits), shims from 545c12b6:
  pewasm 43.9/46.5/49.1 (46.5); s (tick 1 in 8) 64.5/63.7 (64.1); t
  (`GEX_NT_TICK_EVERY=1`, the old tick) 63.6/64.5 (64.05) world fps. The
  tick change is neutral (0.1% faster, noise); both arms 38% faster than
  pewasm (64.1 vs 46.5). (cs1's arm on the older tree: 62.1, other
  session.)
- **Dispatch misses, explained** (s1 counters): ~130 a second, every one a
  call-out's return (`returned`); none translate. gex_dispatch_miss's self
  time (0.29 ms per world frame on the main thread, unchanged by the VMA
  search) is the trap that ends each call-out (V8's unwinding is booked to
  the trapping frame): call_out is 0.65 ms per frame inclusive, the trap
  0.29 of it.
- **Call-outs leave by returning** (uncommitted when written; see git log):
  `DISPATCH_MISS_LEAVE` (-2) from gex_dispatch_miss makes tier-1's miss
  stub `return`, so `enter` returns to JS's #callOut, which reads
  gex_callout_hit. Only when the hit is the innermost call-out and its
  loop called `enter` with nothing nested since (JS sets
  `gex_callout_leave_ok(me+1)` before enter, 0 in the other loops and on
  leaving), and never for a miss whose caller drops the answer (cache_op
  SLOT_CONFLICT / FTAB_TIER1 set NO_LEAVE first: bridge, tier-1 and cgen
  target checks). `GEX_NT_CALLOUT_TRAP=1` traps as before (A/B arm).
- **tick bug found**: with 545c12b6 the tick after a timed wait was
  sampled too, so GetTickCount after a wait could be stale by the whole
  wait (browser NT x64/sync "timeout lasts" failed). Fixed: `tick_now`
  after waits, sampling only at safepoints and the sampled syscall path.
- INLINECALLS (GEX_CGEN_EXP: inline small functions with up to two calls,
  noita.exe 0x70b110/0x70b050 into 0x724580's loop) is a parked patch,
  `scratch/fjit/inlinecalls.patch` (func.rs; re-keys units). C for
  0x724580 grows 5% with it (812 vs 775 KB); unmeasured.
- **Parked for the 2026-10-05 late-night reboot.** Head: see `git log -1`
  on merge/fjit (this doc commit, on top of c6bc9217). Unmeasured in the
  page: 5563fda1 (shared ret exits; size only, measured above), 545c12b6
  (tick reads the clock 1 in 8; VMA binary search in a dispatch miss;
  miss counters). NT suite 83/83 without --cgen; with --cgen 82 + fixedbase
  failed only because gexc was replaced mid-run, rerun passed.
- **s1: TO RERUN** (queued, never started; cancelled by PID for the
  reboot). Caches on gooner and edger (synced): `target/bm-s` (this tree's
  units, unguarded, inlining + shared exits; noita.exe unit
  cf648aa3...-ie77f2dfea53061fa); edger's wt/fjit and web/public/gex shims
  are from 545c12b6 (match bm-s). Command (from ~/gex-sandbox on gooner):
  `T=/home/claude/gex-sandbox/wt/fjit/target; timeout 14400 scratch/bench/bench.sh -- scratch/fjit/e/seq.sh s1 /home/claude/gex-sandbox/wt/fjit 240 "s=$T/bm-s" "t=$T/bm-s@GEX_NT_TICK_EVERY=1" > scratch/fjit/e/seq-s1.out 2>&1`
  (s = tick 1 in 8, t = old tick). If gex-cgen changes first, rebuild:
  `GUARD=0 JOBS=12 scratch/fjit/build-bm.sh $T/bm-s $T/bm-hui`, shims
  with scratch/fjit/build-shims.sh, then `scratch/fjit/e/sync.sh bm-s`.
  Note: shims/browser/build.sh also rebuilds target/x86/release/gexc; don't
  run it during an NT suite.
- After s1: the `gex-nt: misses all/behind/returned/dense/fjit/knows/translate`
  lines in scratch/fjit/e/s1-s1.log on edger, and the profiles'
  gex_dispatch_miss / mono_ns (analysis scripts inline in this session:
  per-file chain counts over cpuprofile parents) against cs1-c1
  (scratch/fjit/prof/cs1-c1 on gooner).
- Other heads: merge/fjit-ung 00806c66 and merge/fjit-inl f5ca966f are in
  integ/2026-10-05g (coordinator gating). wt/fjit-ung and wt/fjit-inl are
  done; their target dirs (fjit-inl/target) can go.

### N1: unguarded units in the page by default (stream F, 2026-10-05)

n1 on edger (guarded bm-h vs unguarded bm-hu, same tree 129c5720, one shim,
seed pinned, 4 cores/8 threads; `scratch/fjit/e/seq-n1.out`):

| leg | world fps |
|---|---|
| pewasm p1 | (page busy, no world) |
| guarded g1 | 43.9 |
| unguarded u1 | 48.7 |
| pewasm p2 | 44.5 |
| unguarded u2 | 47.8 |
| guarded g2 | 40.9 |
| pewasm p3 | 47.2 |

Means: pewasm 45.9, guarded 42.4 (8% slower than pewasm), unguarded 48.25
(5% faster than pewasm, 14% faster than guarded). So the page's `cgen=` now
means the unguarded flavor (`unguarded=0` for the guarded one; the ejected
page passes its flavor explicitly). A page run with `cgen=` needs the
unguarded units (`gexc cgen-image` without `GEX_CGEN_GUARD=1`); a cache of
guarded units needs `&unguarded=0`.

### Stream F final state (2026-10-06; stream F stopped here)

Branch merge/fjit, master 2d9055f9 merged. Everything below is measured on
edger (Quadro P620, 4 cores/8 threads, seed pinned, pewasm paired in the
same sequence, `scratch/fjit/e/seq-*.out`), world fps.

**What landed in the branch, and the numbers that decided it:**

| change | commit | A/B (world fps) | kept |
|---|---|---|---|
| page default: unguarded units (N1) | fdbb1d9e / 00806c66 | unguarded 48.25 vs guarded 42.4; pewasm 45.9 | yes (in integ-g) |
| leaf-call inlining on by default | d54095e5, f5ca966f | i1: 61.45 vs 53.65 without (14.5% faster); pewasm 47.0 | yes (in integ-g) |
| client-state GL calls in the ring | c2fa872b, b1fe3b3d, c6d82c65 | cs1: 62.1 vs 61.55 with `GEX_NT_GL_FAST=nocs` (0.9%, noise); pewasm 47.97 | yes (correct, free) |
| inlined `ret` mismatches share one exit | 5563fda1 | size: noita.exe 0x70bdc0 11,583 vs 13,682 wasm instructions (15% smaller); not timed alone | yes (re-keys units) |
| VMA lookup by binary search; miss counters | 545c12b6 | no effect on the miss cost (the misses never reached that check) | yes (harmless) |
| tick reads the clock 1 in 8 | 545c12b6, 11cf9f1b | s1: 64.1 vs 64.05 (noise); pewasm 46.5 | default back to every read (269346a1), knob `GEX_NT_TICK_EVERY` |
| a call-out leaves by returning, not trapping | f3acedb3 | co1: 64.05 vs 62.1 trapping (3.1% faster; 67.1/61.0 vs 63.8/60.4); pewasm 48.2 | yes (`GEX_NT_CALLOUT_TRAP=1` traps) |

co1's profile: gex_dispatch_miss self time on the main thread went from
0.29 to 0.001 ms per world frame.

**Tests at the final head:** native NT suite with and without --cgen (new
cases retskip and the client-state checks in glfast), browser NT suite
49/49, crate tests of gex-cgen, gex-kernel and gex-emit.

**Dropped / parked:** INLINECALLS (inline small functions with up to two
calls: noita.exe 0x70b110/0x70b050 into 0x724580's loop), patch
`scratch/fjit/inlinecalls.patch`, unmeasured; QPC clock calls left as they
are (they must stay precise).

**Remaining profile items, for later** (co1/s1 main thread, ms per world
frame; the main thread works ~11.6 of ~16 ms, the workers ~6.7, the GL
thread ~7.4):
- mono_ns 0.28 (main) + ~0.05 per worker: QPC from the game (p_qpc) and
  the tick; the arrow-function import is most of a call.
- GuestMem::copy_within in the GL layer's cmd_with 0.16: the GL data copy
  into the ring (texture/buffer uploads), inherent unless the host reads
  guest memory directly.
- serve_x87 0.10: msvcr120's x87 `_CIatan2` path at 64-bit precision
  (fldcw 0x133f) runs on the soft f80; a provider would need bit-exact
  results.
- dlmalloc 0.22 and p_memset 0.53 (workers, f_722640): provider heap and
  memset, mostly bandwidth.
- WebGPU side on the GL thread: createView 0.43, beginRenderPass 0.40,
  submit 0.66, writeBuffer 0.35 (each above pewasm's: 0.17, 0.15, 0.36,
  0.23): view caching and pass merging in mesa-wink / the gl-bridge.
- Guest code: noita.exe 0x724580 family on the workers (~4 ms of 6.7);
  0x864060, 0x70b200/0x70bdc0/0x70b110 next.

**Caches:** gooner and edger `wt/fjit/target/bm-s` (this tree's units,
unguarded, inlining + shared exits); bm-hu2/bm-hui (older keys) can go.
