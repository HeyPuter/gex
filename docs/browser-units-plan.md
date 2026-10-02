# Plan: function-tier units (format `gex-cgen-image 3`) in the browser

Status: plan, not started (stream S12). Wait for S3's native Noita
`--cgen` against `--no-cgen` numbers before starting: if units do not pay
natively, they will not pay here.

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
