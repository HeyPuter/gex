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
