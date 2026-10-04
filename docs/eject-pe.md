# gex-eject for Windows programs, on the §9 runtime

Stream E, 2026-10-03. This is the design; status is at the end. It extends
`docs/eject.md` (Linux programs) to PE programs on gex's NT layer
(`docs/nt-layer.md`), on the function-table runtime of `docs/merge-plan.md`
§9 (units, `gex.ftab`, export providers). The target is an ejected Noita
that replaces pewasm: a standalone project that builds and runs natively,
in Node and in the browser, at or above pewasm's world fps.

## 0. The constraint, and what it means

The user: "the default tier for eject does not ship a jit or interpreter at
all." In an ejected project's default runtime:

- **No guest code is translated at run time.** No tier-1, no function JIT,
  no region tier, no interpreter of guest instructions. Every guest
  function that runs is a wasm function compiled at build time: a unit
  function, a region entry (below), or a provider.
- **A gap fails at build time,** by image, address and reason. If one
  slips through anyway (the session never reached it and recovery missed
  it), the run stops at the first call, naming the address and image. It
  never falls back.
- **Code generated at run time cannot run.** Every generator is replaced.
  Noita's one generator is LuaJIT. Its lua51.dll is replaced with native
  Lua through an export provider, as pewasm did.
- **`--jit-fallback`** (off by default) stays, as an opt-in debugging aid.
  It is the only way the translator is reached.

**Judgement call, to confirm.** The Lua replacement is PUC Lua 5.1.5, which
interprets *Lua bytecode*. It is ordinary program code compiled AOT to
wasm, like any game's script VM. It never generates or interprets guest
x86. That is pewasm's design, and I read the constraint as being about
guest code.

"Ships no JIT" also has a binary meaning: the runtime executable contains no
translator. That is phase E7 (§7): a feature-gated runtime build, plus
wasmtime without Cranelift, loading precompiled `.cwasm`. Until E7 the
runtime is gexc running in an AOT-only mode, which enforces the
behaviour: no translator installed, no tier-1 module, and a loud abort on
any miss.

## 1. Recording a Windows session

`gexc run --record DIR prog.exe` already works for PE (d39b63e6). It names
every mapped image by file sha256 and link-time address (`profile::note_pe`),
and tier-1 blocks are counted per image. Added here:

**1. `Session.nt` (`NtSession`, from the earlier sketch):**
- `abi` (p32/p64);
- the prefix's guest path;
- the exe's guest path and DOS image path, the arguments, and the Windows
  environment;
- the DOS cwd;
- the `GEX_NT_*` settings in effect (`TIME_SCALE`, `INPUT`, `STRICT`, ...);
- `gl`;
- **`images`**: every image mapped, in mapping order, as (object id, guest
  path, base, preferred base, size).

The NT allocator is deterministic, but a multi-threaded program's
`LoadLibrary` order is not. So the eject does not rely on replaying the
order: it pins each image (§2.3).

**2. Files the NT layer opens.** NT file access goes through gex-kernel's
VFS directly, not through Linux `openat`, so today's recorder never sees
it. A hook in `nt::file` (like `set_image_mapped_hook`) reports every
`NtCreateFile`/`NtOpenFile`/`NtQueryAttributesFile`, with:
- the unix path it resolved to;
- read, write, create and directory;
- found or not.

These land in `Session.files` exactly as Linux opens do, so the eject's
rootfs, missing and outputs logic is reused unchanged.

**3. Non-AOT code the session ran.** Recording runs with units on (the
default mode). Anything that ran without a unit body ends up in the
profile in one of three ways:
- **tier-1 blocks**, already counted per image in `profile.json`;
- **fallback and JIT lookups** (`ftab::lookup` answered with index 1, or
  translated), which the recorder logs by address;
- **anonymous executable mappings** (code with no image).

So no new format is needed: the eject reads them back as the gap list
(§3.4).

**4. Providers in use** (name and fingerprint), and the `icall` profile
(already recorded) for the units' devirtualisation hints.

## 2. The project

```text
noita-project/
  gex-project.json     objects (every image), files, nt{}, providers      generated
  rootfs/              guest paths: the prefix's files and the game's     generated
                       files that the session read (hash-checked)
  session/             session.json, profile.json                         generated
  runtime/gexc         the runtime (E7: gex-rt, no translator)            generated
  runtime/web/         shim, gex.mjs, Mesa (mesa-wink `wink`)             generated
  gen/                 gex_abi.h, provider.h, gex_replace.h               generated
  build.sh run.sh run.mjs serve.mjs index.html                            generated
  user/
    eject.json         replacements, extra seeds, settings                yours
    src/*.c            replacement code                                   yours
    lib/lua51/         the Lua provider, copied in at first eject         yours
  build/
    units/             the unit cache: <unit key>/c.wasm + c.cwasm        built
    providers/         provider modules (+ .cwasm)                        built
    regions/           region entries per image (§3.4)                    built
    rootfs/            rootfs + user/files                                built
    build.json         what was built from what; the coverage report      built
```

### 2.1 Images: whole-image units, nothing cut

Every PE image the session mapped is built: noita.exe, the game's DLLs
(msvcr120, msvcp120, SDL2, fmod, fmodstudio, steam_api, lua51) and every
Wine DLL, at its recorded base. Each is a whole-image unit, the same
`gexc cgen-image --pe-base` build `nt_cgen` uses, with:
- every recovered function;
- the recorded icall hints;
- the providers' stubs (in the key);
- the session's extra seeds (§3.4).

There are no hot-set cuts. pewasm built all of Noita statically, and so
does this.

The units go into the project's own cache (`build/units`, which the
runtime uses as `GEX_CGEN_CACHE`), keyed exactly as `nt_cgen::unit_key`
keys them, so the runtime's existing `place()` finds them unchanged.
Wasmtime's compiled form (`c.cwasm`) is built at build time too
(`gex_cache::load_or_compile_at`), so a run compiles nothing. The NT
trampoline page's unit is built the same way.

**Reuse:** a unit is content-keyed, so `build.sh` takes an existing cache
as a read-only source (`GEX_EJECT_UNIT_CACHE=.../cgcache-cg9`) and copies
or links matching keys instead of rebuilding. That is how the first Noita
eject avoids a second hour-long build alongside stream F's.

### 2.2 Providers

The project lists the providers in effect (gex's built-in providers:
ConcRT, crt_mem, crt_math, debug_string). Each is built into
`build/providers` and bound at map time exactly as `nt_cgen::install_providers`
binds it. User replacements and the Lua provider are providers too (§4, §5).

### 2.3 Image placement: a pinned layout

The eject writes `build/rootfs/<prefix>/gex-layout` (the existing
`section::Layout` format) with **every** recorded image's base. Two small
NT-layer extensions make it binding:
- **Absolute paths** in a layout line, for images outside the prefix (the
  game's).
- **An exact layout** (`gex-layout 1 exact`): at boot the allocator
  reserves every layout range, so no allocation can take one before its
  image maps, and an image maps only at its layout base. A layout image
  that cannot get its base stops the run (`LAYOUT_MISSES` becomes fatal).
  So a unit keyed by base always applies.

A relocatable image is relocated to its pinned base. noita.exe has no
relocations and sits at 0x400000, as recorded.

### 2.4 The rootfs and the guest's paths

Under the Linux eject, the guest sees `build/rootfs` as `/`. A PE eject
does the same: guest paths stay what they were when recorded (the prefix's
path, and the game's directory as Z:\…). That keeps the DOS paths, the
environment block and the process parameters identical, so the NT
allocator replays the same addresses.

`gexc run` refuses `--sysroot` for PE today; supporting it means:
- resolving the prefix and the exe as guest paths under the sysroot;
- `nt_cgen`'s `is_wine` and `place` working from guest paths (they read
  the image from the mapped bytes, not the file).

The prefix contributes only the files the session read: the DLLs it
mapped, the NLS files, registry and the `gex-layout`, not all 465 MB.

## 3. The runtime without JIT or interpreter

### 3.1 AOT-only mode (E3; `RunOptions::aot_only`, set by `eject run`)

- **No translator.** `gex_kernel::ftab::set_translator` is never called.
- **No tier-1 module.** The NT run builds today's empty-image tier-1 module
  only for its `enter` export (the root of every guest loop: the thread
  loop, `nested_loop` for callbacks, `raise_nested`). In AOT-only mode the
  runtime module (`gex_cgen::runtime`) gets an `enter(ctx, pc)` export:
  store PC, then `root`. Every loop's `any_enter` finds it. Its imported
  dispatch tables are empty tables.
- **`gex.lookup` serves only installed entries:** units, region entries
  and providers. On a miss it stops the run:

  ```text
  gex-eject: no AOT code for 0x6a1234 (SDL2.dll+0x21234, in a function recovery did not
  find). The project was built from a session that never ran it. Record a session that
  reaches it and re-eject, or add it to "seeds" in user/eject.json, or run with
  --jit-fallback.
  ```

  The same message covers code in an anonymous mapping ("generated at run
  time; replace its generator") and a rewritten page ("patched at run
  time").
- **ftab index 1 (FALLBACK)** is a host function that does the same, naming
  `ctx::LAST_TARGET`. `call_out` and `dispatch_miss`, which a unit's leave
  path can still call, return "leave" and "no", so control reaches the
  root and then the lookup that fails loudly. Nothing translates.
- **Units are required for every image,** Wine's included
  (`Mode::RequireAll`). An image without a unit at its base stops the run
  at map time. `GEX_NT_CGEN_BUILD=0`: nothing is built at run time.
- **Statistics:** at exit, `lookups` misses must be 0, and the AOT share
  line must read 100% units+providers.

### 3.2 What still leaves a function, and lands where

Under §9 a unit function "leaves" (returns 0 with PC set) on:
- a failed return check;
- longjmp;
- a call continuation after a trap-out;
- an exception resume (`NtContinue`);
- a patched page.

The root then looks the PC up. When the PC is a function entry, it is
served. When it is an **interior address**, a unit has no entry for it.
In today's runs those addresses went to tier-1 ("interior" in
aot-coverage). R's numbers on cg9: about 10 trap-out resumes per run, all
during loading, plus setjmp continuations. Without tier-1 each needs AOT
code:

### 3.3 Region entries (build time)

A region entry is §9.2's region tier done ahead of time. It is a function
that starts at an interior address and runs that function's remaining
code under the same `F_body` contract, until the function returns. The
original function is untouched: the region is a second copy of its tail.
- **Built from:** recovery from that start (`func::recover_one`, as the
  function JIT does), lowered by gex-cgen to C, compiled with the unit
  flags, and linked into `build/regions/<image>.wasm`. That module
  installs into the ftab after the image's unit, like a unit with extra
  entries (gex-cgen `standalone` already emits one function as a unit).
- **Its sources:**
  - the recorded gap list (§3.4);
  - every setjmp-like continuation (calls to `_setjmp`, `_setjmp3`,
    `__wine_setjmpex`, `setjmp`: the instruction after the call);
  - the C++/SEH continuations recovery marks as `LandingPad`;
  - every call continuation in a function that has an SEH frame. That is
    the conservative set for exception resumes and can be cut later by
    profile.

  **As built** (`nt_cgen::prepare_regions`; a module per image at
  `<cache>/regions/<unit key>/`, outside the unit's key, installed after the
  unit):
  - the recorded entries the unit lacks (resumes, leaves), the recorded
    indirect-call targets it lacks, and the user's seeds;
  - **every call continuation of a function with a recorded continuation.**
    Once one resume discards a function's frame, the rest of that function
    runs from regions, whose frames the next resume discards in turn. The
    recording saw only the first such entry, because tier-1 ran the rest.
    The function is the unit's nearest entry at or below the PC (seh: 2
    recorded, 52 built; cxx: 9 recorded, 38);
  - **closed under direct calls and tail calls**: a callee the unit lacks
    (code that recovery reaches only from a landing pad) is a region
    function too (cxx: 9 more). In the recording, tier-1 called these
    directly, so they never reached a lookup and were never logged.

  One function per region PC: gex-cgen has no multi-entry functions, so a
  function with n continuations costs about n/2 copies of itself.

### 3.4 The coverage report (build fails loudly)

`build.sh` lists every address the session ran without AOT code (§1.3),
by image:

| class | what the build does |
| --- | --- |
| an entry recovery missed (a function start) | adds it as a seed (`Source::Profile`) to the image's recovery, so the whole-image unit has it |
| an interior address | builds a region entry |
| in an image whose exports a provider replaces | nothing, when the address is a provided export; anything else fails ("lua51.dll+0x1234 runs although every export is provided") |
| an anonymous mapping (generated code) | fails, naming the mapping and its creator (the image that called `NtAllocateVirtualMemory(PAGE_EXECUTE_*)` or `NtProtectVirtualMemory`) |
| a page patched at run time | fails, naming the patching code |

`user/eject.json` `"seeds": ["SDL2.dll+0x21234", ...]` adds more by hand.
The report goes into `build/build.json` and on stdout. A build with a
non-empty failure list exits non-zero.

### 3.5 Threads, callbacks, exceptions, signals

These are unchanged from today's NT runtime:
- every thread's store instantiates every installed module;
- user-mode callbacks run a nested root (`nested_loop`);
- exceptions dispatch nested (`raise_nested`) or by trap-out, and land at
  function entries or region entries;
- safepoints poll in units and the root.

## 4. Replacements on §9: export providers, direct wasm calls

Today an eject replacement is `Intrinsic::Replace(id)` at the bound address:
a host call (`sysreg_write`) per call, with the host calling the user
module. On §9:

- **A replacement is a provider.** Its function is bound in the ftab at the
  replaced address with the same machinery as gex's built-in providers
  (`publish_pairs`). Every caller reaches it with one `call_indirect`: IAT
  calls, GetProcAddress pointers, the library's own calls, units, regions.
- **The image's unit is built with that address as a stub** (in the key),
  so the original code is never compiled, let alone run.
- **Adapters, generated per binding** with wasm-encoder: a small module
  that imports the user module's export and exposes an `F_body`. In order:
  1. read the arguments by the binding's convention;
  2. `call` the user function directly (cross-instance, no host);
  3. write the result;
  4. pop the return address (plus N for stdcall);
  5. set PC;
  6. return `ret == ra`.

  These are the "descriptor plus generated thunk" of §9.9.1, which the
  built-in providers skipped by being hand-written.

  | `abi` | arguments | result |
  | --- | --- | --- |
  | `c` (x86-64 SysV, AArch64) | integer and FP argument registers, as today | rax/x0, xmm0/v0 |
  | `cdecl`, `stdcall`, `thiscall` (i386) | 4-byte stack slots after the return address (i64/f64 take two; `this` in ECX); stdcall/thiscall pop the sum | EAX, EDX:EAX for i64, ST0 for f32/f64 |
  | `ctx` | the user function gets `ctx`; it reads and writes registers itself | the adapter only returns |
  | `body` | the user function *is* an `F_body` (what `decompile` writes, and what providers are) | none |

- **i386 objects** are no longer refused: binding by `symbol` uses the PE
  export table, `offset` is an RVA in the image, and `import` is the IAT
  slot's users. The IAT itself needs nothing, since the export's address
  is bound. `library` binds every export.
- **The user module may have state** (data, stack, heap): §5.1's stateful
  module rules apply. The `[0xF0000000, …)` region from docs/eject.md stays
  for Linux guests. For NT processes the region comes from the NT allocator
  (§5.1), because the browser shim owns 0x8000_0000 and up.

The Linux eject moves to the same mechanism (`Intrinsic::Replace` and the
host round trip go away). Its tests (ack, strtol, getenv, libsq) are the
gate.

## 5. Lua as a provider

### 5.1 Stateful providers (the mechanism Lua needs)

Built-in providers carry no data and no shadow stack. Lua needs all three
of the following, and so do user replacements of any size:

- **Data.** The module is linked `-shared` (PIC), with its data placed at
  `__memory_base`.
  - When its library maps, the runtime reserves a region for the data,
    the heap and the stacks from the **NT allocator** (deterministic,
    below 2 GiB, guest-visible), named `[provider lua51]`.
  - It writes the data segments into that region **once**, then calls
    `__wasm_apply_data_relocs` once.
  - Every store instantiates the module with its data segments stripped
    (the build strips them), so a new thread never re-initialises state.
- **Stack.** `__stack_pointer` is imported per instantiation, so each
  thread gets its own global, pointing at that thread's stack in the
  region (1 MiB each, taken on first use).
- **Heap.** dlmalloc (as pewasm, `dlmalloc32.inc`), with `MORECORE` growing
  inside the reserved region. The region is reserved large and committed
  as it grows (NT `MEM_RESERVE` then `MEM_COMMIT`). Lua objects are guest
  addresses, so the guest reads the strings Lua returns directly.
- **Non-local exits (Lua errors).** wasmtime 27 has no wasm exception
  handling, so setjmp/longjmp go through the host:
  - `gex.try(fn, arg) -> i32` calls ftab[fn](arg) from the host and returns
    0, or the code a `gex.throw(code)` inside it trapped with. Wasm frames
    in between (Lua's C frames, and guest unit frames when a guest
    `lua_CFunction` raised the error) are discarded.
  - The Lua side keeps pewasm's `nl_guest_save/restore`: guest ESP, the
    TEB's SEH list and the provider's own per-thread call depth.
  - In the browser these are JS imports (JS `try`/`throw` through wasm
    frames, as emscripten's sjlj does without wasm EH).
  - One host round trip per `lua_pcall` / `lua_resume`.
  - When wasmtime gets wasm EH (an upgrade, R's call), these become
    `try_table`/`throw` with no ABI change for the Lua code.
- **Calling the guest.** `gp_call(ctx, depth, fn, args...)`, generalising
  `gp_call0`: push the cdecl arguments and a return address, call
  `ftab[probe(fn)]` with that predicted return, and check that it returned
  1. A `lua_CFunction` from the guest is stored tagged, as pewasm stores
    it (bit 31), and called this way. The current guest `ctx` per thread is
    kept in a table indexed by the thread's context slot.

### 5.2 The Lua provider

`user/lib/lua51/` (copied in at the first eject of a session that loaded a
LuaJIT `lua51.dll`, recognised by its `luaJIT_setmode` export) holds:
- `port/lua/*` (PUC Lua 5.1.5, pewasm's build configuration);
- `noita_lua.c` and `noita_lua_libs.c` ported from pewasm's `HOSTLIB`
  macros to `GEX_PROVIDE`/`F_body` form. `pe_arg32` becomes `gp_arg`,
  `MEMR32`/`MEMW32` become `GW`, and `h2g`/`g2h` become the identity,
  since a guest address is a linear-memory offset.
- the `bit`, `jit` and `ffi` stubs as pewasm has them.

`user/eject.json` binds it:

```json
{ "library": "lua51.dll", "sha256": "sha256:…", "provider": "user/lib/lua51", "abi": "body" }
```

`luaJIT_*` exports that Noita does not import are bound to a placeholder
that stops the run naming them. lua51.dll stays mapped (Noita imports it,
and its DllMain runs from its unit), but none of its exports' code runs.
With every export provided, the build's coverage check (§3.4) proves that
no LuaJIT code is reached, mcode included.

**Not copied:** pewasm's per-game patches (`noita_overrides.c`); SDL,
FMOD and Steam, which stay the guest's DLLs on gex's NT layer.

## 6. The web build

- `--web-shim`: the browser shim (`shims/browser`, shared-memory NT build)
  and `gex.mjs`, vendored into `runtime/web/`, as for the Linux eject.
- **Units:** `build/units/<key>/c.wasm` plus `c.entries`, loaded by
  `gex.mjs`'s `loadUnit`: compiled once and shared with every Worker
  (docs/browser-units-plan.md). It is the unguarded flavour only once the
  browser NT layer's guard limit equals the memory size; until then, the
  browser flavour, which is another unit key and so another build.
- **AOT-only** in the shim: the same three rules as natively (no
  translator, a loud `lookup` miss, index 1 aborts). The shim's tier-1
  code is compiled out in E7.
- **GL:** opengl32's unixlib onto `MesaGlDevice`, Mesa built from
  **mesa-wink's `wink` branch only** (memory: mesa-wink-branch), vendored
  as `runtime/web/mesa/`. No virgl, no MobileGL.
- **Files:** `build/rootfs` as the page's VFS bundle, with the prefix at
  its recorded guest path.
- **Depends on stream B** (browser §9: per-Worker ftab, units and providers
  in the shim). The eject's web part waits for B's merge, then adds only
  vendoring and the launchers.

## 7. Phases

| | what | gate |
| --- | --- | --- |
| E1 | record: `NtSession`, the NT file hook, logged lookups | session.json for tests/nt hello and Noita round-trips |
| E2 | eject + build for PE: rootfs, exact layout, units (with cache reuse), providers, `.cwasm`, the coverage report | build of tests/nt programs and the GL demo |
| E3 | AOT-only runtime: `enter`→root, lookup-miss and fallback aborts, all images require units | tests/nt programs and the GL demo run ejected, natively, with 0 translations |
| E4 | stateful providers; replacements as providers with generated adapters; i386 ABIs | Linux eject tests on the new path; an i386 replacement test in tests/nt |
| E5 | Lua provider | tests/nt Lua host program (pcall, errors through guest frames, callbacks), then Noita |
| E6 | region entries; Noita to the menu, then the world, natively; fps against pewasm | 4x clock, null display, paired runs |
| E7 | runtime without translators: a feature-gated shims/native + a `gex-rt` binary, wasmtime without Cranelift loading `.cwasm`; the browser shim likewise | `nm`/size check that gex-lift, gex-emit and gex-fjit are absent |
| E8 | web build of the ejected Noita (after B); fps in Node and Chromium | same recipe in the browser |

## 8. Gaps and risks

- **Interior entries** (§3.2–3.3) are the main correctness risk. A resume
  at an address no region covers stops the run. The conservative region set
  (every call continuation in an SEH-framed function) may be large; it is
  measured on Noita before it is trimmed.
- **Recovery misses** in Wine and the game's DLLs (SDL2's dynapi was one,
  fixed by 56988e74). Each is a seed added by the coverage report; a
  session that never reaches the code finds it only at run time, loudly.
- **sjlj through the host** costs a host round trip per pcall. If Noita
  pcalls per entity per frame, measure it: the fix is wasm EH (a wasmtime
  upgrade), which is R's call.
- **Thread-start determinism.** The exact layout pins images. Heap
  addresses can still differ between runs, which does not matter for AOT
  code.
- **Patched code:** Steam's overlay and anti-cheat style hooks write code.
  Noita's steam_api under gex does neither, and the coverage report would
  name it.
- **Other code generators:** none known in Noita besides LuaJIT. The
  coverage report's anonymous-mapping class finds any others.
- **Size:** about 5 GB of unit cache for Noita and the prefix
  (cgcache-cg9), mostly `.cwasm`. The project ships `c.wasm` plus
  `.cwasm`; a web-only project ships only `c.wasm`.
- **Areas of other streams:**
  - shims/native's run loop and ftab (R): `aot_only`, `enter` in the
    runtime module, `gex.try`/`gex.throw`, stateful provider install;
  - gex-kernel's NT (layout exact mode, file hook): small and general;
  - gex-cgen (F): region entries via `standalone`, `Source::Profile`
    seeds, the PIC data-stripping link for stateful providers.

  Each lands as a small, called-out commit on merge/eject9.

## Status

- 2026-10-03: design (this file).
- E1-E3 built (merge/eject9): recording, eject, build (units reused by key
  from `GEX_EJECT_UNIT_CACHE`, `.cwasm` at build time, exact layout,
  recovery seeded with recorded indirect-call targets), and the AOT-only
  runtime (`gex_native::aot_only`). tests/nt x86 ejected and run with no
  run-time translation, same stdout and exit as a normal run: hello_args,
  sync, mtx, crtmem, tls, icall, user_win, gdi, exitproc, input, display,
  window, lowmem, filemap, and the GL demo glcompat.
  - Stop loudly, by design: jitcode, detour, pagespan (code written or
    patched at run time).
  - cxx, seh and apc resume at interior addresses (a catch continuation,
    the instruction after a faulting call, the trampoline page after an
    alertable wait): covered since by region entries (below).
- E4/E5 in part: stateful providers (`gex_native::libprov`) and the Lua
  provider (`crates/gex-eject/libs/lua51`, 275 KiB). A LuaJIT host
  program (states, chunks, guest C functions, errors from them caught by
  Lua and by the host, nested calls, coroutines, GC, bit, string.format)
  ejected with it gives LuaJIT's output except two cosmetic differences:
  %f rounds exact ties to even (MSVC rounds them away), and a long chunk
  name is abbreviated PUC Lua's way.
- **Noita, ejected, runs to the world with no run-time translation**
  (2026-10-03). Recorded with F's cg10 units, ejected (38 images,
  58 files, 96 MB), built reusing 33 units by key (msvcp120, fmod,
  fmodstudio, steam_api and SDL2 built at their recorded bases), Lua on
  the PUC provider. The run's GEX_NT_STATS line: no tier-1 blocks, no
  translations; world-up 24-28 s.
  - Lua protected calls (`provider_try`): about 1.4 per world frame
    (13,567 over 8,404 frames), and no throws.
  - World fps, 4x clock, null display, cores 1 and 7, paired
    (load average about 15):

    | run | world fps |
    | --- | --- |
    | pewasm `out/noita/build` (bench.cjs, 60 s after 30 s) | 41.98, then 38.73 |
    | gex eject, 60 s windows from world-up | 26.2, 25.9, 27.4, 29.6, 30.4 |
  - What it took besides the above: the native unit flavor for eject
    (the guarded flavor matched no cache); recorded unit-less entries as
    recovery seeds (5 outside lua51); providers exempt from patched-page
    retirement (Noita's mod sandbox rewrites ~120 lua51 entry points, as
    pewasm also ignores).
- **Fixed seed (GEX_NT_FIXED_TIME=1767225600, PEWASM_FIXED_TIME's value),
  same units, cores 1 and 7, GEX_PROF per thread:**

  | run | world fps, 60 s windows from world-up |
  | --- | --- |
  | gex eject (AOT-only, gexc) | 38.0, 43.8, 42.4, 36.6, 44.8, 42.7 |
  | gex `--cgen`, same units (tier-1 behind them, LuaJIT) | 34.5, 38.5, 44.3, 45.6, 44.6, 44.6 |
  | pewasm bench.cjs | 68.51 |

  The eject runtime adds nothing over normal gex: whole-process and
  main-thread profiles match within noise (noita.exe 55.9/53.9%, host
  futex/syscalls 10.6/8.1%, sched_yield 5.2/5.6%, GL 4.4/5.5%, NT layer
  3.1/4.2%; Lua 1.1% on PUC Lua against 0.6% on LuaJIT). The main thread
  is about 44% busy, a quarter of it in sched_yield, futexes, GL and the
  NT layer. The gap to pewasm is gex's, not the eject's.
- **E7: gex-rt** (`crates/gex-rt`): the runtime with no translator and no
  wasm compiler (see the commit for what is linked). tests/nt hello_args,
  sync, apc, mtx and glcompat run on it (glcompat's "a query's result"
  check is flaky, 2/5 on gex-rt and on gexc alike). Noita on gex-rt, fixed
  seed, cores 1 and 7, a paired window (load average about 17):

  | run | world fps |
  | --- | --- |
  | gex-rt | 38.8, 45.2, 55.9, 52.5, 50.8; then 50.6, 51.1, 54.9, 54.9, 54.6 |
  | gexc AOT-only | 61.2, 53.2, 49.2, 51.4, 52.9 |
  | pewasm, between them | 44.70 |

  Under this load pewasm's number moves 39-69 between runs; only paired
  windows compare.
- apc passes ejected with R's nested APCs (fb7dfd52).
- **Region entries (§3.3), 2026-10-04:** `nt_cgen::prepare_regions`
  recovers a function entered at each PC (`func::recover_one`, with the
  unit's entries as strong starts) and lowers it with gex-cgen's existing
  write_recovery and build_c_module (no gex-cgen change) into
  `<cache>/regions/<unit key>/`; `install_regions` publishes it after the
  unit. The units stay F's, unseeded. The set is the recorded entries,
  plus every call continuation of their functions, closed under direct
  calls (see §3.3). An AOT-only branch to non-executable memory now raises
  the guest's access violation instead of stopping. **seh, seh traps, cxx
  and apc pass ejected on gex-rt**, and so does the rest of tests/nt x86
  (hello_args, sync, mtx, crtmem, tls, icall, user_win, gdi, exitproc,
  input, display, window, lowmem, filemap, glcompat). jitcode, detour and
  pagespan stop loudly, as designed. cxx's earlier kernelbase clang failure
  was a SIGTERM from the session ending, not a build error.
- Found: a unit of Noita's lua51.dll built at a rebased address (0xb70000)
  lowers the import thunk `jmp [IsProcessorFeaturePresent]` at +0x4e89a to
  `gex_illegal`, so its DllMain faults (also with --jit-fallback). The Lua
  provider replaces DllMain, so the eject does not depend on it.
