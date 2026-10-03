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

The browser can drop the guard once its NT layer's guard limit equals the
memory size (no host data above the guest's addresses).
