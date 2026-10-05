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

## 6a. Link groups for the ejected page (design, 2026-10-04)

merge-plan §9.6 says an ejected build is one link group of everything,
which is what pewasm built: noita.exe, msvcr120 and msvcp120 in one module.
This is the design for the first group, measured in the page against
pewasm's page (the only performance target).

### What a group buys

Today each image is its own unit. A call to another image is
`call dword [IAT slot]`, an indirect call. In a unit that becomes a load of
the slot, a probe of the function table (`gex_ftab_idx`) and a
`call_indirect` (V8 checks the signature), unless an indirect-call hint
(`icall_hints`) names a callee in the same image. Nothing inlines across
images.

noita.exe's IAT call sites (`call [IAT]`, plus `jmp [IAT]` thunks):

| target | sites | the hottest |
| --- | --- | --- |
| msvcr120.dll | 18,401 (+46) | `operator delete` 12,360, `operator new` 3,722, memmove 705, `type_info::name` 682, free 221, malloc 132 |
| msvcp120.dll | 4,568 (+161) | `_Xbad_alloc` 1,768, `_Xlength_error` 928, iostream destructors |
| lua51.dll (the Lua provider) | 4,047 (+6) | lua_checkstack 1,070, lua_gettop 467, lua_tonumber 356 |
| kernel32, SDL2, FMOD, Steam, ... | about 500 | QueryPerformanceCounter 173 |

The page profile (§ the page against pewasm's page) has msvcr120 at 14.5%
of the main thread's unit time and ntdll at 8.8% (RtlAllocateHeap is the
top of it), under `new`/`delete` -> malloc/free -> HeapAlloc. pewasm calls
all of these directly, and its host CRT's malloc is wasm.

### The group

- **Members:** images that an ejected project maps at known bases. Every
  base is known in an ejected project (the exact layout pins them), so any
  set works. The first group is noita.exe + msvcr120 + msvcp120 (pewasm's
  set). The project lists it in user/eject.json:
  `"link_groups": [["noita.exe", "msvcr120.dll", "msvcp120.dll"]]`.
- **Recovery:** each member's own recovery, unchanged and cached as now.
  The group's emission reads all of them and merges entries, summaries,
  trap-free and guard-free sets, and the touched maps. Function names are
  `f_<guest address>`, unique across members because the bases are fixed.
- **Calls between members:**
  - A direct call to a member's entry (a thunk's `jmp` target, a known
    callee) becomes a direct C call. `known` is the union of the members'
    entries, so the existing `Callee::Direct(t) if known.contains(t)` path
    and its page checks apply unchanged.
  - IAT calls: at build time, each import of each member is resolved
    against the exporting member's export table at its recorded base,
    forwarders followed, as Wine's loader resolves it. This gives
    `IAT slot -> target`. A `call [slot]` site then gets a build-time hint
    `ret_pc -> target`, through the existing devirtualised-call path:
    `ft = load(slot); if (ft == K) f_K(...) else ftab(ft)`. The compare
    keeps it correct when the program or a mod rewrites an IAT entry (a
    hook): a mismatch goes through the table, as today. Profile hints
    (`icall/`) still apply to the other indirect calls, now across members.
  - Leaf copies (`func::is_small_leaf`) work across members too: msvcr120's
    small leaves are copied into noita.exe's units.
  - An IAT target that a provider serves (crt_mem's memmove, crt_heap's
    malloc/free, concrt) is a stub address with no unit entry. In G2 the
    provider's C is compiled into the group as objects, and the hint calls
    the provider's function directly (`gexp_<export>`).
- **Link:** one `wasm-ld` over every member's objects (plus the providers'
  in G2) into `<cache>/groups/<group key>/c.wasm`, with `c.entries` and
  `gex.funcs` as for a unit. The element segment holds every member's
  entries. No data segments, as for units.
- **Key:** a hash of `group v1`, the members' unit keys (identity, base,
  providers, icall hints) in a fixed order, and the resolved IAT map.
  The objects cache stays keyed by C text: units with no cross-member calls
  are reused, the rest recompile. For noita.exe that is most units, so
  about the cost of one noita.exe build (about 2 h at nice 19, 4 jobs; its
  link peaks near 10 GB, per F).
- **Region entries** stay per member (their own modules). They call into
  the group through the table as they call into units today.
- **Install (browser shim and native host alike):** a group is wanted once
  all its members are placed (mapped at their expected bases and passing
  the byte check). Then the group module is published in place of the
  members' units, and the members are marked as covered so their own units
  are not fetched. Wine maps all of an exe's static imports before running
  any of their code (DllMain included), so the group is in before any
  member runs. If a member is missing or elsewhere, each image uses its own
  unit: an ejected project still builds the per-image units, which the
  group's objects mostly share, and the build fails if the layout cannot
  hold.
- **ThinLTO (G3):** units compile `-flto=thin` to bitcode objects (cached
  as now), and wasm-ld runs ThinLTO across the group (`--thinlto-jobs`),
  with the code generator flags the units use today
  (`-mllvm -switch-to-lookup=false`, no shadow-stack users: the splice
  check runs on the linked module as now). The page check stays the first
  statement of every body, so an inlined body still leaves on a patched
  page; a test asserts LTO keeps it. Measured separately from G1, since
  its build cost is the large one.
- **Lua (G4):** the stateful Lua provider in the group, which is what
  pewasm's native Lua was: 4,047 call sites made direct. Its data base
  becomes the group module's `__memory_base` (units have no data of their
  own), and its stack global the group's `__stack_pointer`.

### Runtime-specific providers (to settle at the integ merge)

R's crt_heap and nt_qpc are `native` (bound only when the native,
unguarded flavour is built or run). The ejected web build now *is* the
native unguarded units, and a provider's stubs are part of its image's
unit key. So either:
- the browser binds them too, which needs `gex.nt_qpc` (a clock read, which
  costs about a microsecond in a Worker, so likely a shared-memory clock
  word) and crt_heap's `provider_alloc_top` in the shim (the stateful
  mechanism is there); or
- the web build keys msvcr120 and ntdll without them, which is a second
  build of those two images.

The first is better for the page: `new`/`delete` on dlmalloc in wasm is
what pewasm runs.

### Phases and measurement

- **G1:** members' recoveries merged, IAT hints, one link, no LTO, installed
  in the shim and in gex-rt. Correctness: tests/nt with a group of the exe
  and its CRT (ucrtbase), and Noita AOT-only in the page. Measure: the page
  ABBA against pewasm, seed pinned, wake on.
- **G2:** stateless and stateful providers compiled into the group (crt_mem,
  crt_math, concrt, crt_heap), with direct calls to them.
- **G3:** ThinLTO across the group.
- **G4:** the Lua provider in the group.

Each phase gets its own page ABBA, and stays only if it gains.

### As built (G1-G3)

- **G1:** `nt_cgen::prepare_group`, `gex_cgen::image::build_group_module`,
  the native install (`install_placed`: members held, then the group), the
  browser install (gex.mjs `#cgenGroup`, shim `gex_cgen_hold` /
  `gex_cgen_group`: `groups/<key>` published as a unit with the members'
  pages, then each member's providers and region entries), user/eject.json
  `link_groups`. cxx.exe + ucrtbase on gex-rt: 24 IAT sites direct, same
  output as the recording.
- **G2** (`GEX_GROUP_PROVIDERS=1` at build): each member's stateless
  providers compiled into the group as translation units of their own
  (`provider::group_source`: no `GEX_PROVIDE` exports, a `gexg_<provider>_<fn>`
  per bound function), the export addresses served by bodies of the group
  that tail-call them (`FnUnit::add_provider_body`), and IAT calls to them
  hinted. Stateful providers (crt_heap, Lua) stay modules of their own.
- **G3** (`GEX_GROUP_LTO=1` at build): bitcode objects (`-flto=thin`, its own
  object keys) and `wasm-ld --lto-O2 --thinlto-jobs`, with
  `-switch-to-lookup=false` at link time.
- Both switches are in the group's key.

### Scoreboard: the ejected page against pewasm's page

World fps from world-up+90 s to the end of a 330 s run, world seed pinned
(1767225600) on both, 4x clock, hardware WebGPU in headless Chromium,
cores 5,11 (builds on 0,2,6,8 throughout). pewasm is `port/browser.mjs`
on stream B's build. Means of the legs; ratio against the pewasm legs of
the same sequence.

| date | change | ejected | pewasm | ratio |
| --- | --- | --- | --- | --- |
| 10-04 | guarded browser units (`p-noita12w`) | 39.2, 37.8 | 55.6, 59.5 | 0.67 |
| 10-04 | unguarded cg14 units + yield `os` (`p-noita14y`) | 47.2, 51.0 | 58.8, 56.7 | 0.85 |
| 10-04 | + wake-on-idle yield (hand port) | 62.7, 66.1 | 58.8, 56.7 | 1.11 |
| 10-04 | integ merge: nt_qpc, crt_heap, concrt provider kick (`p-noita15`, G0) | 67.7, 66.3 | 58.8, 59.1 | 1.14 |
| 10-04 | G1: noita.exe + msvcr120 + msvcp120 linked as one group | 67.3, 62.6 | 58.8, 59.1 | 1.10 (not kept) |
| 10-04 | quiet window (load 5-9), 9 legs: G0 again (`p-noita15g0`) | 69.1, 69.9 | 52.3, 61.3, 57.3 | 1.22 |
| 10-04 | + Mesa batched clears (`p-noita15m`) | 73.9, 73.7 | same | 1.29 |
| 10-04 | + msvcp120_thrd, YIELD_PARKED kick (`p-noita16g0m`) | 74.5, 74.9 | same | 1.31 |

Notes:
- G1 is installed and correct on both runtimes (members held, the group
  placed in 1.7 s, 111,811 entries, 149 MiB), but does not gain: the
  devirtualised IAT calls are a compare and a direct call instead of a probe
  and a `call_indirect`, and without inlining that is not where the time is.
  `link_groups` stays opt-in.
- In the G0 page, Noita's main thread (ctx 0x1000) spends about 2.0 s of
  every 10 s in NT calls: NtDuplicateObject and NtClose at about 4,900 a
  second each (about 20 us a call in the page), all from msvcp120's
  `_Thrd_current` (native `GEX_NT_SYSCALL_SITES=1`). It duplicates the
  current thread's handle, closes it at once, and returns the closed value
  with the id. The `msvcp120_thrd` provider returns {0, id} without either.
- The yield park is a real `memory.atomic.wait32`. Per 10 s (all threads):
  about 17,700 parks woken by a kick (2.7 s in all), 5,900 timed out
  (0.7 s); kernel kicks about 25, so the ConcRT provider's kicks do the
  waking. guest_yield's time in a V8 profile is blocked time, not spin.
- **Every edit to crates/gex-cgen re-keys every unit**
  (`GEX_CGEN_UNIT_INPUTS` hashes all of its .rs and .h). Objects are cached
  by C text, so a re-key costs emission and links (about 15 min for
  noita.exe), but the native build and the shim must come from the same
  gex-cgen tree, or the page misses units by key.

### State at the move (2026-10-04, 21:25)

- Branch merge/eject9 (gex `master` is trunk now; merge `master` for
  updates). Everything committed; tests/nt eject gate after both main merges:
  all 16 cases ok on gex-rt and the web build under Node, glcompat ok on
  gex-rt (its web run needs the page: no GL device under Node, as before).
- Best page configuration (1.31x pewasm, 9-leg sequence above):
  `scratchpad/eject9/p-noita16g0m` = p-noita16's units without the group,
  plus the batched-clear Mesa. Not kept: G1 (`link_groups`).
- **The Mesa batched-clear change is not pushed and not in gex.** It lives
  in `scratchpad/eject9/gexroot/third_party/mesa` (a plain repo made from
  wink 9016782's tree, commit dbf6668), and as
  `scratchpad/eject9/mesa-patches/0001-gex_wgpu-clears-recorded-into-the-pending-batch-not-.patch`.
  Built with `scratchpad/eject9/build-mesa.sh` (eject9's
  web/build-mesa-wasm.sh with GEX_MESA_SRC, the replay target only) into
  `scratchpad/eject9/mesa-out`. Pushing it to mesa-wink `wink` needs the
  user's approval (the coordinator asks).
- Resume:
  1. G3 (ThinLTO group): `p-noita16lto` was stopped mid-build (the
     per-image units reuse p-noita16's by key; the group recompiles every
     object as bitcode). Rerun its build.sh with `GEX_GROUP_LTO=1
     GEX_EJECT_UNIT_CACHE=<p-noita16>/build/units`, then the page sequence
     (`scratchpad/eject9/legs.sh N pew A B ... pew`, cores of the bench box).
     G2 (`GEX_GROUP_PROVIDERS=1`) is built in code but not measured.
  2. Compositor, next: Noita switches FBO almost every draw, so draw_vbo
     begins a render pass per draw (15 of 84 us per draw,
     `?mesaenv=GEX_WGPU_TIMING=1`); views (13 us) and setters (20 us) next.
  3. Wanted from U's merge (memory64 providers): rebase libprov's
     per-process relocate_once (29a2c2a1) and eject's cwasm_path naming onto
     it.
- Scripts: `scratchpad/eject9/{legs.sh, abba3.sh, page-run.mjs, ntweb.sh,
  ntweb-rest.sh, ntgroup.sh, run-ejected.sh, bshim.sh}`; projects
  `p-noita15` (G1 group), `p-noita15g0`, `p-noita15m`, `p-noita16` (G1 +
  thrd), `p-noita16g0m`, `p-noita16lto` (partial).

### State on gooner (2026-10-04, late; the pewasm-replacement block)

First block on gooner (RESUME-ON-GOONER.md "E's first block"), before G3
and the compositor. Paths: project root `~/gex-sandbox`, scratch
`~/gex-sandbox/scratch/eject9` (`$S` below).

- Step 0, rebuilt on gooner: `$S/build-native.sh` (gexc in target/x86,
  gex-rt in target/x86-rt), `$S/bshim-gooner.sh` (shim, kernel, seed into
  web/public/gex), Mesa: the batched-clear build copied from worldmachine
  into `$S/mesa-out` (source with the patch committed, dbf66687, in
  `$S/gexroot/third_party/mesa`; `$S/build-mesa.sh` rebuilds it), then
  copied to web/public/gex; page `web/build-eject-page.sh $S/page17`
  (needs `npm ci` in web/ once). `target/noita-prefix` = a copy of
  `~/.cache/gex/wine-11.15` (the recording's DLL hashes match).
- Project `$S/p-noita17`: `gexc eject noita/rec --out p-noita17 --web-shim
  web/public/gex --web-page page17` (GEX_RT set), user/web-entries.txt from
  p-noita16, seed pinned in user/files (pin.sh mod/save), no link groups
  (G0), cg14 icall hints copied into build/units/icall, `JOBS=4 nice -n 19
  ./build.sh` (log `$S/logs/build17.log`). After a page rebuild:
  `rsync -a --delete $S/page17/ $S/p-noita17/runtime/web/page/`.
- Checklist findings so far:
  - **Audio: neither side has any.** FMOD's x86 DLL fails
    `System::init` with FMOD_ERR_OUTPUT_INIT (51; Noita's logger.txt: "Error
    initializing output device"): gex has no WASAPI device (mmdevapi is
    never loaded), natively or in the page, so no bank is ever opened.
    pewasm binds fmod/fmodstudio to `port/src/noita_fmod_silent.c` (an
    interim binding with no output) and ships no banks. Real audio is a new
    feature (an mmdevapi/WASAPI driver onto WebAudio, plus 1.2 GB of banks
    to stream), for the coordinator to decide.
  - **Saves did not persist** across page loads (the page's filesystem is
    the global kernel's tmpfs). Added: `web/src/persist.js` (+
    persist-worker.js, persist-db.js): `nt.html?persist=DIR&persistdb=NAME`
    restores DIR from IndexedDB over the bundle, and a Worker snapshots DIR
    out of the global kernel every 30 s, on hide and on pagehide (as
    pewasm's IDBFS syncfs). The eject's nt.json says `persist` =
    `<prefix>/drive_c/users`; index.html passes it unless `?persist=0`.
    **Bench runs must pass `persist=0`** (page-run's Chromium profile
    keeps IndexedDB between runs).
  - Start-up marks for the world-up profile: `[nt-page] fetching/fetched/
    untarred/running at T` and one `[unit] KEY MiB: load, compile` line per
    unit (wall clock ms mod 1e7 on both).
- **Results on edger (bench box, BENCH_CORES default, seed pinned, 4x clock,
  world fps from world-up+90 s to 330 s; pewasm = run-pewasm-page.sh):**

  | sequence | ejected page | pewasm | ratio |
  | --- | --- | --- | --- |
  | site17b (upload present), ABBA | 34.6, 33.8 | 44.4, 48.6 | 0.74 |
  | site17d (direct present), ABBA | 45.3, 45.2 | 48.0, 47.7 | 0.95 |

  - Profile (site17b): the compositor worker spent 27% of its time in
    `texImage2D` (the WebGPU canvas uploaded into WebGL every composited
    frame, GPU-synchronous on the P620) and Noita's main thread waited on
    the previous frame for ~27% of each 10 s. On worldmachine's RX 6500 XT
    the same wait was ~0.2%, which is why the old 1.31x does not carry over.
  - Fix (7a3f88c0): Mesa presents straight into a page canvas laid over the
    output where the GL window is, whenever that window is on top
    (`gl-rect` messages place it); otherwise the old upload. `?glpresent=
    upload` keeps the old path. +32% on edger.
  - Headless screenshots do not show the overlay (nor the uploaded GL
    frame); visual checks run with `GEX_NT_GL_READBACK=1`.
- **Start-up (world-up):** units are not the cost (all 38 compiled and
  installed within 7 s of the process start; noita.exe's 131 MiB compiles in
  0.35 s, lazily). Process-time line on edger: first present 6 s; Noita's own
  init on the main thread to ~28 s (guest 16.8 s, then 7 s of synchronous GL,
  mostly the compositor busy uploading); menus; a second stall at the
  loading screen; world at 57-65 s. The gex input script clicks at presented
  frame 224 while pewasm's clicks at frame 180, so world-up is not like for
  like; the measured frames-per-second during menus/loading are bounded by
  the same compositor wait as above.
- **Download (site17b/d):** 74.7 MB in all (units 261 MB -> 31.0 MB, rootfs
  94 MB -> 30.8 MB, shim 45 -> 6.8, Mesa 26.6 -> 3.1, kernel 13.3 -> 1.9, page
  JS 1 MB), against pewasm's 190 MB (which ships all of data/fonts, 165 MB;
  the eject ships the files the session read).
- **Packaging:** `node web/eject-site.mjs PROJECT OUT [--bench] [--overlay
  DIR] [--drop PATH]`: zstd-19 files decoded and cached by sw.js (which also
  isolates; sync XHR from Workers goes through it), a launcher with a
  progress bar, `_headers`, `serve.mjs`, README. Without --bench the
  scripted input and the 4x clock are dropped.
- **Play-through (edger, site17p = --overlay overlay-play: a `killme` mod
  that kills the player 900 frames after each spawn):** release notes, main
  menu, New Game, mode select, world (seed 1767225600), walking, levitating,
  shooting, death ("Game Over", cause 'killme'), stats screen, New Game
  again into a new world, pause menu. Scripts `$S/shots/p1..p7.json`,
  `$S/play-edger.sh`.
- **Saves across reloads (edger, p7/p8):** in the world, Escape, Save &
  Quit (the process exits, status 29), a snapshot stored 46 files (850 KB)
  of 62 under drive_c/users, the page reloaded and restored 62; the main
  menu then offers Continue (and skips the release notes it had shown), and
  Continue puts the player back where they had walked.
- **35-minute soak (edger, bench settings):** no freeze (presents never
  stall after world-up; the only stalls are loading screens before 96 s).
  World fps falls over the run on both runtimes, so it is the game (an idle
  player, a growing world), not gex: ejected 44.1, 41.3, 38.9, 37.3, 36.0,
  32.3, 30.2 per 5 minutes; pewasm 43.6, 30.5, 23.0, 19.1, 16.9.
- **Merged master 3718d0ca** (81a0ae2a; unit keys moved): p-noita17 relinked
  at the new keys (`$S/logs/build18.log`); the measurements above are on the
  previous keys (site17b/site17d).

- **Resume (next):** (1) repack the site from p-noita17 at the 3718d0ca
  keys (`node web/eject-site.mjs p-noita17 site18 --bench`; copy
  site17d.zcache first) and re-run the ABBA (`legs-edger.sh N pew SITE SITE
  pew` through bench.sh); (2) the remaining ~5%: the main thread still waits
  on the previous frame (profile with `prof-edger.sh`), then the compositor
  items (render pass per draw); (3) world-up like for like: one input
  script for both runtimes (pewasm's frames 60/120/180 vs gex's
  12/90/144/224), then the menu/loading stalls; (4) audio waits for the
  coordinator's call.

- **Merged master 65d5fdf8** (fbd45ce3); p-noita17 relinked (`build19.log`),
  site18 packed from it (eject-site now packs only this build's units:
  97fe6712). **ABBA on edger, site18:** ejected 41.9, 46.6 vs pewasm 46.1,
  48.8 = 0.93x (one low leg).
- **The main-thread wait (profile p3, site18):** "GL previous frame" is
  still ~2.3 s per 10 s of Noita's main thread: a submit waiting for the
  previous async GL batch to finish in the compositor worker. That worker
  is 73% "(program)" (native time inside the WebGPU/Dawn calls, not JS or
  Mesa), 25% JS/wasm, no readbacks (all maps write-only, 0 forced
  submits). Next: a Chrome trace (gpu, devtools.timeline) of that thread to
  see what the native time is (getCurrentTexture back-pressure on the P620,
  i.e. GPU-bound, or wire flushes), then pewasm's Mesa flags (E2/E4).
- **World-up, like for like:** `site18i` = site18 with pewasm's input
  schedule (clicks at presented frames 60/120/180, gex's coordinates;
  `eject-site.mjs --input`), and `$S/wup-legs.sh N SITE` measures world-up
  after navigation on both (pewasm polled every 100 ms in the page; gex from
  the first `peek>0` stats line, `[world-up]` in page-run's output).

- **Compositor worker, Chrome trace (`$S/trace-edger.sh`, page-run
  `--trace`):** the 73% "native" was WebGL's
  `GetResourceForPushFrame` -> `CopyRenderingResultsFromDrawingBufferToResource`
  -> `ReadPixels`: headless Chromium composites in software, so every
  frame the WebGL output canvas pushes is read back (2.9 s of 5 s), even
  with the GL window presenting itself. The compositor now draws nothing
  when the output has not changed (a signature of windows, textures,
  pointer and focus; f68abb91's sibling commit), so no frame is pushed.
  After it: the worker 52% busy, the GPU process 40%, no readbacks.
  **ABBA (site18s): ejected 59.3, 55.7 vs pewasm 47.5, 45.4 = 1.24x**;
  world-up 59 -> 49 s after the process start. (pewasm's page in the same
  headless setting is measured the same way, so the pair stands; the
  readback itself is a headless artifact that a GPU-composited browser
  does not pay.)
- **World-up, like for like (site18ir, pewasm's frame schedule):** pewasm
  29.2-32.3 s after navigation, ejected 62.7-66.4 s (before the change
  above). Timeline (process time): first present 6 s; Noita's init 6-26 s
  (guest 16 s); menus 26-35 s (clicks at frames 60/120/180); loading
  36-53 s (guest 8.3 of 10 s: world generation); 55-62 s mostly GL host
  (5.6 s: shader translation); world 62 s. **The eject runs with
  `-single_threaded_loading` (recorded) and pewasm does not**:
  `eject-site.mjs --args ''` (site18m) runs AOT-only without it (no stops,
  locally); its world-up A/B is `$S/wup-ab.sh`.
- Mesa with pewasm's flags (SIMD + ThinLTO; meson release is already -O3):
  `$S/build-mesa2.sh` with `GEX_MESA_CROSS2=$S/mesa-o3.ini` into
  `$S/mesa-o3-out` (building).

- **World-up A/B (edger, pewasm's click schedule, seconds after
  navigation):** pewasm 29.2-32.3; ejected before the frame skip 62.7-66.4;
  with it (site18is) 53.4-56.4; plus multi-threaded loading (site18m, no
  `-single_threaded_loading`) 51.5-52.6 (and 51.7-57.2 against site18ir).
  Multi-threaded loading runs AOT-only with no stops; worth keeping for a
  shipped site (`--args ''`).
  - Where the rest is (site18m, process time): first present 6 s; init
    6-25 s, the main thread mostly blocked in the ConcRT condvar provider
    (`cond_wait_`, 59% of 7-24 s) on the loader threads (4 threads, about 26 s
    of guest time between them) and 24% in synchronous GL submits; menus
    25-36 s; loading 36-52 s (one thread 10 of 10 s guest: world generation).
    The loader and world-generation threads are guest code, so this is F's
    side (codegen) unless a page profile shows otherwise; their Workers
    start after the profiler attaches (`--profile` attaches at AT only).
- **Merged master 5ab54ab2** (4da10a61; no gex-cgen change, keys kept).
- **Mesa with SIMD + ThinLTO** (`$S/mesa-o3-out`, `$S/mesa-o3.ini`) against a
  control from the same tree (`$S/mesa-ctl-out`; its replay matches the
  copied worldmachine build in size): sites site18ctl / site18o3 (project
  copies in `$S/p17v/`), ABBA queued (`$S/edger-mesa1.txt`).

- **Mesa flags result (edger ABBA, 5 legs):** control 53.6, 56.8 vs SIMD +
  ThinLTO 51.5, 56.9: no gain (noise); not adopted. pewasm 49.9 in the same
  sequence (ejected 1.1x).
- **The main-thread wait is gone:** after the frame skip, "GL previous
  frame" is 1-3 ms per 10 s (was 2.3-2.7 s). Noita's main thread per 10 s:
  guest 4.3-4.9 s, NT calls 2.1-2.2 s, waits 2.8-3.3 s, GL host 0.12 s. The
  NT calls and GL submission are F's now; the compositor worker is no
  longer on the critical path.
- **Resume (E):** world-up (pewasm 30 s vs 52 s): profile the loader and
  world-generation threads (attach the profiler before they start, e.g.
  `--profile 2,40`), compare with pewasm's start-up; ship with
  multi-threaded loading. Then the compositor items only if a profile
  puts the worker back on the critical path.

- **Post-merge check (site19, master 5ab54ab2 merged, keys unchanged: 38
  units present, 0 built; p-noita17's user/eject.json now has `"args": []`,
  which the web build honours since this commit's sibling):** ABBA on edger
  ejected 57.2, 58.6 vs pewasm 46.0, 45.0 = **1.27x**, world-up 49 s after
  the process start. Multi-threaded loading is the project's setting
  (user/eject.json `args`), not an eject-site default: eject-site is
  generic, and `--args` stays the override.
- Start-up profiles from the process start for both runtimes:
  `$S/pw-prof-start.sh TAG AFTER SECS` (pewasm, F's browser-prof.mjs) and
  `prof-edger.sh SITE TAG 1 60` (page-run now profiles Workers that start
  during the window too).

- **Queued (user decision, after the start-up work): no WebGL in the
  browser.** Port the Wayland compositor (web/src/compositor-worker.js, and
  dom-compositor.js if still live) from WebGL2 to WebGPU on the GPUDevice
  Mesa uses: GL windows become textures the compositor samples directly
  (no cross-API copy, no second context, no webgl2 start-up retry, no
  overlay canvas), wl_shm rows uploaded with queue.writeTexture from guest
  memory; keep the frame-skip semantics (no present when nothing changed).
  web/src/gl.js and the demos' WebGL path are being removed on master by
  the coordinator.
- **Start-up profile from the process start (site19, prof-gst):** every
  guest thread's Worker spent 2.5-3.7 s in `loadFile` (synchronous
  requests through the service worker) plus 0.5-1.5 s compiling modules:
  `loadUnit` fetched each unit's c.wasm (131 MiB for noita.exe) only to
  read its entries, though the Module came from a sibling. Fix 065139bb:
  the bytes are fetched lazily, only by a Worker that compiles. A/B
  site19/site20 queued (`$S/edger-wab3.txt`).

- **Start-up: threads recompiled every unit (fixed, 35578368).** A
  per-second timeline of the start-up profile (`$S/profline.mjs`,
  `$S/profimg.mjs`) showed Noita's loader threads starting one after the
  other, each busy ~3 s while the rest (main included) waited in the ConcRT
  condvar provider: each new thread's Worker fetched and compiled all 39
  units (352 `[unit]` lines in one run). host.mjs's thread start path
  never sent the process's compiled modules (the `start` closure that did
  is dead code). With them: 44 unit compiles per run. **World-up after
  navigation (edger, A/B x3): site19 50.6-51.6 s -> site21 36.5-40.6 s**;
  pewasm 30.2 s. ABBA (site21): ejected 56.8, 56.8 vs pewasm 48.3, 48.0 =
  1.18x; world-up 35-36 s after the process start. The lazy c.wasm fetch
  (065139bb) alone measured nothing; it matters now that threads have the
  Modules (no bytes fetched at all).
- pewasm's own start-up timeline (`$S/wup-pewasm2.sh`, timestamped
  console): process ~4.3 s, first surface 4.7 s, its input frames 60/120/180
  at 10.6/11.0/11.3 s, world 29.2 s: about 6 s of init and 18 s of world
  generation. Ours before the fix: init ~20 s (the serial thread starts),
  world generation ~16 s.

- **Unit preload (a2… see log, eject page `?preload=`):** the page
  compiles every unit with `compileStreaming` while the bundle downloads
  and hands the Modules to every Worker (`preModules`); no Worker fetches
  or compiles a unit (0 `[unit]` lines). World-up A/B x3: site21 37.5-38.5
  s -> site22 35.6-38.8 s (mean 38.2 -> 36.7). pewasm 30.2 s.
- **Where the remaining ~6.5 s is (site21, per-second timeline, process
  time):** boot 0-5 s on the main thread (now mostly gone with the
  preload: 40% loadFile, 10% compile; 17% gex_key sha256 remains);
  shader translation in the compositor worker (Tint SPIR-V->WGSL) at
  9-12 s and 40-43 s, during which Noita's main thread waits on GL
  (~3 s each), so the menus run at ~40 fps where pewasm's run at ~170
  (its clicks at frames 60/120/180 land within 0.7 s, ours within ~3 s);
  world generation 19-40 s on one thread, guest code plus spin-yields
  (~19k NtYieldExecution per 10 s; 0x1d80 guest 7.6-9.9 s of 10), against
  ~18 s in pewasm. Levers: a persistent SPIR-V->WGSL cache (IndexedDB,
  helps every load after the first), the shader translation off the
  critical path, and world generation (F, codegen).

- **State at the shutdown (2026-10-05).** Head is the WIP commit "WIP:
  compositor on WebGPU" on merge/eject9. Last measured good page: site22
  (d1ad650c; 1.18x fps, world-up 36.7 s vs pewasm 30.2 s). The WIP is
  untested (syntax only); do not package it before step 1.
  - Done in the WIP: web/src/gpu-device.js (one shared device:
    `requestGexDevice`, and `textureCanvas()`, an OffscreenCanvas whose
    'webgpu' context hands Mesa a texture on that device, so gex_gl_display
    copies each displayed frame into a texture the compositor samples);
    mesa-gl.js takes `options.gpu`; compositor-worker.js: Painter, TextCache,
    cursor on WebGPU (one pipeline, dynamic-offset uniforms, bind group per
    texture, scene texture copied to the canvas, `describe()` async via
    `Painter.read`), wl_shm uploads with `queue.writeTexture` from guest
    memory, GL windows drawn from `glFrame().tex`, the frame skip keeps
    working (GL frames enter the signature by presented serial), the
    overlay/direct-present path and the webgl2 retry removed in the worker.
  - Resume:
    1. Page side: delete `#glOverlay`/`gl-rect` handling and the `present`
       transfer in web/src/compositor.js (the worker ignores them now);
       remove dom-compositor.js's `getContext('webgl2')` loseContext line.
    2. Build the page, rsync into p-noita17/runtime/web/page, pack a site
       (`eject-site.mjs p-noita17 site23 --bench`, copy site22.zcache), run
       locally with play.mjs: check the GL frame's orientation (flip uv if
       upside down), the format (Mesa configures the canvas format; sampled
       BGRA reads correctly), titles, cursor, `describe()`.
    3. tests/nt browser-gui (uses describe) and a GL demo page.
    4. edger: ABBA vs pewasm (site22 as the before), world-up A/B
       (`wup-ab.sh`), and a Chrome trace (`trace-edger.sh`) to confirm no
       readback and no second context.
    5. Then world-up's remaining levers (above): persistent SPIR-V->WGSL
       cache, shader translation off the critical path; world generation to
       F via the coordinator.
  - Nothing of mine is running on gooner or edger.

- **After the restart (2026-10-05, gooner):** the WebGPU compositor is
  finished (5d5eb91f): the page-side overlay (`gl-rect`, `?glpresent`) and
  dom-compositor's WebGL2 call are gone. Found on the way: **on a software
  adapter (SwiftShader: every headless run on gooner, and the gate) Chromium
  149 has no shared-image backing for any WebGPU canvas**; the first
  `getCurrentTexture` of any WebGPU canvas loses *every* device in the
  renderer ("destroyed"), Mesa's included, and a probe cannot detect it
  without that loss (`$S/wgt/probe.mjs`, two.html). So the compositor reads
  the scene back into a 2D canvas when the adapter is the fallback/swiftshader
  one, and copies it into a WebGPU canvas otherwise (`?present=canvas|
  readback`). Mesa itself never touches a real canvas now (textureCanvas).
  - Checked: site23 locally (SwiftShader) shows Noita upright, in colour,
    titled, cursor drawn; tests/nt/browser-gui.mjs x86/x64 plain and --gl all
    ok; tests/nt/run-browser.sh 49/49. On edger (canvas path): presents and
    describe()'s readback move, no GL readbacks.
  - Edger queue `$S/c23/queue.sh` (log `$S/c23/queue.txt`): ABBA pew/site23/
    site22, world-up A/B site22/site23, trace tr23, readback shots.
- **Shader translation cache (ed9511a5 + Mesa patch
  `$S/mesa-patches/0002-*`, committed in `$S/gexroot/third_party/mesa`; built
  in `$S/mesa-ctl-out`, copied to web/public/gex):** Tint's SPIR-V -> WGSL
  answers kept in Mesa (in-process: 507 hits of 563 by the menus, i.e. the
  same SPIR-V is translated again and again), loadable from a blob shipped
  with the site (`eject-site --wgsl-cache FILE`; recorded by play.mjs's
  `{"do":"wgsl","file":F}` step) and from IndexedDB (`?wgslcache=all|site|0`;
  bench runs use `site`). Blobs carry the Mesa build's identity and are
  refused by any other build. Locally with the blob: 0 misses, compile 303 ->
  11 ms. The Mesa patch has to reach mesa-wink (coordinator).
  - Edger chain `$S/c24/chain.sh` (log `$S/c24/chain.txt`): trains on site24
    (blob at `$S/c24/edger.wgsl`), packs site24w, then world-up A/B x3
    site24/site24w and legs pew/site24w/site24/pew.

- Harness: `$S/play.mjs` = page-run.mjs + `--script FILE` (timed clicks,
  keys, holds, screenshots, evals, `persist` snapshots and reloads through
  CDP input, as a person would).

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
- **cg12 (2026-10-04):** merged merge/fjit (632bb7ba). seh, cxx and apc
  pass ejected on gex-rt against cg12's units (overlay
  `scratchpad/eject9/units-cg12`, symlinks into `fjit/cgcache-cg12`; the
  `UC=` variable of `ntcase.sh`).
- **State at the reboot (2026-10-04):** the Noita rebuild on cg12 has not
  started. `target/noita-prefix/gex-layout` is now the staged cg12 layout
  (sha256 63b13c67..., the old one is kept as
  `scratchpad/eject9/noita/gex-layout.pre-cg12`). F's cg12 noita.exe unit
  (`fjit/chain2.sh` -> `fjit/noita-cg12.log`) was still building at the
  reboot, so the cg12 recording waits for it: a recording without a
  noita.exe unit would log every lookup as an interior entry. The old
  recording is at `noita/rec-cg10`; a stale p-noita9 build (cg10 keys) was
  stopped and can be deleted. Resume:
  1. Make sure cgcache-cg12 has noita.exe at 0x400000 (rerun F's
     chain2.sh if its log has no `exit 0`).
  2. Record: `OVL=$S/eject9/noita/ovl12 CPUS=1,7 ./rec-noita.sh 420`.
     ovl12 holds symlinks to cg12's unit dirs and a copy of `icall`.
     **Never link `build.lock`:** the first try did, and waited on F's
     build.
  3. `gexc eject noita/rec --out p-noita12`, then `build.sh` with
     `GEX_EJECT_UNIT_CACHE=$S/eject9/noita/ovl12` at nice 19 on 0,2,6,8.
     Note the regions line: whether expanding call continuations stays
     small on Noita's big functions.
  4. Run on gex-rt (`PROJECT=... run-ejected.sh`), check 0 translations,
     then the fixed-seed pewasm pairing (step 3 of the plan), then E8.
- **Noita on cg12 (2026-10-04):** recorded with F's complete cg12 units
  (`noita/rec`, 420 s, noita.exe's unit 100,222 entries/128 MiB placed in
  117 s, world at 293 s), ejected (`p-noita12`: 38 images, 58 files,
  96 MB) and built reusing every unit by key. Region entries stay small:
  none in noita.exe; sechost 2 recorded -> 7, combase 2 -> 6, lua51 82
  (+7 by direct calls), 6 MB in all. On gex-rt: no translations, no
  tier-1 blocks, world-up 20-29 s unloaded, resumes 10 (NtContinue into
  ntdll, and the sechost/combase region entries), provider_try ~1.4 per
  world frame, no throws.
  - The first eject had no `runtime/gex-rt` (GEX_RT unset, gex-rt lives in
    `target/x86-rt`), so run.sh fell back to gexc's AOT-only mode without a
    word. Eject now refuses a Windows program's project without gex-rt
    (bd65f052).
  - One run of nine (on gexc AOT-only, under load) stalled in loading at
    t=28 s, just before the three NtContinue resumes (presents 235,
    provider_try 1762, resumes 7); the stats thread then blocked in the
    60 s report. Six gex-rt runs (`hanghunt.sh`, watchdog with eu-stack)
    did not reproduce it.
  - **Neither fixed-time variable pins Noita's world.** "Setting random
    seed" differs every run on pewasm (PEWASM_FIXED_TIME) and on gex,
    also with the guest wall clock frozen. noita.exe 0x883540: the world
    seed is the WORLD_SEED magic number (0x1206fe8) if set, else drawn from
    the engine RNG, which is seeded at start-up through a config callback
    (0xddcac0, the line's printer). The "fixed seed" pairings above were
    random-seed pairings; pinning needs WORLD_SEED on both runtimes.
- **Paired, world seed pinned (port/seedpin, WORLD_SEED 1767225600 on both),
  2026-10-04 02:42-03:04:** cg12 Noita, gex-rt vs pewasm `out/noita/build`,
  ABBA on cores 1 and 7, both at a 4x clock (PEWASM_TIME_SCALE=4, the
  recording's GEX_NT_TIME_SCALE=4), world-up+30 s to +210 s. Load average
  17-19; builds on 0-2,6-8 shared 1 and 7 in every leg.

  | leg | run | world fps |
  | --- | --- | --- |
  | A1 | gex-rt | 55.61 |
  | B1 | pewasm | 50.00 |
  | B2 | pewasm | 52.79 |
  | A2 | gex-rt | 49.71 |

  gex-rt 52.7 against pewasm 51.4 (1.02x), with no translator in the
  binary and 0 translations. An earlier pass ran pewasm on a 1x clock (its
  frame cap: 58.3 at 0.97x realtime) and is void, as is the first pass that
  U's Muck run shared.
- **E8, the web build (2026-10-04, merged merge/web9 a5b24767):**
  - `gexc eject --web-shim web/public/gex [--web-page DIR]` vendors the
    x86 shared-memory shim, kernel, seed and host JS (`runtime/web/nt`),
    and the page (`web/build-eject-page.sh DIR`: nt.html built with vite,
    Mesa's wgpu replay beside it; `runtime/web/page`). build.sh then also
    builds every image's unit in the browser's (guarded) flavour, its
    region entries and providers into `build/web/cgen`
    (`GEX_EJECT_WEB_UNIT_CACHE` reuses units by key), copies the stateful
    providers (Lua) to `build/web/cgen/stateful/`, writes
    `build/web/nt.json` (exe, prefix, cwd, args, env, settings) and, with
    a page, `build/web/rootfs.tar`.
  - `./run.mjs`: the web build under Node (run-node.mjs `--no-jit`).
    `node serve.mjs` + `index.html`: in a browser (`?jit` for the
    fallback, `?env=` for more settings).
  - The browser runtime, AOT-only (`gex_set_jit(0)`, every Worker): index
    1 stops loudly, or raises the guest's access violation for memory
    nothing executable is at; region-entry modules (`regions/<key>`) load
    after their unit; the trampoline page's syscall continuations have
    region entries (the browser resumes there); units of newly mapped
    images, new threads and a Worker's catch-up on the function table are
    served inside the call (`serve_in_place`), not by trapping out of the
    guest's frames; user APCs are delivered nested (fb7dfd52's browser
    half). Stateful providers (`GEX_NT_STATEFUL`) work as natively, with
    `provider_try` a JS try/catch.
  - A training run, `./run.mjs --record-entries` (the fallback on;
    `GEX_NT_ENTRY_LOG`): where the browser entered unit images without an
    entry, kept in `user/web-entries.txt`, which build.sh turns into region
    entries. tests/nt needs none since the nesting fixes.
  - Results, AOT-only: tests/nt x86 all 17 (hello_args, sync, mtx,
    crtmem, tls, icall, user_win, gdi, exitproc, input, display, window,
    lowmem, filemap, apc, seh, cxx under Node; glcompat, GL on Mesa over
    WebGPU, in headless Chromium from its project's page), each with the
    stdout and exit of the recording; the same projects pass on gex-rt.
    The LuaJIT host program prints the same on the web build as on gex-rt.
    (filemap needed run-node.mjs to bundle every directory: a sparse
    prefix's `Temp` had no parents.)
  - A project's own files go in `user/files/<guest path>` (copied into
    build/rootfs and the page's tar): Noita's world seed is pinned there
    with `port/seedpin/pin.sh mod|save`.
- **E8: Noita's web build in the page, AOT-only (2026-10-04).** cg12
  recording, `gexc eject --web-shim web/public/gex --web-page DIR`
  (`p-noita12w`), build.sh: 38 browser-flavour units (25 built here: the
  guarded flavour is another key), world seed pinned through
  `user/files` (port/seedpin). In headless Chromium on hardware WebGPU
  (Mesa wink's wgpu replay):
  - The first AOT-only run stopped at noita.exe+0x1791ac (a call
    continuation; the browser leaves wasm where native does not). A
    training run (`index.html?record`, the fallback on) crashed in
    LuaJIT's own code: the browser retired the Lua provider's entries on
    pages Noita's mod sandbox patches. With providers exempt (as
    natively), the training run reached the world and recorded 73
    entries in `user/web-entries.txt`.
  - Rebuilt (169 region entries), the AOT-only page runs to the world and
    on: 420 s, world frame 13,859, no stops, nothing translated
    (callouts 0, in-place installs 0). World fps 34-45 in 60 s windows
    (untimed: build cores 0,2,6,8, Chromium beside it; pewasm's page is
    about 59 per stream B).
  - The training run now behaves as the AOT-only run except that a miss
    translates (`gex_set_aot` without `gex_set_jit(0)`): units and threads
    served in place, catch-ups in place. It records 13 entries for Noita,
    all in noita.exe (the fallback alone recorded 73, most of them
    continuations of the syscalls it left wasm for). Where the browser
    still leaves wasm and native does not: a call-out (training only) and
    the waits and yields the browser parks in JS (`GEX_NT_RESUME_LOG`
    names `set_resume` sites). With the 13 (108 region entries in all) the
    AOT-only page ran 600 s clean, world frame 22,209.
  - build.sh warns, by image, of functions a unit was built without
    (`nt_cgen::left_out`): the guarded flavour leaves noita.exe 0xdd80a0
    and msvcr120.dll 0x1637a00 (`clang wanted a shadow stack`); not
    reached so far. A paired page measurement (pewasm's page and the
    ejected page, same cores) is next.
- **The page against pewasm's page (2026-10-04; the target: native
  numbers are debugging proxies).** Cores 5,11, hardware WebGPU, world seed
  pinned on both (1767225600), world fps from world-up+90 s to the end of
  330 s (stream B's wfps.sh window).
  - Guarded browser units (`p-noita12w`), ABBA: ejected 39.2, 37.8 against
    pewasm 55.6, 59.5: 0.67x.
  - Then: B's step 0 (the web build is the native units, unguarded:
    `build/units`, `nt.json` `cgen`/`unguarded`), cg14 units (F's, carries
    off), and the yield policy. Same ejected project per arm (`p-noita14y`):

    | arm | runs | mean | vs pewasm |
    | --- | --- | --- | --- |
    | pewasm page | 58.8, 56.7 | 57.8 | 1.00 |
    | os (the spin, today) | 47.2, 51.0 | 49.1 | 0.85 |
    | wake (R's e094d4ae + park patch) | 62.7, 66.1 | 64.4 | 1.11 |
    | sleep:50 (reference) | 63.1 | 63.1 | 1.09 |

    wake is on `exp/yield-wake` (a hand port) until R's and B's merges.
  - V8 profile of the main thread (os): units 10.7 ms per world frame
    (noita.exe 61% of it; msvcr120 14.5%, ntdll 8.8%, kernel32/kernelbase
    8%: the yield path, the heap, TlsGetValue), shim syscall glue 5.9 ms,
    performance.now 1.5 ms. pewasm's page: wasm about 7.6 ms per frame, its
    main thread 63% busy (a yield gives the event loop a turn). Fixed since:
    yields sampled apart (every 512th), cgen::drain lock-free.
  - Next levers: R's crt_heap / nt_qpc providers; the link group (merge-plan
    §9.6: noita.exe with msvcr120 and msvcp120 in one module, IAT calls
    direct, ThinLTO), pewasm's build.
- **State at the reboot (2026-10-04, evening).** merge/eject9 is the
  branch; exp/yield-wake holds the os/wake experiment only (drop it after
  the integ merge).
  - G1 (link groups) is written, built, **not yet tested** (WIP commit):
    `gex_cgen::image::build_group_module` (emit_and_link over several
    recoveries; one member gives the same C as before), `nt_cgen::
    prepare_group` (IATs resolved within the group, API sets mapped to
    ucrtbase/kernelbase, `FF 15` call sites hinted, `<cache>/groups/<key>/`),
    `install_placed` in the native host (members held until every member is
    placed, then the group module; `<cache>/groups/index.txt`), and
    user/eject.json `"link_groups"`. The browser shim does not install
    groups yet (it uses the members' own units, so it still runs).
  - Resume:
    1. Wait for the coordinator's green on integ/2026-10-04, then merge
       merge/main into merge/eject9 (17 files conflict: libprov, aot_only,
       ftab, nt_cgen, gex-eject, the browser shim; keep both behaviours:
       providers kept on rewrite, no native gating, top-down provider
       stacks except with GEX_NT_STATEFUL, gex_provider_alloc_top/stack/
       nt_qpc). Verify the native and browser unit keys of msvcr120 and
       ntdll match. Drop exp/yield-wake.
    2. Test G1: `scratchpad/eject9/ntgroup.sh cxx x86
       '[["cxx.exe","ucrtbase.dll"]]'` (gex-rt and Node), then the group in
       the browser shim (cgen.rs place(): hold members, publish
       `groups/<key>` once all are placed; gex.mjs loadUnit of
       `groups/<key>`), then Noita with
       `[["noita.exe","msvcr120.dll","msvcp120.dll"]]` (about 2 h of clang
       at nice 19) and its page ABBA against pewasm (abba-yield.sh shape,
       wake on, cores 5,11 when the coordinator gives them).
    3. G2-G4 per §6a.
- Found: a unit of Noita's lua51.dll built at a rebased address (0xb70000)
  lowers the import thunk `jmp [IsProcessorFeaturePresent]` at +0x4e89a to
  `gex_illegal`, so its DllMain faults (also with --jit-fallback). The Lua
  provider replaces DllMain, so the eject does not depend on it.
