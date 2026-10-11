# Mesa's wasm size: what runs, what ships

Stream M. Branch `merge/mesa-cold`, worktree `wt/mesa-cold`; Mesa changes on
the local branch `m/cold` of third_party/mesa (off the wink pin 60202246).

The page's OpenGL is Mesa built to wasm (`gex-wgpu-replay.wasm`,
web/build-mesa-wasm.sh). On master (07f1ed19) it is 20.6 MB with names and
18.9 MB as the dists ship it (names stripped): 16.0 MB of code in 18,864
functions, 2.8 MB of data, 1.7 MB of names. The goal: cut what a page
downloads and compiles, without breaking a workload.

## 1. Baseline

The shipped files (web/build-all.sh strips custom sections for the dists):

| flavour | raw | zstd -19 |
| --- | --- | --- |
| MP (mt) | 18,898,007 | 3,305,617 |
| SP (st) | 18,669,577 | 3,300,261 |

Code by component (names; mt, master):

| component | functions | code |
| --- | --- | --- |
| Mesa core, util, gallium aux (`_mesa_`, `util_`, `save_`, `vbo_`, ...) | 6,207 | 3.25 MB |
| Tint (SPIR-V reader, IR, WGSL writer and reader) | 2,966 | 2.67 MB |
| GLSL compiler and IR | 658 | 2.16 MB |
| other (unprefixed C helpers, lambdas) | 5,691 | 2.14 MB |
| gex's wire decoder, `one()` (a single function) | 1 | 1.23 MB |
| NIR | 371 | 1.06 MB |
| SPIRV-Tools (Tint's input validation) | 1,308 | 0.98 MB |
| gex_wgpu driver, emdawnwebgpu | 288 | 0.96 MB |
| st/mesa | 773 | 0.78 MB |
| libc, libc++ | 433 | 0.42 MB |
| spirv_to_nir (vtn) | 168 | 0.37 MB |

## 2. Measuring what runs

A profiling build is `GEX_MESA_PROFILE=1 web/build-mesa-wasm.sh` (add
`GEX_MESA_THREADS=0` for SP): the release link, then
`wasm-split --instrument --in-memory` on its plain module, shipped whole.
Each function's entry stores 1 at byte `funcIndex` of linear memory, which
is free: the link puts the main stack first (`-Wl,--stack-first`, see
section 3), so address 0 up is the far end of a 16 MB stack nothing reaches.
The module exports `__write_profile`; the glue (web/gex-wgpu-jspi.js) finds
it and, every 2 s while the set grows, prints
`[gex-wgpu] mesa-profile N HITS BASE64` (bit i = defined function i) to the
console, where every page harness already keeps `gex-wgpu` warnings. Other
builds export no such function.

```
GEX_MESA_PROFILE=1 web/build-mesa-wasm.sh
# swap target/web-build/mesa-mt/build/src/gallium/targets/gex-wasm/split/gex-wgpu-replay.{js,wasm}
# into a page tree, run the workload, keep its console log
tools/mesa-hot.py target/web-build/mesa-mt/build/src/gallium/targets/gex-wasm/gex-wgpu-replay.wasm.orig \
    LOG... --also web/mesa-hot.txt -o web/mesa-hot.txt
```

`--also` keeps what an earlier list held, by name, so a new workload adds
to the hot set rather than replacing it. (The M1 numbers below came from an
earlier form of this: a link with `-sGLOBAL_BASE=65536` and no ASSERTIONS,
which wrote a cookie at address 0.)

### Hot sets (M1)

Profiling build of the master pin (18,829 functions, 15.59 MB of code; no
ASSERTIONS, so 35 fewer functions than the shipped one). Runs on edger,
Chromium, real GPU:

| workload | how | functions run | their code |
| --- | --- | --- | --- |
| Muck, to its menu (10 min) | muck-page.sh on a copy of edger's wt/unity | 3,313 (17.6%) | 8.26 MB (53.0%) |
| Undertale on W3 (10 min, 2,220 frames) | W's run-utpage.sh, tree D with this Mesa | 2,968 (15.8%) | 7.47 MB (47.9%) |
| Hollow Knight, to its menu (12 min, interpreter) | hk-page.sh on the same copy | 3,095 (16.4%) | 7.99 MB (51.2%) |
| Noita, menu then 150 s of world (63.2 world fps) | scratch/GL/np/gex-page.sh on a copy of wt/glpath | 3,006 (16.0%) | 7.97 MB (51.1%) |
| **union** | | **3,605 (19.1%)** | **8.55 MB (54.8%)** |
| margin: direct callees of the union up to 1 KB | tools/mesa-hot.py | +848 | +0.31 MB |

Four different engines agree closely: the union is only 292 functions
more than Muck alone. Copies of edger's trees (`cp -al`, Mesa swapped by
`rm` + copy, never through the hard links) kept the other streams' trees
untouched.

Most functions never run, but most of the code bytes do: the big functions
all run. The wire decoder's `one()` (1.18 MB), GLSL's builtin-function
initialiser (0.55 MB), `_mesa_swizzle_and_convert` (0.44 MB) and
`nir_eval_const_opcode` (0.38 MB) are a quarter of the code by themselves,
and each runs in every workload.

The Undertale run used master's Mesa in place of tree D's (W's MSAA patch
68c5ed1d); it drew its 2,220 frames, but WebGPU reported a CopyDst usage
error that tree D's Mesa does not, and utpage.mjs's frame counter, which
stops at the page's first GL failure, stayed at 0.

Cold code by area (Muck and Undertale union): Mesa core 2.44 MB (display
lists `save_*` 0.7 MB, `_hw_select`, ARB programs, format packing for
formats not used), Tint 1.50 MB (`ReadIR`, the IR path used only when the
AST reader fails, and the WGSL reader), spirv_to_nir 0.35 MB, SPIRV-Tools
0.35 MB. None of it is a whole subsystem a workload can do without: display
lists, GL_SELECT and ARB programs are GL features some application uses.
Hence the split (section 4), not deletion.

## 3. Trimming the build (M2)

Sizes are of the module as a dist ships it (names and DWARF stripped by
web/build-all.sh), mt unless said; zstd at -19. Mesa CPU ms per frame is
the replay's `submit_ms` (scratch/GL/replay-run.mjs on edger, Chromium on
its GPU, the last 600 frames of Noita's world trace and of HK's menu
trace, counters off), two passes in ABBA order; lower is faster.

### ASSERTIONS=2 out, the main stack first

ASSERTIONS=2 implies STACK_OVERFLOW_CHECK=2: a call to a limit check at
each of the 15,695 places the stack pointer moves, and a cookie at address
0. Nothing in gex relied on it to catch a bug: the one stack overflow a
long Unity run hit (a 16 MB pthread stack) is in a pthread, which the check
did not watch either. In its place the replay links with
`-Wl,--stack-first`: the main stack sits at the bottom of memory, and
overflowing it wraps below address 0, where the next store traps instead of
silently overwriting data. A pthread's stack has no such guard, as before.
(With STACK_FIRST, emcc also stops passing Binaryen `--low-memory-unused`;
no measurable cost below.)

| build | stripped | zstd -19 | Noita world, ms/frame | HK menu, ms/frame |
| --- | --- | --- | --- | --- |
| master (ASSERTIONS=2) | 18,898,007 | 3,305,617 | 4.885, 4.901 / 5.172, 5.091 | 6.253, 6.456 / 5.122, 5.191 |
| ASSERTIONS=0 | 18,444,998 | 3,347,542 | 4.980, 5.123 | 5.354, 5.213 |
| ASSERTIONS=0, --stack-first | 18,564,629 | 3,360,944 | 5.046, 4.349 | 6.268, 5.175 |
| + decoders kept apart (one-caller inlining off) | 18,655,290 | 3,296,522 | 5.099, 5.140 | 6.461, 5.525 |
| + Tint's SPIR-V validation off (**M2 build**) | 18,655,830 | 3,296,846 | 5.114, 5.231 | 5.201, 5.161 |
| M2 build at -Os (compile and link) | 13,158,052 | 2,798,299 | see below | see below |

Two runs each, one per ABBA pass; master's second pair is from the second
job (each job compares its builds against master in the same session). The
spread between passes of one build (up to 1.1 ms on HK) is larger than any
difference between builds: none of these steps costs measurable Mesa CPU.
SP (st) M2 build: 18,442,251 stripped, 3,294,291 zstd, against master's
18,669,577 and 3,300,261.

### -O3 or -Os, and wasm-opt -Oz

Compiling and linking all of Mesa at -Os (Tint stays -O3) takes the M2
build from 18,655,830 to 13,158,052 bytes (29.5% smaller) and from
3,296,846 to 2,798,299 compressed (15.1% smaller). It costs Mesa CPU on HK's
menu trace; four ABBA runs each, same job:

| trace | -O3 (M2 build), ms/frame | -Os, ms/frame |
| --- | --- | --- |
| Noita world | 5.107, 5.048, 5.050, 5.010 (mean 5.054) | 4.976, 4.993, 4.983, 4.721 (mean 4.918) |
| HK menu | 4.712, 4.873, 4.980, 4.733 (mean 4.825) | 6.324, 6.141, 6.248, 5.497 (mean 6.053) |

-Os is 2.7% faster on Noita and 25.5% slower on HK (1.23 ms more per frame,
all four runs), so the build stays -O3. Running only Binaryen at -Oz on the
-O3 objects (`-Oz` on the link) saves 364 KB raw (18,444,998 to
18,080,546 with ASSERTIONS=0) and 2 KB compressed: not worth a measurement.

### The wire decoder's `one()`

`one()` was 1.18 MB because tools/gen_gl_compat.py's 2,520 per-command
decoders (web/mesa/gl-compat-generated-decoders.inc) are each called from
one `case` of its switch, and Binaryen inlines any function with a single
caller whatever its size (`--one-caller-inline-max-function-size`, unlimited
by default) -- LLVM's `noinline` does not reach it. The generator now marks
them `noinline`, and the replay links with
`-sBINARYEN_EXTRA_PASSES=--one-caller-inline-max-function-size=0`: `one()`
is 166,537 bytes, the decoders 2,522 functions of 1.04 MB (35 over 1 KB),
GLSL's builtin initialiser comes apart the same way (`create_builtins`
374,567 and `create_intrinsics`). 91 KB more raw, 64 KB less compressed,
and no Mesa CPU cost (table above). The decoders are what the split can now
place one by one: a workload runs a few hundred of them.

### Tint's SPIR-V validation

Tint's SPIR-V reader ran SPIRV-Tools' validator (`spvtools::ValidateBinary`,
363 KB of code that every workload ran) on every module Mesa handed it. Mesa
wrote that SPIR-V itself, from NIR it had already validated, so in a release
the check only finds Mesa bugs. wink/patches/tint-spirv-reader-skip-validation.patch
makes it conditional on `TINT_SPIRV_VALIDATE=1` (read once; the replay takes
it through `_gex_gl_setenv`, a page through `GEX_NT_PAGE_ENV`/`?env=`). Mesa
CPU over the whole trace, where every shader translation counts, two runs
each:

| trace | validation off | validation on (`TINT_SPIRV_VALIDATE=1`) |
| --- | --- | --- |
| HK menu (3,166 frames, 49 translations) | 4.956, 4.866 ms/frame | 5.228, 5.769 |
| Noita world (2,777 frames, 54 translations) | 4.330, 4.340 | 4.627, 4.626 |

Off is 0.59 ms/frame less on HK (1.9 s of Mesa CPU over the trace, 10.7%)
and 0.29 ms/frame less on Noita (0.8 s, 6.3%). The code stays linked (the
switch can turn it back on), so the size does not change.

### Tint and SPIRV-Tools behind a warm WGSL cache

With every translation in the WGSL cache (`_gex_wgsl_cache_load` of the blob
a first run dumped; a page gets one from the site's gex-wgsl-cache.bin or
IndexedDB), the profiling build shows Tint and SPIRV-Tools do not run at
all:

| trace | run | functions run | Tint | SPIRV-Tools |
| --- | --- | --- | --- | --- |
| HK menu | cold cache | 3,962 | 1,237 (0.87 MB) | 510 (0.51 MB) |
| HK menu | warm cache (49 of 49 hits) | 2,035 | 4 (0 MB) | 0 |
| Noita world | cold cache | 3,895 | 1,256 (0.88 MB) | 505 (0.51 MB) |
| Noita world | warm cache (54 of 54 hits) | 1,959 | 4 (0 MB) | 1 |

GLSL, NIR and spirv_to_nir still run (Mesa compiles GLSL to SPIR-V before
it looks the WGSL up). Mesa CPU with the warm cache: HK 4.768, 4.781
ms/frame against 4.956, 4.866 cold; Noita 4.794, 4.867 against 4.330,
4.340, which is slower and unexplained (both runs; the misses were 0).

## 4. The cold part (M3)

web/build-mesa-wasm.sh links the replay with `-sSPLIT_MODULE=1` and splits
its plain module (`gex-wgpu-replay.wasm.orig`) with web/mesa-split.py:
the functions in web/mesa-hot.txt stay in `gex-wgpu-replay.wasm`, the rest
go to `gex-wgpu-replay.deferred.wasm` beside it. Every publish step that
copies the one copies the other (build-all's dists, build-eject-page.sh, the
mesa-wgpu-*/gl44 tests). `GEX_MESA_HOT=FILE` splits by another list,
`GEX_MESA_HOT=none` (any missing file) ships it whole.

### Loading (web/gex-wgpu-split.js, a --pre-js)

- A miss is one load of the whole cold part per instance of the module,
  however many cold functions run after it.
- In a Worker (the desktop's compositor in MP, the system Worker in SP),
  the cold part is left until a function there first runs. Once the module
  has presented a frame, the Worker fetches and compiles it in the
  background (`fetch` + `WebAssembly.compile`); a miss after that only
  instantiates it. A miss before then fetches it with a synchronous XHR and
  compiles it synchronously, which a Worker may do at any size.
- On a page's main thread (demos.js' gears in MP, the replay and smoke test
  pages) Chromium refuses a synchronous compile or instantiation of a module
  over 8 MB, so there the cold part is instantiated with the module,
  asynchronously, before it starts, and never missed. (`gexColdLazy: true`
  in the module's options keeps it lazy, for measuring a miss.)

Two things had to change in what wasm-split and Emscripten's glue do:

- Features. The module has no target_features section, and with `-all`
  Binaryen 130 writes the cold part's imports as exact function imports
  (import kind 0x20), which Chromium refuses ("Invalid import kind 32").
  With reference types it also puts the placeholders in a second table,
  while Emscripten's glue (6.0.1) calls the loaded function through the
  first, `wasmTable.get(index)`: some other function. mesa-split.py passes
  the link's own feature set.
- JSPI. Emscripten's placeholders are JS functions that load the cold part
  and then call the function from JS. The first call into a cold function
  that then waits on WebGPU (glReadPixels mapping a buffer) failed: "SuspendError:
  trying to suspend JS frames". mesa-split.py replaces each placeholder with
  a wasm trampoline (generated, merged in with wasm-merge) that calls the
  one import `gexcold.load` and then calls through its own table slot, by
  then the function itself, so no JS frame is left on the stack.
  gex-wgpu-split.js supplies `gexcold.load` through Emscripten's
  `instantiateWasm` hook.

Nothing in gex's Mesa runs on a pthread (no glthread), so one instance per
page or Worker is all there is; a pthread would load its own the same way.

### The hot list

web/mesa-hot.txt is made from the profiling build of this branch's M2 build
(24,721 functions, 15.79 MB of code; `GEX_MESA_PROFILE=1`), edger,
Chromium on its GPU, unless said:

| workload | functions run | their code |
| --- | --- | --- |
| Muck page, to its menu (10 min) | 4,219 | 4.91 MB |
| HK page, to its menu (12 min, interpreter) | 3,954 | 4.65 MB |
| Noita page, menu then 150 s of world | 3,833 | 4.62 MB |
| Undertale page on W3 (10 min) | 3,750 | 4.51 MB |
| browser-gui x86 --gl (Wine's glwindow.exe; gooner) | 2,364 | 2.63 MB |
| gears trace replay (gooner) | 3,243 | 4.85 MB |
| HK menu trace replay (3,166 frames) | 3,962 | 5.45 MB |
| Noita world trace replay (2,777 frames) | 3,895 | 5.46 MB |
| Muck menu trace replay (3,000 frames) | 4,479 | 5.95 MB |
| **union** | **4,865 (19.7%)** | **6.22 MB (39.4%)** |
| margin: direct callees up to 1 KB, of callers of up to 64 | +1,047 | +0.34 MB |
| **kept** | **5,912 (23.9%)** | **6.56 MB (41.5%)** |

The page harnesses cut console lines at about 4,000 characters, and a set
of 24,721 functions is 4,120 in base64, so the page rows above miss the last
937 functions (0.98 MB: dispatch stubs, part of Tint's IR); the trace
replays of the same games, which print in full, cover them. The glue now
prints the set in 2 KiB pieces (`mesa-profile N HITS @OFFSET BASE64`), and
tools/mesa-hot.py reads both forms (and as much of a cut line as there is).

The margin leaves out the callees of a dispatcher (a caller of more than 64
functions): the wire decoder `one()` calls all 2,522 per-command decoders,
which would otherwise all be kept.

### Sizes (M3)

As the dists ship them (stripped), zstd at -19:

| flavour | file | raw | zstd -19 |
| --- | --- | --- | --- |
| MP | master, whole | 18,898,007 | 3,305,617 |
| MP | M3: gex-wgpu-replay.wasm | 9,959,418 | 2,017,052 |
| MP | M3: gex-wgpu-replay.deferred.wasm | 9,577,387 | 1,509,353 |
| SP | master, whole | 18,669,577 | 3,300,261 |
| SP | M3: gex-wgpu-replay.wasm | 9,730,808 | 2,017,572 |
| SP | M3: gex-wgpu-replay.deferred.wasm | 9,606,743 | 1,507,155 |

What a page fetches and compiles before Mesa starts is 47.3% smaller raw
than master's (9.96 against 18.90 MB) and 39.0% smaller compressed (2.02
against 3.31 MB) in MP; 47.9% and 38.9% in SP. The two parts together are
0.88 MB more raw than the unsplit M2 build (19,536,805 against 18,655,830: the
17,425 trampolines, and the
exports the cold part imports through).
