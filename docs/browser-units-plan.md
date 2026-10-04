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

### Stream B state at the second reboot (2026-10-04, ~00:55)

- **Browser fjit is green** on merge/web9-fjit (wt/web9f): 3cf08afe
  (gex-xlate: `Src` is `repr(u8)`. The niche on the null guest-memory
  pointer made every fetch empty. Also jumptable `with_capacity(hi - lo)`,
  where `hi` asked for 16 GB) and 96ae953e (shim: MODULES held across
  publish, GEX_FJIT=2 trace, GEX_FJIT_MAX/ONLY/DUMP knobs). Under taskset
  5,11, `GEX_FJIT=1 tests/nt/run-browser.sh` is 49/49 and plain is 49/49.
  F is cherry-picking 3cf08afe into merge/fjit (cg13). Merge web9-fjit
  into web9 once merge/fjit carries both fixes and 3c9f6f82 (they re-key
  units).
- **Noita browser units: not finished.** The build of noita.exe at
  0x400000 (key dir `27f192cd...`) was killed after 1h40 of clang at nice
  19 with 2 jobs. About 2980 objects are in `target/bnoita2/cgen/objects`,
  and the link had not run. These are cached per object, so a rerun only
  compiles what is missing. The other seven images have not been started.
  The providers are copied in. Resume with scratchpad
  `web9/build-noita-images.sh web9/gexc-web9b` (log
  `web9/bnoita2-img2.log`), then the page run above (run-noita-page.sh)
  paired with `run-pewasm-page.sh` (pewasm about 59 world fps).
- cg12 (coordinator): layout `~/.cache/gex/wine-11.15/gex-layout` sha256
  63b13c67..., 18 D3D lines appended, and the existing bases are unchanged.
  The native cache is `scratchpad/fjit/cgcache-cg12`. Build browser units
  for the new D3D DLLs only when Peggle in the page needs them.

### Stream B: Noita on browser units, measured (2026-10-04, ~05:35)

- **Numbers** (seed pinned to 1767225600 on both, 4x clock, cores 5,11,
  real GPU; ABBA): pewasm in the page 58.4 / 50.8 world fps; gex on
  browser units 29.7 / 30.4 world fps, AOT 99.4% of guest. Round 1, before
  the two fixes below: pewasm 56.6 / 55.8, gex 1.3 / 1.2.
- **The NT trampolines need a unit** (8e96b196). Natively it is built at
  boot; the page cannot, so every syscall stub ran on tier-1 and each
  call into one from a unit (opengl32's GL thunks, ~7.7k/s) was a
  call-out ending in a trap: 1.3 world fps. `GEX_CGEN_GUARD=1
  GEX_CGEN_CACHE=... gexc cgen-image --nt-trampolines32` builds it; the
  page's key matches. -> 26 world fps.
- **Clock reads in the syscall path** (9dbecfdf): `performance.now()` is
  about a microsecond in a Worker, and each NT syscall read it four times
  (plus a BigInt each and a `syscall_name` String with four substring
  searches). Noita's main thread makes ~5*10^5 NtYieldExecution a second
  (it spin-yields for its job threads). Now every 16th call and around
  waits. -> 30-31 world fps.
- **Where the time goes now** (main thread, V8 profile): units 48%, NT
  syscalls 38% (nearly all NtYieldExecution: waiting), providers 6%;
  job threads ~40% busy, 60% in the concrt provider's `cond_wait_` (an
  Atomics.wait, which the AOT line counts as provider time: the AOT share
  overstates). The main thread's own work per frame (~17 ms) is pewasm's
  whole frame: the units themselves are slower than pewasm's code. The
  guarded flavour (a bounds guard on every guest access) is the first
  suspect; an unguarded browser layout is the lever.
- **Seed pin in the page:** browser-noita.mjs pins by default
  (NOITA_WORLD_SEED; a pin.sh farm plus the save's mod_config.xml in the
  prefix; NOITA_PEEK=0x1205004 checks it: 1767225600 seen). The scratchpad
  pewasm page takes `?seed=` and run-pewasm-page.sh passes it (world seed
  read back: 1767225600).
- **Precondition:** with `cgen=` in GEX_NT_PAGE_FLAGS, browser-noita.mjs
  fails on any "no function-tier unit" / "no provider module" line (the
  trampolines included) and on AOT under NOITA_MIN_AOT (99); the fps
  window starts after the profile.
- **Merged:** merge/web9-fjit and merge/fjit (2c83dead) are in merge/web9;
  run-browser.sh 49/49 plain and GEX_FJIT=1. That re-keys every unit: the
  measured units (target/bnoita2, gexc-web9b) no longer match this tree.
  Rebuild with scratchpad `web9/build-bunits3.sh web9/gexc-web9c` (into
  target/bnoita3: trampolines, Noita's eight images, then the prefix), and
  run the page with GEX_CGEN_CACHE/GEX_WINE_PREFIX pointing there
  (absolute paths: the bundle symlinks the prefix).

### Design: unguarded units in the browser (stream B, 2026-10-04)

**What the guard protects today: almost nothing.** A browser NT process
shares one linear memory between the guest (below 2 GiB, `ALLOC_HIGH`
0x7fff0000) and the shim (statics and stack at 0x0600_0000..0x0700_0000,
`--global-base`; heap from `NT_HEAP_BASE` 0x8000_0000 up to the memory's
maximum 0xffff0000).
- Tier-1 guards at 2 GiB (`set_guard_limit(NT_HEAP_BASE)`, a runtime
  value).
- Units and providers guard at `GEX_GUEST_LIMIT`, a *compile-time*
  constant from gex-abi: 0xffff0000, the memory's end. So a guarded unit's
  wild access in 2..4 GiB lands in the shim's heap exactly as an unguarded
  one would. The only accesses the guard changes are those at or past
  0xffff0000, which trap anyway past the memory's maximum. In the browser
  today, guarded and unguarded units behave the same; the guard is pure
  cost (pewasm: MEMCHECK=0, worth about 1.2x by itself).
- The shim's statics and stack (0x0600_0000..) are below 2 GiB, so every
  tier can reach them. The NT VM keeps them unallocated, nothing more.

**Step 0: unguarded browser units now (small, no layout change, no
safety lost).**
- The page picks the flavor: `cgen=2&unguarded=1` (default on, once A/B'd)
  sets `gex_cgen::image::set_unguarded(true)` in the shim before the first
  key, so the keys are the native `:unguarded` ones.
- Build them with gexc **without** GEX_CGEN_GUARD=1, into another cache:
  the same units as natively, at the page's bases.
- A/B: guarded `target/bnoita3` against unguarded `target/bnoita3u`, paired
  with pewasm, seed pinned.
- Cost: about 2-3 h of clang (noita.exe plus the prefix) on 0,2,6,8 at
  nice 19, a shim flag, a page flag. Native unguarded units for these
  bases are what `gexc run --cgen` builds anyway, so a prefix with Noita's
  layout may already be in `~/.cache/gex/cgen` (check the keys first).

**Step 1: isolation without a guard, the biased guest (the real fix;
large).** Guest address `a` of a 32-bit NT process lives at linear
`a + 0x8000_0000`. The shim takes 0..2 GiB (runtime low area, dispatch
directory, statics, stack and heap); the guest takes 2 GiB..4 GiB-64 KiB.
- **The access:** `GEX_P(a)` becomes `(char *)(uintptr_t)(uint32_t)a +
  0x80000000u`. clang folds the constant into the memarg offset (to verify
  on the unit C: needs an `inbounds` GEP on a 32-bit base). The wasm
  effective address is a 33-bit sum, so `a >= 0x7fff0000` traps past the
  memory's end. That is the bounds check, free. A displacement of up to
  0xffff rides along in the same memarg.
- **Isolation:** no guest address names a shim byte. Better than today,
  where every tier reaches the shim's statics.
- **What changes:**
  - gex-cgen: GEX_P and GA (providers), and the guest base goes into the
    unit key as a flavor (natively 0, unchanged).
  - gex-emit tier-1: the base in its memarg, for an NT i386 process only,
    and the guard drops for i386.
  - The shim: `guest_bytes()` starts at the base (GuestMem is already a
    slice, so the kernel's accesses follow), the memmap, code fetch for
    translation, and `NT_HEAP_BASE` and `--global-base` move below the
    base.
  - Every JS reader of guest pointers needs the base added:
    gl-bridge.mjs, wl-bridge.mjs, host.mjs, worker.mjs, gex.mjs, and
    web/src mesa-gl, gl, compositor-worker, dom-compositor.
  - Thread contexts and the dispatch directory stay low; they are runtime
    addresses, not guest ones. Linux processes and 64-bit guests keep
    base 0 and the guard.
- **Risks:** any host path that treats a guest pointer as a linear one
  silently reads the shim's memory. A debug mode with the base at
  0x4000_0000 (and the guest capped at 1 GiB) would make such a bug fault
  or show garbage early.
- **Estimate:** the emitter and header changes are small. The JS and shim
  audit is most of the work, plus the gates and Noita, Peggle and
  Half-Life in the page.

**Step 1, refined (design work while step 0 builds):**
- **Fold verified:** clang -O2 for wasm32 puts `(char *)(uint32_t)a +
  0x80000000u (+ disp)` entirely in the memarg (`i32.load offset=
  0x80000008`). No instruction is added per access.
- **The shim's link layout stays as it is.** Its statics and stack at
  0x0600_0000 are already below 2 GiB; only the NT heap moves, from
  `NT_HEAP_BASE` 0x8000_0000 up, to 0x0800_0000..0x8000_0000 (about
  1.9 GiB, against about 2 GiB now). The runtime's low area (thread
  contexts at 0x1000, the dispatch directory at 1 MiB) stays where it is,
  and is then outside the guest's reach. Today a guest null-pointer field
  writes into it, which is why `GUEST_FLOOR` exists.
- **One base per process,** chosen at boot: 0x8000_0000 for an NT i386
  process, 0 otherwise.
  - tier-1: a per-process emitter setting (`set_guard_limit` already is
    one). The base goes into the memarg, and the 2 GiB guard goes away.
  - Units and providers: a compile-time flavor in the key (`:bias80`,
    `GEX_GUEST_BASE` in gex_abi.h, used only by GEX_P and GA). This is
    the only gex-cgen change: two macros and a key suffix, called out
    for F.
  - The kernel: `guest_bytes()` starts at the base, so GuestMem follows,
    and its slice stays under `isize::MAX`.
  - JS: `Gex.guestBase` for every reader of a *guest* pointer. Addresses
    the shim hands over that are its own (rings, contexts, command
    buffers in its heap) are linear already and stay as they are.
- **The NT allocator:** top-down and zero-bits allocation cap at
  `ALLOC_HIGH` 0x7fff0000. But `Vm::is_free` (fixed-address
  NtAllocateVirtualMemory and NtMapViewOfSection) accepts any range up to
  `GUEST_LIMIT` 0xffff0000. So a fixed request at 2 GiB or above
  succeeds today, and in the page it lands in the shim's heap: a latent
  bug now, and a must-fix for step 1. Cap it at the process's user limit
  (0x7fff0000 for i386). The KUSER_SHARED_DATA, PEB and TEB addresses
  (0x7ffe0000, 0x7ffd....) are below it.
- **Memory:** the page's memory starts at 2 GiB plus the guest's initial
  size. V8 reserves the maximum and commits the initial size. Pages never
  touched cost no RSS, but check that Chrome's renderer limit counts RSS
  and not the commit (the first run's memwatch line).
- **Readers to audit** (sites that touch `memory.buffer` / DataView):
  gex.mjs 56, gl.js 15, worker.mjs 11, compositor-worker.js 10, gl-bridge
  7, mesa-gl 6, wl-bridge 4, dom-compositor 3, host 2. Most take
  addresses the shim computed. The guest ones are GL client memory
  (opengl32 client arrays, glTexImage pixels, glReadPixels out), wl SHM
  buffers, NT callback and exception frames (the ctx's SP and PC are
  guest values), and the call-out's stack reads.
- **Test** (the coordinator's condition): a tests/nt case that maps a
  buffer at the top of the allocatable range (fixed base near 0x7ff0_0000),
  fills it, and hands it to GL (glTexImage2D, glReadPixels) and to a wl
  SHM buffer, with run-browser.sh checking the pixels. With base 0 it
  passes trivially; with the bias, a reader that forgot the base reads
  the shim's heap and fails. Also a debug base (`GEX_NT_GUEST_BASE`,
  e.g. 0x4000_0000 with the guest capped at 1 GiB), so an error can't
  hide behind memory that happens to be zero.

- **Stateful providers** (R's merge/abi9-page, E's d3d20072) put their
  C data and a per-Worker stack in memory from the shim (`__memory_base`,
  `gex_provider_alloc`). Under the bias these stay linear and below the
  base, out of the guest's reach, which is fine. But crt_heap's dlmalloc
  hands *guest* pointers to the program: its arena must sit above the
  base, and malloc/free must convert (`p - base` out, `p + base` in),
  as must every provider that returns or takes a guest pointer. The same
  goes for the yield park's `addr` (`sync::YIELD_GEN`) and for
  `GA()`-style reads in all providers. Audit these with the JS readers.

**Rejected:**
- *Mask (`a & 0x7fffffff`):* one AND per access, but isolation by silent
  aliasing (a wild pointer corrupts guest memory instead of faulting).
  Fallback only.
- *Multi-memory (shim in its own memory):* Rust/LLVM cannot target it for
  the shim's code, and every host accessor of guest memory would change.
  Step 1 gets the same isolation inside one memory.

Order: step 0 first (it is the performance, and it is as safe as today),
then step 1 for isolation, after the coordinator agrees.

### Stream B state at the third reboot (2026-10-04, ~14:10)

- **merge/web9 @ this commit**:
  - merge/web9-fjit and merge/fjit (2c83dead);
  - R's merge/abi9-page (abi9, stateful providers in the page, carries
    off), via merge/web9-bias;
  - the browser yield park (R's patch) and page GEX_NT_YIELD /
    GEX_NT_DESKTOP;
  - the NT fixed-address cap (ce415836);
  - `gexc cgen-image --nt-trampolines32 | --providers`;
  - the unguarded mode bit (page `unguarded=1`, run-node
    GEX_CGEN_UNGUARDED=1);
  - the aot-share wait class (NOITA_MIN_AOT 97);
  - seed pin and unit precondition in browser-noita.mjs.

  run-browser.sh is 49/49 (plain) on web9-bias. Not yet on the final
  merge: web9's shim was rebuilt after the merge and installed, but its
  gate was not rerun. Do that first.
- **Not done:** the step-0 A/B (guarded vs unguarded) and the yield arms.
  - bnoita3 and bnoita3u (pre-rebase, carries on, *no icall hints*):
    abandoned.
  - The new sets: `target/bnoita4u` (unguarded) and `target/bnoita4g`
    (guarded). They use the staged layout (real copy, `--append`), have
    the cg14 icall hints copied into cgen/icall (noita.exe's key ends
    `-ie77f2dfea53061fa`) and providers built, and come from gexc
    `target/x86/release/gexc` of web9 (= scratchpad gexc-web9d).
  - Both were stopped mid noita.exe: bnoita4u had 991 objects, bnoita4g
    244 (of about 3300).
  - **bnoita4u's noita.exe failed twice** at about unit 925 (and
    bnoita3u's at about unit 1550), with the error cut off by `tail`. The
    guarded flavor of the same tree built fine (bnoita3). Find the
    failing unit first: run `gexc cgen-image noita.exe --pe-base 0x400000`
    with GEX_CGEN_CACHE=target/bnoita4u/cgen, *without* GEX_CGEN_GUARD,
    and keep the whole log. Objects are cached, so it gets back to the
    failure quickly.
- **Resume:**
  - Scripts are saved in `target/streamB-scripts/`: build-bunits4.sh
    GEXC u|g, run-noita-page.sh, run-pewasm-page.sh, abba.sh, wfps.sh,
    and the pewasm page's index.html with `?seed=`. Their copies also
    sit in scratchpad web9/.
  - Then: guarded vs unguarded (page flag `&unguarded=1`), each paired
    with pewasm, seed pinned, on 5,11. Then GEX_NT_YIELD=os against the
    default wake on the unguarded set.
  - Report the step-0 delta before step 1.
- **Compositor (the next lever after these):** per frame, from dg4's
  compositor line:
  - 12 queue submits (2.9 ms);
  - 60 render passes for 56 draws (0.7 ms);
  - 218 writeBuffer, 137 copyBufferToBuffer, 15 createBuffer, and
    writeTexture of about 840 KiB;
  - about 11 ms of gex_gl_submit per frame.

  The profile also shows `vsnprintf`/`printf_core` about 200 ms per 11 s
  under gex_wgpu_draw_vbo_inner (mesa-wink gex_wgpu_screen.c,
  `gex-cull` / `facing capture` fprintfs).
- **Disk:** /home is at 98% (45 GB free). bnoita2, bnoita3 and bnoita3u
  are several GB each and superseded. F read bnoita2 and bnoita3, so ask
  before deleting.

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
