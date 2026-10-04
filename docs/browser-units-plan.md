# Plan: function-tier units (format `gex-cgen-image 3`) in the browser

Status (stream S12, 2026-10-02): step 1 done -- units load and run in
the browser shim under node, and `tests/nt/run-browser.sh --cgen` passes
(shims/browser/src/cgen.rs, `#cgen*` in shims/browser/js/gex.mjs). Steps
2-3 (Chrome) wait on the function-tier ABI decision (docs/merge-plan.md
§9); loading, sharing modules between Workers and the key carry over.

How step 1 came out, against the plan below:
- The key is `gex_cgen::image::image_key`, the native host's own function
  (crates/gex-key under it), never a copy.
- The unit's bytes stay out of the shim, as planned: JS loads
  `<key>/c.wasm` and `c.entries` through the Env (`loadUnit`; a directory
  under node, a synchronous request in a Worker), compiles once and shares
  the `WebAssembly.Module` with sibling Workers; the shim gets the entry
  list, checks the mapped bytes and publishes.
- The browser's runtime slots do not start in table 0 (the global slot
  allocator starts above the statically recompiled images' tables), so
  there is one body table per dispatch table (`gexc.bodies0..7`,
  `gex_abi::BODY_LIMITS[t]`, S3's dispatcher).
- `--cgen` needs every program image's unit built natively at the base it
  is mapped at (`gexc cgen-image --pe-base`), and Wine's from
  `gexc cgen-prefix`; any change to gex-abi or gex-cgen changes every key.

## §9 parity, stream B (2026-10-03)

- **Dense block tables** (`gex_emit::dense`, shims/browser/src/dense.rs):
  tier-1's dispatch tables 4..7 (the run's) no longer exist per Worker.
  Their slots are a `u32` map per table in the process's memory, naming an
  index in one more table, `blocks`, with an entry per installed block;
  dispatch reads the map and falls back to the classic table when the map
  word is zero. Peggle: 166k blocks against 1.74M slots per Worker.
  Half-Life: the classic tables (4.85M entries in each of 11 Workers) ran
  the renderer out of V8 heap at about 95 s; dense it plays c1a0 with the
  renderer's memory bounded. `?dense=0` / `GEX_DENSE=0` is the old layout.
- **Units on the §9 ABI:** per-Worker `gex.ftab`, units placed by element
  segment at `__table_base`, `gex.lookup`, the probe cache, bridges in the
  dense or classic tier-1 slots, and the function table's runtime module
  rewritten for dense dispatch. Unit keys match the native host's (export
  providers' stubs included), and the providers are bound after each image
  from `<cache>/providers/`. `tests/nt/run-browser.sh --cgen` with
  browser-flavour units (`GEX_CGEN_GUARD=1 gexc cgen-prefix|cgen-image`;
  PE32+ images at the base the page gives them, 0x400000 for an exe): 48/49
  (x64/cxx: no unit, the guarded flavour fails to compile it).
- **In the page:** `nt.html?cgen=1|2` reads units from `/gex-cgen/`
  (the harness serves `GEX_CGEN_CACHE` there).
- **The call-out:** index 1 runs tier-1 nested (`gex_env.call_out`, JS's
  loop as for NT callbacks) until the guest returns to the calling body:
  a translated return is caught by a watch stub in this Worker's table
  entry for it, an untranslated one by the miss path.
- **AOT share:** tests/nt/aot-share.mjs classifies V8 CPU profiles of the
  Workers by module (units, providers, runtime, tier-1, host), the native
  GEX_NT_STATS line's accounting; browser-noita.mjs prints it with
  `GEX_NT_PROFILE`.

### Stream B state at the reboot (2026-10-03, late)

merge/web9 @ this commit; merge/web9-fjit (worktree wt/web9f) = it + F's
3c9f6f82 + a WIP browser function JIT.

- Done and gated (run-browser.sh 49/49 plain and --cgen, games ok): dense
  tables, units + providers + icall keys, the call-out, in-place dispatch
  misses (65f9a719's half), nested NtRaiseException (ffa2cec9's half; seh.exe
  x86: exc_nested=20 escaped=19 with GEX_NT_EXC_TRACE=1), GEX_NT_FIXED_TIME,
  fault-pc bit 62 masked.
- pewasm in the page (port/browser.mjs, scratchpad `web9/run-pewasm-page.sh`,
  fixed seed, 4x): about 58 world fps. gex's Noita-on-units number is next.
- **Noita browser units** (GEX_CGEN_GUARD=1, gexc built from 0dc27c10, copy
  at scratchpad `web9/gexc-web9b`): the whole prefix is built into
  `target/bnoita2/cgen` (2991 s); the eight Noita images were not (noita.exe
  was compiling when stopped). Resume with scratchpad
  `web9/build-noita-images.sh web9/gexc-web9b` (the bases the page uses:
  noita 0x400000, lua51 0x1560000, msvcr120 0x15c0000, SDL2 0x6c740000,
  msvcp120 0x16b0000, fmod 0x1730000, fmodstudio 0x18f0000, steam_api
  0x1a10000; nice 19, 2 jobs), copy `target/bnt2/cgen/providers` in, then
  `GEX_CGEN_CACHE=target/bnoita2/cgen GEX_WINE_PREFIX=target/bnoita2/prefix
  GEX_NT_PAGE_FLAGS='&cgen=2' GEX_NT_PAGE_ENV=GEX_NT_FIXED_TIME=1767225600,GEX_NT_TIME_SCALE=4
  GEX_NT_PROFILE=worker,60,20 run-noita-page.sh TAG --gameplay` (scratchpad),
  and check the AOT line before trusting the fps.
- **Function JIT (WIP, wt/web9f):** shims/browser/src/fjit.rs translates a
  `gex.lookup` miss in the shim and publishes it as `FJIT | n`; tier-1 misses
  are served through bridges (`serve`), FTAB_TIER1 handled. Not green:
  GEX_FJIT=1 hello_args/sync time out under run-node; debug that first.

## Where the page is now

Noita in headless Chromium, hardware WebGPU, full speed: about 4.2
presents/s, against about 11 natively. The main thread splits into 65%
guest (tier-1), 29% the per-frame GL readback, and about 6% NT calls, waits
and glue (`GEX_NT_STATS` time lines, commit 553f14b6). The readback goes
first (the direct GL frame). After that, guest execution is the whole cost,
and units are the lever.

## What a unit is (docs/function-tier.md §11)

One wasm module per image: clang's link of the image's recovered functions,
with `e_<entry>` (nest root) and `ib_<entry>` (body) for each entry. Both
sit in the unit's own table, and `gex_fnref(k)` names them. The module
imports the gex host functions, the memory, and the per-store dispatcher's
`gexc.dispatch` and `gexc.call_known`. Installing a unit copies `e_` into
the dispatch table at the entry's slot and `ib_` into the per-store body
table. noita.exe's unit is about 195 MiB of wasm for 100,147 functions.
Wine's units come from staging.

## 1. Compile time

- **Never through the shim.** A unit's bytes go from `fetch` to
  `WebAssembly.compileStreaming` in JS, and only its entry list reaches
  the shim (about 100k × 12 bytes). The shim heap is capped at 2 GiB and
  fragments under large blocks; 195 MiB in it is out of the question.
- **One compile per process.** Compile on the page (or one dedicated
  Worker) and post the `WebAssembly.Module` to every thread Worker. That
  shares code by reference, and each Worker only instantiates. This is the
  path `#sharedModules` already uses for tier-1.
- **Lazy, not eager.** Rely on V8's lazy and dynamic tiering: Liftoff
  compiles a function on its first call, and TurboFan compiles it once it
  is hot. Eager Liftoff for 195 MiB is seconds of CPU and several hundred
  MiB of code, most of it never run.
  - To measure: wall time to `compileStreaming` resolution, first-frame
    delay, and time to TurboFan on the hot set (Chrome tracing
    `v8.wasm`).
- **In the background.** The program starts on tier-1, as now. The unit
  compiles beside it and is installed when ready, the way
  `publish_cgen_unit` installs natively. World load does not wait on a
  compile.

## 2. Caching

- **Chrome's wasm code cache, through the HTTP cache.** IndexedDB no longer
  stores `WebAssembly.Module`s. Serve each unit at a content-addressed URL
  (S3's cache key, which covers the image, gex-cgen, gex-abi and
  UNIT_ABI_VERSION) with `Content-Type: application/wasm`,
  `Cache-Control: immutable`, from the same origin. `compileStreaming`
  then gets V8's cached code on the next visit (it caches tiered-up code
  for modules over the size threshold).
  - To measure: second-load compile time.
- **Ship units with the bundle.** Units are built ahead of time
  (`gexc cgen-prefix` for Wine; a build step for noita.exe), not in the
  browser. There is no compiler in the page, so a missing unit means
  tier-1, as natively before the background build.
- **A unit must match the mapped image.** Refuse loudly on a hash mismatch
  (the native host compares the executable bytes) and keep tier-1.

## 3. Memory

- **The renderer holds:**
  - the 4 GiB shared guest memory (its own budget);
  - the unit's wire bytes once (195 MiB, shared by every Worker);
  - compiled code only for what ran (lazy);
  - per-Worker instance state and tables.
- **The shim** holds only entry lists and directory bases. Watch it with
  the memmap and `foreign=`: nothing of a unit's size may land in the arena.
- **Table elements.** Per Worker, instantiating writes about 200k elements
  (`e_` and `ib_`). With 10 Workers that is 2M `table.set`, tens of ms.
  Install by element segments, or by one JS loop per Worker, never through
  the shim.

## 4. Table sizes

Dispatch tables are per Worker and live on the V8 heap at 8 bytes an entry.
Noita's tier-1 tables already reach 4.8M entries per Worker, about 38 MB
each, now grown in bounded steps (553f14b6). The body table is numbered
like dispatch, so in the worst case it adds the same again per Worker.

- **Budget.** At most 10 Workers × (dispatch + body) × 4.8M × 8 B ≈ 770 MB
  of V8 heap. That is the renderer-OOM risk again. Measure before going
  further.
- **If it is too big:** the slot layout is shared with S3's units and stays
  as it is unless S3 agrees. The alternative to raise with S3 is a dense
  body table, indexed by unit function number (about 200k entries per
  Worker), with the slot-to-function map in shared linear memory (4 bytes
  a slot, once per process, not per Worker). `call_known` then makes one
  extra load.
- **Install units before tier-1 numbers their pages,** so a page gets one
  base, never two (S3's base-stability rule, dab86a66).

## Steps

1. Node first (`run-node.mjs`, same shim and glue): load a Wine DLL unit
   and noita.exe's, install them through a browser port of
   `publish_cgen_unit`/`install_cgen` (JS compiles and posts, the shim
   publishes the entries), and pass tests/nt with units (`detour` and
   `mtx` included).
2. Chrome: `compileStreaming` timing and memory for the 195 MiB unit, cold
   and cached. Check the table heap against the budget above.
3. Noita `--gameplay`, full speed, hardware WebGPU: fps and the time split,
   against tier-1 in the page and natively.
4. The software WebGPU fallback is untouched. Units are guest-side only.
