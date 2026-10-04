# A 64-bit guest memory, chosen per process

Stream U, 2026-10-03. Design only: nothing here is built yet. It goes to the
coordinator before the shared emitter, cgen or shims change, because streams F
and R are working in those files.

## 1. Why, and the decisions already made

All three Unity games need more address space than a wasm32 guest has
(docs/games.md, "Address space"). Unity's memory manager gives every
allocator block its own 256 MiB-aligned slot. Measured natively: Muck uses 19
slots at its menu, Hollow Knight 17 at its title screen, Rain World 16 after
engine init, and the count grows with what a level loads. A gex x86-64 guest
has 2 GiB for mappings, and wasm32 can never give more than 4 GiB (16 slots
before anything else is placed). Launch knobs do not close the gap: Muck has
none that changes the count, and Hollow Knight's best (`-memorysetup-*` heap
blocks at 256 MiB) takes it from 17 to 13.

The user's decisions (coordinator, 2026-10-03):

- **memory64, not software translation** of high addresses.
- **wasm32 stays the default** for every guest. Programs that fit, every
  i386 guest and Noita included, keep today's code and today's speed.
- **memory64 is chosen at run time, per process, only when necessary**, in
  two levels: at exec (a flag, a learned per-binary record, optionally a
  general heuristic; no game lists), and a live switch when a running wasm32
  process overflows.
- **Every tier has both flavours:** tier-1, gex-fjit, gex-cgen units (the
  flavour in the unit key) and both shims (V8 caps memory64 in the browser).
- **Resurrect 835d18a6's deleted build** rather than writing it again.

## 2. What was deleted, and what to take back

835d18a6 removed a complete memory64 build (6079b8b7, c353e8a8, 68290edb,
df10332d), measured and rejected as a replacement for wasm32:

| | guarded wasm32 | memory64 |
|---|---|---|
| gzip | 10004 ms | 9557 ms (control without the guard: 10017) |
| perl | baseline | 13% slower |
| nbench | baseline | NUMERIC SORT -16.7%, IDEA -27.4%, STRING SORT +36.4% |

That verdict stands for programs that fit: they stay on wasm32. Here
memory64 is the only way some programs run at all, so its cost is the price
of running them, not a regression.

What the old build had, by file (all to be taken back, adapted to today's code):

- `gex-emit/src/tier1.rs`: `Context::memory64`, `ptr_ty`, `scratch_addr`,
  `ctx_base`, `guarded_byte_address`: i64 addresses into loads, stores,
  atomics, `memory.copy`/`fill`, and **no bounds guard** (a 64-bit memory has
  the engine's own explicit check, so gex's guard only duplicates it).
- `gex-emit/src/wasm.rs`, `module.rs`: the memory import with limits flag
  0x04 and u64 limits; i64 data-segment offsets.
- The browser shim built for `wasm64-unknown-unknown` (`build64.sh`), with the
  glue choosing the shim (`gex.mjs`, `worker.mjs`, `run-node.mjs`,
  `wasm-sig.mjs`). In the browser the shim shares the guest's memory
  (docs/nt-layer.md §1.2), so a 64-bit guest needs a 64-bit shim.
- wasmtime's `wasm_memory64`.

What did not exist then and does now: gex-fjit, function-tier units (C via
clang), export providers, the function table, the NT layer, the browser NT
shim and its heap, Mesa in the page.

## 3. The flavour

```rust
// gex-abi
pub enum MemFlavor { M32, M64 }
```

One value per process, fixed at exec (level 1) and changed only by a live
switch (level 2). Everything that emits or links code takes it as a
parameter. Nothing branches on it at run time: an M32 process runs exactly the
modules it runs today.

| | M32 (today) | M64 |
|---|---|---|
| memory type | shared, 0xffff pages, i32 index | shared, i64 index, max `LIMIT64` |
| guest limit | `GUEST_LIMIT` = 4 GiB - 64 KiB | `LIMIT64` (§4) |
| address in code | guarded: past the limit goes to `POISON_ADDR` | plain i64, no guard; the engine traps out of range |
| translations | tier-1 / fjit / units, M32 | the same generators, M64 output |
| cache keys | as today | `+mem64` in the unit key and the fjit and tier-1 cache keys |

**Layout: only data moves up.** Everything below 4 GiB keeps today's layout:
the reserved low region (thread contexts, dispatch directory, FCACHE,
PATCHED_PAGES), images, stacks, the NT allocator's range, the browser shim's
own data. An M64 process adds a **high arena** `[4 GiB, LIMIT64)` for
non-executable data:

- **Linux** (`gex-kernel/src/mem.rs`): `mmap` without `PROT_EXEC` and without
  a fixed address goes to the high arena, which also takes `PROT_NONE`
  reservations. Executable and file-backed executable mappings stay below
  `MMAP_TOP`. Unity's 512 MiB reservations go high; Mono's JIT code
  (`mmap(PROT_READ|PROT_WRITE|PROT_EXEC)`) stays low.
- **NT** (`nt/vm.rs`): `NtAllocateVirtualMemory` without an executable
  protection, and without `zero_bits` or an address that forces it low, goes
  to the high arena. `MEM_TOP_DOWN` and `PAGE_EXECUTE*` stay low. Images and
  sections stay low.
- So the dispatch directory, PAGE_GEN and PATCHED_PAGES keep their 2 GiB
  reach: code never lives high. **Open:** `mprotect`/`NtProtectVirtualMemory`
  that makes a high page executable. It is rare (a JIT allocating data then
  flipping it), and at first it fails loudly with the address. The general
  answer is a sparse page-generation table for pages above the directory,
  which the function-table path (hash address map, §9.8.1) needs anyway.

Keeping code low also keeps every per-page array in the low region the same
size, so M64 costs nothing there.

## 4. LIMIT64, and what step 0 measured

`LIMIT64` is **16 GiB on both hosts.** V8 caps memory64 at 262144 pages and
refuses to compile a module whose declared maximum is larger, and an imported
memory's maximum has to match the module's, so one figure lets one module
run natively and in the browser. A Unity game's 20-30 slots at 256 MiB
alignment reach about 9-12 GiB of address with about 650 MiB committed, so
16 GiB holds them. The high arena is allocated bottom-up from 4 GiB, first
fit, because the browser grows memory to the highest address in use.

Step 0's smoke tests (2026-10-03, `scratchpad`, no gex code involved):

| | wasmtime 27 | V8 (Node 26, Chromium 151) |
|---|---|---|
| shared memory64, max 16 GiB, grown to 12 GiB | 31 µs, RSS 9.8 MB | 0.1-3.4 ms, RSS 51 MiB: committed lazily |
| a second thread (store / Worker) reads it, `wait`/`notify` | yes | yes |
| an access past the end | traps | traps |
| table64 `call_indirect` | yes | yes |

- **wasmtime:** a shared memory has to be a static allocation, so the
  engine needs `static_memory_maximum_size(LIMIT64)` (and `wasm_memory64`).
  That raises every memory's virtual reservation, wasm32 ones included,
  without changing their bounds-check elision (a 4 GiB reservation already
  elided them), so it is set only when an M64 process exists.
- **clang 22 / wasm-ld:** `--target=wasm64 -fPIC` and `wasm-ld -mwasm64
  --experimental-pic -shared --shared-memory` link. Such a module imports
  `__memory_base` and `__table_base` as **i64** globals and the function
  table as a **table64** (`table i64 funcref`, `call_indirect` with an i64
  index).
- **So an M64 process's `ftab` is a table64,** and its tier-1 and fjit
  modules index it with i64 (one `i64.extend_i32_u` before each
  `call_indirect` through it). The tier-1 dispatch tables, which only tier-1
  modules use, stay 32-bit tables.

## 5. Choosing the flavour at exec (level 1)

In order, the first that answers wins:

1. **`gexc run --mem64` / `--mem32`**, and `?mem64=1` in `nt.html`.
2. **A learned record:** `<gex cache>/mem64/<content hash of the main
   image>`, written when an M32 process of that image overflowed (§6). The
   main image is the ELF executable or the PE exe that `gexc run` was given.
   The record says which limit it reached and when, so a stale one can be
   dropped.
3. **A heuristic (optional, off by default):** none of the cheap candidates
   are specific enough. x64 PEs are all LARGEADDRESSAWARE, and an ELF's
   `PT_GNU_STACK` says nothing. Leave it off unless one is found that picks no
   program that fits.

**Overflow detection** (both levels): an M32 process whose mapping request
fails for lack of address space (`mmap` → ENOMEM with no `MAP_FIXED`,
`NtAllocateVirtualMemory` → STATUS_NO_MEMORY on the range, not the commit
limit) while the high arena would have taken it. The kernel then writes the
record and logs one line:
`gex: <image> needs more than 4 GiB of address space; it runs with --mem64 from the next start`.
With level 2 it also switches instead of failing.

## 6. The live switch (level 2)

When an M32 process overflows and live switching is on:

1. **The overflowing thread** is in a syscall, a safepoint with its whole
   state in its context. Its result is "restart": PC goes back to the syscall
   instruction, so the allocation runs again after the switch.
2. **Stop the world.** Set a process-wide switch request. Every other thread
   leaves its wasm frames through the existing trap-out (`StopExecution` with
   `resume_at`) at its next safepoint. A thread blocked in a host wait (futex,
   NT wait, poll) is woken and leaves the same way, with its syscall
   restarted: a spurious wakeup is allowed for all of these, and a relative
   timeout restarts with its full length. The wait for all threads to arrive
   needs a new barrier in both shims.
3. **New memory.** Create the M64 shared memory and copy every mapped range
   (`k.mem.vmas()`, which is what fork already copies). Natively this is a
   copy of resident pages, about the 650 MiB committed for Unity, not 4 GiB.
   The kernel's allocators switch to the M64 layout. Mappings that exist stay
   where they are; new data goes high.
4. **New translations.** Each thread rebuilds its store (as a new thread
   does) against the new memory, with the M64 runtime modules. The fjit and
   tier-1 caches are keyed by flavour, so code retranslates lazily. Units load
   their M64 build if one is cached; otherwise that image runs on fjit until
   one is built. The ftab address map and the probe cache are emptied.
5. **Resume** every thread at its saved PC.

**What makes it hard:**

- **NT nested guest loops** (docs/nt-layer.md §5). A thread inside a
  win32u → user32 callback has a host frame of the outer syscall, and wasm
  frames below that, bound to the old instances. Such a thread cannot leave
  without abandoning the outer syscall. Rule: switch only when no thread is
  nested; otherwise record and fail as level 1 does. Unity overflows during
  startup, outside any window procedure, so this covers the games; a modal
  loop (MessageBox, a dialog, a window-move loop) would block a switch.
- **Units in flight.** A thread inside a unit function is not at a safepoint
  until it polls. Units poll at back-edges and calls (§4.3), so it arrives;
  the return-0 chain writes its state back on the way out.
- **The browser:** each thread is a Worker with its own instances. The switch
  is a respawn of every Worker against the new memory, with the shim's own
  data copied too, since the shim lives in the guest memory. The M64 shim is
  a different module, so the page loads it on demand.
- **Signals and timers** pending during the switch stay queued in the kernel,
  which is unchanged.

**Estimate (agent time):** level 1 in all tiers and both shims is about 2-3
days. Level 2 natively for Linux guests is about 1 day on top (the barrier,
the store rebuild, the restart semantics); NT adds about half a day (the
nested-loop rule, NT waits). The browser live switch (a Worker respawn and
the shim's own state) is about 1-2 days more. Recommendation: level 1
everywhere first, then level 2 natively, then level 2 in the browser if the
learned record is not enough there.

## 7. Each tier

- **tier-1 (`gex-emit`):** the resurrected `Context::memory64` paths. The
  guard (`guard_to_i32`, `emit_address`) becomes "push the i64" in M64. The
  statement lowering (`fmode`) is shared with fjit, so both get it at once.
- **gex-fjit:** emits through tier-1's lowering, so loads and stores follow.
  Its own address arithmetic (page checks against PAGE_GEN, FCACHE probes,
  ctx accesses) uses `ptr_ty`. Its module's memory import takes the flavour.
- **gex-cgen units:** the C header's `GEX_P`/guard macros compile to plain
  64-bit pointers. clang targets `wasm64-unknown-unknown` (wasm-ld supports
  memory64 and `-shared` PIC; its `__memory_base`/`__table_base` handling has
  to be checked for wasm64 PIC). `+mem64` goes in the unit key. Units are
  built on demand per flavour: `cgen-prefix` keeps building M32, and builds
  M64 with `--mem64`.
- **Providers** (`gex-cgen/providers/*.c`): compiled per flavour as units are.
- **Native shim:** `SharedMemory` with a memory64 type
  (`MemoryTypeBuilder::memory64(true).shared(true)` exists in wasmtime 27;
  to confirm in a smoke test), `Config::wasm_memory64(true)`, and
  `GuestMemory`/`GuestMem` already use u64 addresses.
- **Browser shim:** two builds (wasm32 and wasm64) from one source, as
  `build64.sh` did; the glue picks by flavour; JS views of a memory over 4
  GiB (`Atomics.wait` on a `BigInt64Array`, offsets as Numbers up to 2^53).
- **Kernel and NT layer:** already u64 throughout. Audit anything that keeps
  a guest address in a `u32`: the GL transport's client pointers, the NT
  32-bit paths (P32 processes never go M64, see below), the wayland pool
  views.
- **i386 guests (Linux and PE32)** cannot address more than 4 GiB, so they are
  always M32.

## 8. Risks

- **Speed of M64 code:** wasmtime gives a 64-bit memory explicit bounds
  checks. The 2026-08 numbers (§2) say neutral to 13% slower, more on some
  kernels. Only processes that need M64 pay it.
- **V8 memory64 + shared + 16 GiB:** support and commit behaviour have to be
  measured first; a browser that cannot do it keeps level 1 off.
- **wasm64 PIC in clang and wasm-ld** for units: least exercised path; if it
  falls short, M64 processes use fjit only until it is fixed (correct, slower).
- **Code in the high arena** (§3): fails loudly at first.
- **Two shims in the browser** double the shim build and its cache.

## 9. Order of work (each step gated by tests; shared files sequenced by the coordinator)

0. **Smoke tests, no shared code touched:** a wasmtime shared memory64 of 64
   GiB virtual instantiated by two threads; the same in V8 (Chrome) up to 16
   GiB, and whether growth commits lazily; clang `-target wasm64` `-shared`
   with a PIC module importing a 64-bit memory.
1. `gex-abi`: `MemFlavor`, `LIMIT64`, the high-arena bounds; kernel `mem.rs`
   and `nt/vm.rs` high arena behind the flavour. Tests: an x86-64 program
   that reserves 24 × 512 MiB and touches each (tests/x86-programs, run with
   `--mem64`), and the NT equivalent (tests/nt).
2. tier-1 M64 (resurrected), native shim memory64, `--mem64`. Gate:
   x86-programs and lockstep under `--mem64`; M32 unchanged (bit-identical
   modules for M32, checked by the cache key not changing).
3. gex-fjit M64. Gate: Tier 0 corpus under `--fjit --mem64`.
4. The learned record and overflow detection; tests/nt and the Unity games
   natively.
5. Units and providers in M64 (wasm64 clang), with `+mem64` in the unit key.
6. The browser: wasm64 shim, glue, `?mem64=1`, the learned record in the
   page's storage.
7. Level 2 natively (Linux, then NT), then in the browser if needed.

Owners: steps 0, 1, 4 and the Unity side are stream U's. Steps 2, 3 and 5
touch gex-emit, gex-fjit and gex-cgen (streams F and R), and step 6 the
browser shim; the coordinator sequences them.

## 10. As built (steps 1-3, native)

- **The flavour is per host process:** `gex_abi::set_mem_flavor` /
  `mem_flavor()`, set by `gexc run` (`--mem64`, `--mem32`, else the learned
  record) and `gexc build --mem64`. Everything that emits or links code
  reads it; nothing branches on it at run time. A wasm64 build (the browser
  shim, later) is M64 by construction.
- **M32 is byte-identical** (`tools/mem32-identity.sh`): every tier-1, fjit
  and cgen module of the Tier 0 corpus on ga64/gx64/gi386, and every unit
  object and module of four PE images. Unit keys hash gex-abi's and
  gex-cgen's *sources* (`gex-cgen/build.rs`), so any edit there changes them,
  M32 or not; no M32 key gains a flavour field.
- **Placement, refined:** anonymous mappings without `PROT_EXEC` go high;
  *file* mappings stay low whatever their protection (ld.so reserves a
  library read-only, then maps its text inside it), and so do anonymous
  mappings the dynamic loader asks for (an `mmap` whose pc is in ld.so's
  text at `INTERP_BASE`: glibc reserves `PROT_NONE` memory for an
  over-aligned library such as UnityPlayer.so). `MAP_FIXED` or `mprotect`
  that would put code above 4 GiB fails loudly (EACCES).
- **Code generation:** `FuncBody` address helpers that emit today's bytes
  for M32; the context stays an i32 everywhere (F_body and every host
  import keep their signatures) and is widened where it is an address; no
  bounds guard and no bulk-op clamp in M64 (the engine traps past the end,
  which the shim reports as SIGSEGV); `ftab` is a table64 with i64 indices,
  `__table_base`/`__memory_base`/`__stack_pointer` are i64; units are clang
  `--target=wasm64` with a substituted header. Providers are i386-only and
  never M64.
- **Native shim:** the engine gets memory64 and a 16 GiB static reservation
  only for M64; the guest memory is created at its full 16 GiB (a
  reservation; the high arena never grows it).
- **Validated:** every corpus program's tier-1, fjit and cgen module
  validates as M64; the corpus runs under `--mem64` with native output on
  tier-1 and `--fjit` (60/60); `tests/x86-programs/mem64-reserve.c` reserves
  12 GiB the way Unity does.
- **Not yet:** the browser (step 6), the live switch (step 7), executable
  pages above 4 GiB.

### State at 2026-10-04 (paused for a second reboot)

- **The sigsuspend fix is verified:** `tests/x86-programs/sigsuspend-thread.c`
  fails on the kernel before bdbe0ea8 (tier-1, `--fjit`, `--fjit --mem64`:
  "took the suspender's") and passes after. Muck now gets past the GC that
  hung: the player log reaches "Desktop is 800 x 600 @ 60 Hz" after
  "Completed reload, in 362 s" (about 15 min wall clock to the first frame,
  warm cache, with Xwayland and sh also running M64).
- **X without Xvfb:** Xwayland runs *inside the guest* on gex's compositor
  (`inguest.sh` below: `sh` starts `Xwayland :71 -geometry 800x600 -shm -ac
  -nolisten tcp` without `LD_LIBRARY_PATH`, waits for its socket, then execs
  Muck with `DISPLAY=:71`). Getting there needed three fixes, all committed:
  the fjit forgot nothing on `execve` (Xwayland exec'd from sh read its data
  as sh's read-only data: "initializing atoms"); `access(2)` ignored its
  mode (Xwayland chose root's /var/lib/xkb for xkbcomp output); native
  `execve` put ld.so at PIE_BASE+0x1000_0000 instead of INTERP_BASE, so under
  M64 glibc's reservation for UnityPlayer.so went above 4 GiB. Xwayland's
  lock and socket node are host files (/tmp/.X71-lock,
  /tmp/.X11-unix/X71); the script removes only its own.
- **The new blocker:** right after the first frame, tier-1 (block
  translation, not fjit) emits a module wasmtime rejects under M64: "type
  mismatch: expected i64, found i32" (runs x3 at 0x6b379799 and x4 at
  0x6b3792c9, same function region). The module is saved:
  `work/unity-resume/6b3792c9.wasm` (from `GEX_DUMP_BAD_WASM=DIR`, added
  for this). It is an M64 address that tier-1's emitter leaves i32 on some
  path the corpus does not cover. No fps number yet: no frame after the
  first.
- **Resume:** the scripts, gex-gl x86-64 build and the warm fjit cache
  (6.4 GB) are copied to `/home/velzie/src/noita-wasm/work/unity-resume/`
  (`muck.sh`, `inguest.sh`, `gl64/`, `xdg/`; the scratch originals are under
  /tmp/claude-1000/.../scratchpad/unity, paths inside the scripts point
  there). Steps:
  1. `wasm-tools print` / `validate` the saved module (cargo install
     wasm-tools if missing), find the instruction at offset ~1156 and the
     gex-emit M64 address helper that misses it; add the guest instruction
     to the M64 corpus; rebuild.
  2. Rerun `GEX_FJIT_CACHE=1 GEX_DUMP_BAD_WASM=DIR muck.sh TAG 3000 --fjit
     --mem64` (cores 1,7) to the menu UI; screenshot to
     work/games/screens/, fps (`GEX_FPS=1`).
  3. Then halflife.sh's native mode to the in-guest Xwayland, generally
     (better as a `gexc run` option that starts Xwayland as a guest process
     than as a wrapper script, so the program stays the run's image for the
     learned-M64 record and units); then step 6 (browser) and the live
     switch; then Rain World and Hollow Knight (F has pinned the D3D DLLs in
     the gex-layout prefix, sha256 63b13c67..., with units in F's
     cgcache-cg12).

### State at 2026-10-04, later (Muck at its menu)

- The rejected module was tier-1's self-modification guard
  (`emit_guard`): it loaded the code bytes at an `i32.const` address. It now
  uses `FuncBody::addr_const`, which emits the same bytes for M32
  (`tools/mem32-identity.sh`: identical apart from five gx64 cgen modules,
  whose output depends on how many CPUs the build sees; old and new
  binaries agree on 2 CPUs and on 4). Test:
  `tests/x86-programs/mem64-smc.c`.
- Muck then reached its menu (docs/games.md, Unity status), after three
  fixes not specific to M64: the function JIT leaves writable-and-executable
  code to tier-1, tier-1 translates again when its page was retired
  mid-translation, and a worker thread's `exit_group` ends the process.
- **Next:** `tests/games/halflife.sh`'s native mode to an Xwayland on
  gex's compositor. Half-Life is i386 and Xwayland x86-64, and one gexc
  process decodes one of the two (`gex_xlate::set_compat` is per process,
  and a guest fork is a thread of the same host process), so the
  in-guest Xwayland Muck uses needs either a per-thread decode mode or a
  second gexc running Xwayland with guest TCP `listen` (the kernel's AF_INET
  sockets only `connect` today). After that: the browser (step 6), the live
  switch (step 7), and Rain World and Hollow Knight.

