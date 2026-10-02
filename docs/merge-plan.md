# Merging pewasm into gex

Written 2026-09-30. This is the plan for folding pewasm (the static PE → Wasm
recompiler in `../pewasm`) into gex, so that one system has both projects'
strengths and pewasm can be retired.

- **gex brings the runtime.** A program runs instantly: code is translated
  as it is reached, including code generated at runtime, and the Linux
  personality (fork, exec, signals, a VFS, a compositor) is real.
- **pewasm brings standalone, customizable output.** A generated project
  needs no runtime beyond what the program uses. Libraries can be swapped for
  native Wasm builds (the Noita port binds SDL2, Lua, FMOD and Steam this
  way), individual functions can be replaced, and whole guest functions are
  compiled as C by clang. That last part is where its speed comes from.

The end state: gex, plus a **function-level AOT tier** that uses pewasm's
code-generation scheme, plus **`gex-eject`**, which turns what a gex session
learned into a pewasm-style standalone project.

Related documents: `PLAN.md` (gex design), `OPTIMIZATIONS-MISSING.md` (gex
measurements), `../pewasm/docs/optimization-plan.md` (pewasm's optimization
work; its "Direction" section, §12 and §16 were updated for this merge).

---

## 1. Decisions already made

| Question | Decision |
| --- | --- |
| Which project survives | gex. pewasm is retired at the end (§6, M8). |
| Lifters and IR | gex's Rust lifters (AArch64 and x86-64 on `master`; i386 compatibility mode still on the `x64` branch) and GIR. pewasm's SLEIGH/p-code lifter (`cpplift`) is not carried over. It may stay as an extra oracle for diff-fuzzing. |
| ABI and memory model | gex's: guest virtual address equals the linear-memory offset, a context struct per thread, the dispatch directory in guest memory, and the engine's own bounds check with the poison top page. pewasm's arena offset, `MEMR*/MEMW*` checks and read-only-page table are dropped. |
| AOT code generation | Guest functions emitted as C and compiled with clang (pewasm's scheme). No LLVM in the browser. A Binaryen-based backend is a possible later option for runtime use, and must be measured first. |
| Ejected builds and code nobody has seen | Fail loudly by default, as pewasm does. Including a JIT fallback (the lifter plus the tier-1 emitter) is a per-build flag. |
| Windows programs | A native NT-syscall and win32u layer in gex-kernel, with Wine's PE DLLs unchanged on top. Plus per-DLL native redirects for hot paths (opengl32, dsound, dinput, CRT). Not a wholesale port of pewasm's Win32 host. |
| Profiles | Guest-level, keyed by (object content hash, offset). A profile only ever changes speed, never correctness. |
| Win32u callbacks (win32u calling user32 in the middle of a syscall) | **A nested guest loop** (option (a) in `docs/nt-layer-scope.md` §7). The kernel handler re-enters the execution loop recursively. Windows has no fork/exec, so gex's flat-stack invariants aren't at risk. Same nesting model as the function tier. |
| Wine version | Pin **Wine 11.15** and vendor its PE DLLs. win32u sub-codes, callback ids and the shared-memory layouts are tied to one release. |
| 32-bit Windows syscalls | Implement the 32-bit NT ABI thunks natively in gex-kernel, rather than running Wine's wow64 DLLs. |

---

## 2. End-state architecture

> **Revised 2026-10-02: see §9.** Every tier now shares one call-returning
> ABI and function-sized code; the per-block tier-1 and the splice/nest
> bridge below survive only as the region tier's last resort.

```
 ELF closure (gex-ld)          PE image + DLLs (gex-ld, PE support: M6)
          \                    /
           lifters: AArch64 | x86-64 | i386         (gex-lift, gex-lift-x86)
                     |
                    GIR: blocks  ──►  whole functions (M1)
                   /        \
       tier-1 (runtime)      function tier (AOT, M2)
       one wasm fn / block    one C fn / guest fn → clang → wasm
       return_call chaining   locals across blocks and calls, inlining, PGO
                   \        /
          dispatch table: every entry address resolves to one or the other
                     |
       runtime: full gex (JIT, fork, signals, compositor)
            or: ejected slim runtime (only what the manifest says was used)
                     |
       personalities: Linux (gex-kernel)  |  NT/win32u (gex-kernel, M6)
```

The two tiers coexist per address, as `PLAN.md` §5.5 already plans for
tier-2. Tier-1 is always correct for any entry address. The function tier is
faster wherever its assumptions hold, and hands control back to tier-1
wherever they don't (§4).

---

## 3. What happens to each pewasm component

| pewasm component | Fate |
| --- | --- |
| `cpplift`, `lift64`, SLEIGH decoding | Retired. The x86 recognizers (jump tables, MSVC SEH4/EH/throw data, vtables, CRT init tables, guard tables) are ported to gex discovery as *discovery aids* for PE images (M6). |
| C emission scheme (`emit_*.cc`): registers as C locals, write-back at calls and exits, pending-jump trampoline | Re-implemented from GIR in the function tier (M2) |
| Landed optimization passes | Re-implemented on GIR from the specs in pewasm's optimization plan §16: jump-table cases kept inside the function, re-entry pruning, flag elision across calls and returns, clean-spill elision, profile-guided devirtualization and leaf inlining, the inline dispatch-cache probe |
| `build.py` (content-addressed object cache, PCH, batched compiles, local-count check) | Ported to the `gexc build` / eject build (M2, M5) |
| Import binding by weak placeholder plus strong override (`imp_slot_*`, `project.json` report) | Becomes eject's override mechanism (M5), generalized from IAT slots to PLT/GOT symbols and any function |
| `src/x86` Win32 host (~19.5k lines, ~807 imports) | Not ported as code. It is the reference for the NT/win32u layer and the per-DLL redirects (M6). Lock-free critical sections and the ConcRT port carry over as techniques. |
| `src/x64` D3D11 → WebGL2, DXBC translator | Deferred. Revisit when a 64-bit D3D11 target matters. |
| WGL bridge to Mesa (`host_wgl.c`), in-memory shader cache (`src/mesa/shader_cache.c`), Mesa optimization overlays | The shader cache links into gex's Mesa-wasm builds now (it wraps `gex_wgpu_spirv_to_wgsl`). The opengl32 redirect in M6 borrows the WGL logic. |
| `--runtime-x86` (p-code interpreter for runtime code) | Retired. gex's tier-1 translates runtime code. |
| `bench/` corpus and runner, soundness corpus | Shared from M0 on, built for gex targets as well |
| Noita port (`port/`: SDL2/Lua/FMOD/Steam bindings, null GL, frame-time tools) | Moves to gex as eject overrides in M7 |

---

## 4. The hard part: the function tier inside a runtime that assumes flat stacks

> **Revised 2026-10-02: see §9.** The invariants below still hold; nests,
> leaves and per-slot dispatch are replaced by one call-returning ABI.

Tier-1 never nests guest calls on the Wasm stack, and gex relies on that for
`fork`, `execve`, signal delivery, thread switches and `longjmp`. Function-tier
code breaks it deliberately: a guest call becomes a Wasm call, and live guest
state sits in Wasm locals. That is where the speed comes from. Everything
below exists to keep the runtime correct anyway.

### 4.1 Invariants the function tier keeps

1. **Guest-visible state is architecturally correct at every call boundary
   that can reach unknown code**: syscalls, indirect calls without a guarded
   known target, calls into tier-1, signal safepoints. "Guest-visible" means
   the guest stack contents (including the return address, x30/LR on
   AArch64), callee-saved registers, and memory. Values the function tier
   keeps only in locals (promoted stack slots, parameters of internal fast
   calls) are written to their architectural home before such a call and
   reloaded after it.
2. **Every function-tier function is also enterable through the dispatch
   table** with the uniform `(ctx) -> ()` signature. The fast internal
   interfaces (`f_i(ctx, args…)`) are used only between function-tier
   functions whose relationship was proven (pewasm plan §6).
3. **Any address the function tier doesn't own is tier-1's.** Interior
   entries, undiscovered code and code modified at runtime all fall back to
   tier-1. The fail-loud equivalent is only for ejected builds without JIT.

### 4.2 Unwinding function-tier frames (deoptimization)

Some events need the guest thread's *entire* state in the context and on the
guest stack, with no Wasm frames below: `fork`, `execve`, a thread migrating
between Workers, a signal handler that `siglongjmp`s out, and guest `longjmp`
or C++ unwinding across function-tier frames.

The mechanism is one Wasm exception, `gex_unwind`:

- Each function-tier call site that can reach such an event gets a landing
  pad. The pad writes that frame's live locals to their architectural homes,
  records "resume at this guest PC in tier-1", and rethrows.
- The outermost catcher (the host loop) finds the guest state complete and
  resumes in tier-1 at the innermost resume PC. Each caller's remaining body
  then runs in tier-1, because each caller's return address is already real
  guest state.
- Call-graph summaries (`may_unwind`) decide which call sites need pads, the
  same way pewasm's summaries decide which spills are needed. Code whose
  whole call tree can't reach an unwinding event gets none.

**Returns are checked.** After every function-tier call, compare the guest SP
and PC with the expected values: the frame's SP, and the return address the
caller pushed or put in LR. On a mismatch, the guest unwound or switched
stacks underneath us (`longjmp`, coroutines, a `ret` used as a computed jump),
so throw `gex_unwind` and let tier-1 continue from the real state. This
generalizes pewasm's `recomp_jmp_pending` check.

**Cost:** pads are code size, not runtime; the return check is one compare
per call. Price both with ballast before tuning (`OPTIMIZATIONS-MISSING.md`,
"The indirect-dispatch path is not a cost").

### 4.3 Signals, threads, runtime code

- **Signals** are delivered at safepoints, which in the function tier are
  loop back-edges and calls that may unwind. A handler runs nested on the
  current Wasm stack. If it returns normally, nothing else happens. If it
  leaves by `siglongjmp`, the return check or `gex_unwind` handles it.
- **Threads** use gex's Worker-per-thread model and `Atomics.wait` for
  blocking, so a function-tier frame blocking in a syscall is fine. Migrating
  a thread between Workers goes through §4.2.
- **Code modified at runtime** uses gex's existing machinery for pages that
  are both writable and executable (on `master` since the Qt/KDE bring-up, d388bec). A function whose pages
  change is invalidated: dispatch entries revert to tier-1, and frames of it
  already on the stack finish in tier-1 when they unwind or return into it.
- **Stack scanners** (conservative GCs that read callers' frames by address
  range) count as unknown code under invariant 1. Promoted slots are written
  back before any call that may reach one. The soundness corpus has a case
  for it (pewasm plan §2A.4).

---

## 5. Validation

This plan relies on four checks, not on review.

1. **The shared corpus.** pewasm's `bench/` (portable C behind
   `harness/kernel.h`), built for every target: i686 PE (pewasm, until M8),
   x86-64 and AArch64 musl (gex), i386 Linux (gex compatibility mode), and
   `emcc -O2` as the baseline. One table compares all of them. Every run also
   checks its output checksum.
2. **Tier-1 is the reference implementation.** Run the same program with the
   function tier on and off, record at gex-kernel's syscall boundary, replay,
   and compare state at each syscall. On a mismatch, bisect over which
   functions use the function tier. gex has two tiers, which makes this
   possible without any outside oracle.
3. **The soundness corpus** (pewasm plan §2A.4): one program per bail-out
   condition, plus a mutation check. Deliberately disable each safety
   condition in a debug build; some test must fail.
4. **Instruction-level diff-fuzzing**: gex's `tests/diff-fuzz` (AArch64,
   QEMU oracle) and `tests/diff-fuzz-x86` (x86, with native execution as a
   possible oracle).

Measure with gex's rules: CPU time, paired rounds, the median of per-round
ratios, alternating order, a null test and a control kernel. Price every
optimization with ballast before building it.

---

## 6. Milestones

Agent-time estimates. Each milestone ends with the shared corpus table, the
soundness corpus, and whichever demos it touches.

### M0: Prerequisites (1–2 days)

- The x86-64 frontend (`gex-isa-x86`, `gex-lift-x86`, `gex-kernel/src/x86.rs`)
  and `tests/diff-fuzz-x86` are already on `master`. Still only on `x64`:
  i386 compatibility mode (`gex-kernel/src/i386.rs`, 4f2d4e1) and the Wine
  integration. Upstream i386 compatibility mode **without** the Wine pieces:
  PE32 programs (Noita) and the corpus's i386 target need it. Leave the Wine
  work on `x64`. M6 replaces it rather than building on it, apart from
  individual generic kernel fixes worth cherry-picking (mmap placement,
  `POLLHUP`, `clock_nanosleep`, SCM_RIGHTS receipt).
- Build the shared corpus for gex targets (pewasm plan §12 item 2), and get
  the first comparison table: gex tier-1 / emcc against pewasm / emcc on the
  same kernels.
- Write the pass specs in pewasm plan §16 for everything that has landed
  there.
- Link pewasm's shader cache into gex's Mesa-wasm build and measure it.

### M1: Function recovery and whole-function GIR (2–3 days)

- Function boundaries from symbols, `.eh_frame` FDEs, call targets and
  (later) PE `.pdata`/exports. Jump-table recovery for both ISAs, so a
  function's successor sets are complete. Otherwise liveness gives up with
  "assume everything live", as `OPTIMIZATIONS-MISSING.md` #53 notes.
- A function-level GIR: a CFG over lifted blocks, SSA across blocks for
  registers and flags, per-op guest PC, and memory effects.
- Whole-program summaries over the call graph (inputs, outputs, preserved
  registers, stack effect, `may_unwind`, `may_leave_pending`), iterated to a
  fixpoint over SCCs. This is `OPTIMIZATIONS-MISSING.md` #51–55 and #57.

### M2: The function tier (3–5 days), the speed-parity milestone

- Emit C per recovered function: registers as locals, state written to the
  context only where §4.1 requires, landing pads (§4.2) as C `try`/`catch`
  via Wasm exceptions or an explicit unwind flag (measure which).
- Compile with clang to wasm32 and link. Function entries go into the
  dispatch table next to tier-1 blocks, using the module linkage gex already
  has (`gex-emit` `Linkage`/`merge`).
- Port pewasm's landed passes from the §16 specs, in order of measured value:
  - re-entry pruning, which here becomes "interior entries go to tier-1";
  - flag elision across calls and returns;
  - jump-table cases kept in the function;
  - clean-spill elision;
  - profile-guided devirtualization and leaf inlining (depends on M3);
  - clustering and build caching.
- **Acceptance:** on the shared corpus, gex function tier / emcc is at or
  below pewasm / emcc on the same kernels (pewasm today: 2.93× emcc geomean
  on Tier 0 with SSE2, lower after its latest passes; use the value current
  at M2). The soundness corpus passes, and so does the tier-1 lockstep replay
  from §5.
- Then the larger items from pewasm's plan, implemented here instead of in
  `cpplift`: frame promotion (plan §5), sound interface recovery (§6), x87
  and SSE work in `gex-lift-x86` (§9). Each is priced first.

### M3: Session manifest and PGO (1–2 days)

- `GEX_PROFILE=1` writes per-object profiles keyed by (content hash,
  offset):
  - exact block and function entry counts (not the aging `GEX_HOT` table);
  - the top-4 targets at each indirect site;
  - `ret`s that went to an unexpected address;
  - interior entries;
  - syscalls per function.
- The session manifest adds the loaded closure (objects and hashes), the
  syscalls used, files opened and environment.
- `gexc build --profile` uses it to choose the function-tier set, guarded
  devirtualization targets, inlining budgets, branch weights and hot/cold
  splitting. Profiles for shared objects (libc, libstdc++) merge across
  programs.

### M4: `gex-eject` v0 (1–2 days)

- `gex-eject <session>` writes a project containing:
  - the closure's AOT modules (tier-1, plus the function tier once M2 lands);
  - a launcher;
  - the runtime;
  - assets from the manifest's file list.
- **Library replacement** at the PLT/GOT boundary. gex-xlate's `intrinsics`
  already maps guest functions to wasm code by address. Generalize it to
  "symbol → user-supplied wasm function" (e.g. an Emscripten-built SDL2).
  Resolved GOT slots are prebound (`OPTIMIZATIONS-MISSING.md` #59–60).
- Reproducible: re-ejecting the same session with the same inputs gives the
  same project.

### M5: `gex-eject` v1: customizable and slim (2–3 days)

- **Overrides:** a user C file that defines a function replaces the lifted
  one. Functions are keyed by symbol, or by (object hash, offset) plus a
  hash of the original bytes, so a changed binary reports stale overrides
  instead of misbehaving. User files live apart from generated ones and
  survive a re-eject, like pewasm's `src/*.c`.
- **`gex-eject decompile <fn>`** writes the function's generated C as an
  editable starting point. It becomes more readable as frame promotion and
  interface recovery land.
- **Slim runtime:**
  - gex-kernel subsystems behind cargo features (VFS backends, processes and
    fork, sockets, ptys, the compositor, GL);
  - the syscall table trimmed to the manifest's set, with everything else a
    stub that fails loudly;
  - single-process builds when the program never forks.
- **`--jit-fallback`** (off by default): ship the lifter and the tier-1
  emitter, so unseen code gets translated instead of trapping.

### M6: Windows programs (10–14 days, can run in parallel with M3–M5)

Scoped in `docs/nt-layer-scope.md` (stream S8). Noita + winemine + a small GUI program need 68 of Wine's 270 NT syscalls, 98 of its 484 win32u calls and 122 of its 308 wineserver requests. The estimate is ~21–29k lines of new Rust for Noita and ~31–42k with winemine and notepad (gex-kernel is 17.5k today), so the original 5–8 days was too low. Build it in the order that doc's §8 recommends: x64 console → threads, sync, exceptions → 32-bit ABI → USER core + minimal GDI for Noita's menu → rest of Noita → winemine → notepad. Noita's GL goes through opengl32's unixlib, not the NT boundary, so the opengl32 redirect covers it. The helper processes (services, winedevice, plugplay, rpcss, explorer) aren't needed if the redirects and stubs are done right.

- **PE loading in gex-ld:** relocations, imports, delay imports, exports,
  TLS callbacks, resources, forwarded exports. The logic can be ported from
  pewasm's `tools/pe64.py`.
- **Discovery aids for PE:** `.pdata`, and pewasm's MSVC recognizers ported
  to Rust. Only the ones the M2 lockstep replay shows are needed.
- **The NT layer in gex-kernel:**
  - the NT syscalls Wine's PE `ntdll.dll` actually issues (measured with
    `--strace` on winemine and one real app before implementing);
  - the object/handle/wait/message-queue model that wineserver provides
    today;
  - the win32u subset those programs need, with windows mapped directly to
    gex-wayland surfaces.

  This retires unix `ntdll.so`, `win32u.so`, winewayland and the wineserver
  process. That removes the `x64` branch's layout patches (`limit_4g`), the
  shared-memory coherence workaround, file-descriptor passing over sockets
  (SCM_RIGHTS) and the ~3 GiB memory footprint.
- **Per-DLL native redirects:** opengl32 → gex GL, dsound/winmm → WebAudio,
  dinput/xinput → Gamepad API, hot CRT functions. They reuse pewasm host
  logic.
- 32-bit programs run through i386 compatibility mode.

### M7: Noita on gex (2–3 days)

- Run Noita (PE32, msvcr120/msvcp120) on gex through M6, then eject it with
  the port's replacements expressed as eject overrides: SDL2 → Emscripten
  SDL2, lua51 → native Lua, FMOD → FMOD's HTML5 build, steam_api stubs,
  null GL for headless benchmarking.
- **Acceptance:** Noita's headless world-fps and browser frame time are at
  or above the pewasm port's (1.00× realtime at the 60 fps cap as of
  2026-09-30).

### M8: Retire pewasm (0.5 days)

- Archive the repository. Move the corpus, soundness tests, measurements and
  specs into gex.
- Optionally keep p-code as an extra diff-fuzz oracle.

**Critical path:** M0 → M1 → M2 (speed parity) → M3 → M4/M5. M6 can start
after M0 in parallel, since it mostly touches gex-ld and gex-kernel. M7
needs M2, M5 and M6.

---

## 7. Risks and open questions

- **Unwinding (§4.2) is the least proven part.** Build it against the
  soundness corpus first: `longjmp` across frames, C++ exceptions, signal
  handlers that `siglongjmp`, `fork` with function-tier frames on the stack,
  coroutines/`swapcontext`. Keep the function tier opt-in per object until
  those pass.
- **Build time and module size.** Whole closures through clang are slow.
  Profile-selected hot sets (M3) keep the function tier small, pewasm's
  build caching keeps rebuilds incremental, and cold code stays tier-1.
- **One guest architecture per build.** gex selects the ISA with a cargo
  feature, so one binary serves one guest. Mixing x86-64 and AArch64
  programs in one session needs either both lifters in one build or
  per-process runtimes. i386 already runs inside the `x86` build.
- **win32u is large.** Scope it by measurement: log which `NtUser*`/`NtGdi*`
  calls winemine and one real app make before committing to the NT layer's
  shape.
- **Engine features.** Wasm exceptions, tail calls and (optionally)
  multivalue must be available in every target browser, or have a fallback
  build.
- **Ejected output contains translated third-party code.** Distributing an
  ejected build is the user's licensing responsibility. The tool should say
  so and never publish anything itself.

---

## 8. Fan-out: how the work is split across agents

The milestones in §6 are ordered by dependency. Many of them don't depend on
each other at all, so they can run at the same time. This section splits the
work into **streams**. Each stream has one owner agent, one branch, a set of
files it may edit, and a gate it must pass before its work is integrated.

### 8.1 Ground rules

- **Canonical checkout:** `~/src/noita-wasm/gex`. It stays on `master`,
  fast-forwarded to `origin/master`. Nobody commits there directly.
- **Integration branch:** `merge/main`, cut from `origin/master`. Each stream
  works on `merge/<stream>` in its own worktree at
  `~/src/noita-wasm/wt/<stream>` (`git worktree add`). A stream merges into
  `merge/main` when its gate passes. The coordinator rebases `merge/main`
  onto `origin/master` daily. Nothing is pushed, and nothing lands on
  `master`, without the user.
- **Coordinator:** the session that wrote this plan (`noita-wasm-51`). It
  owns this document, the status file, `merge/main`, and the decisions that
  cross streams: changes to `gex-abi`, to GIR's core types, and to the
  interface between two streams. A stream that needs one sends the
  coordinator a message. It does not make the change itself.
- **Status:** `~/src/noita-wasm/MERGE-STATUS.md`. It sits outside the
  repositories so there are no branch conflicts. Each stream edits only its
  own row: state, last commit, last measured result, blockers.
- **Working rules for every stream:**
  - Measure with gex's rules: CPU time, paired rounds, the median of
    per-round ratios, a null test, a control kernel.
  - Price an optimization before building it.
  - Unsupported things fail loudly.
  - Record rejected experiments, with their numbers, in the stream's notes
    section of the status file.
  - Don't edit files owned by another stream.
- **Agent form:** multi-day streams run as separate Claude sessions, started
  by the user with the kickoff prompt in §8.5, so the user can see and steer
  them. Short, bounded tasks (S8, S9) run as subagents of the coordinator.

### 8.2 Streams

| Id | Stream | Milestone | Owns (may edit) | Needs | Gate to integrate |
| --- | --- | --- | --- | --- | --- |
| **S1** | Benchmark baseline and corpora | M0 | `pewasm/bench/`, `pewasm/tests/soundness/` (the corpus moves into gex at M8) | nothing | First table: gex tier-1 (AArch64 and x86-64 musl) / emcc next to pewasm / emcc on Tier 0, built from the canonical checkout. Then Tier 0 grows to ~25–30 kernels, Tier 1 suites, and the soundness corpus with portable cases. |
| **S2** | Function recovery and whole-function GIR | M1 | `gex-ir` (new function-level types; changes to core types go through the coordinator), `gex-xlate` (discovery, a new `func.rs`, summaries) | nothing | For the corpus binaries and gzip/perl: every function found, jump-table successor sets complete, a function CFG with SSA across blocks that prints and round-trips, call-graph summaries converged. `cargo test` green, diff-fuzz unchanged. |
| **S3** | C backend plumbing | M2 part 1 | a new crate `gex-cgen` (GIR → C), the `gexc build` flag that enables it, clang and wasm-ld invocation, linking clang-built functions into gex modules and the dispatch table, a C header generated from `gex-abi` | S2's API from ~day 2; until then it works on blocks | **Phase A (starts now):** every tier-1 block emitted as a C function with the uniform `(ctx)` signature, chained with `musttail`, compiled by clang, linked in, and passing the corpus checksums. That proves the plumbing and prices clang against gex's own emitter on identical input. **Phase B:** whole functions from S2's IR, registers as locals across blocks, invariants §4.1.1–3. Gate: corpus checksums and the tier-1 lockstep comparison. |
| **S4** | i386 compatibility mode upstream, without Wine | M0 | `gex-kernel` i386 parts, 32-bit mode in `gex-lift-x86`, plus cherry-picks from `x64` | nothing | Static i386 musl binaries (the corpus's i386 target) and busybox i386 run on `merge/main`. No Wine code comes along. The generic kernel fixes worth taking (mmap placement, `POLLHUP`, `clock_nanosleep`, SCM_RIGHTS receipt) land as separate commits with tests. |
| **S5** | PE loading in gex-ld | M6 part 1 | a new `gex-ld/src/pe.rs`, PE support in `gexc closure` | nothing | pewasm's MinGW and MSVC fixtures (`pewasm/examples`, the G1/G2 corpus builds) load, relocate, and resolve imports and exports. Delay imports and TLS callbacks work. The layout matches pewasm's `project.json` for the same inputs. Imports resolve to a placeholder table that fails loudly (no NT layer yet). |
| **S6** | Profiling and session manifest | M3 | the profiling hooks in `gex-emit/tier1.rs` (only that feature), a new `gex-xlate/src/profile.rs`, the manifest writer in `gexc` | nothing | `GEX_PROFILE=1` writes exact per-object counts, top-4 indirect targets per site, unexpected `ret`s and interior entries, keyed by (object hash, offset). Overhead is measured, the format is documented, and the manifest also lists the closure, syscalls and files opened. |
| **S7** | Unwinding and deoptimization (§4.2) | M2 part 2 | the host loop in `shims/native` and `shims/browser`, `gex-kernel` hooks for fork, exec and signals, the unwind support in `gex-cgen` (coordinated with S3) | S3 phase B | The soundness corpus's `longjmp`, C++ exception, `siglongjmp`-from-handler, fork-with-frames and `swapcontext` cases pass with the function tier on. |
| **S8** | NT/win32u scoping (research) | M6 part 2 | a new doc, `docs/nt-layer-scope.md`, only | an `x64` worktree | A list, with counts, of the NT syscalls and `NtUser*`/`NtGdi*` calls that winemine and one real app make under the current Wine setup, grouped by subsystem, with a size estimate for the native layer. Runs as a subagent. |
| **S9** | Small carry-overs | M0 | gex's Mesa-wasm build scripts | nothing | pewasm's SPIR-V → WGSL shader cache linked in, and its effect measured. Runs as a subagent. |

Later streams, started as earlier ones finish:
- **S10: porting passes** onto the function tier, from pewasm's §16 specs. Up to two agents, split by pass, both inside `gex-cgen` and the GIR passes. Needs S3 phase B.
- **S11: gex-eject** v0 and then v1 (M4, M5). Needs S6, and S3 for v1.
- **S12: the NT layer and per-DLL redirects** (M6). Needs S5 and S8.
- **S13: Noita on gex** (M7). Needs S11 and S12.

### 8.3 Waves

| Wave | Streams running | Starts when | Ends when |
| --- | --- | --- | --- |
| 1 | S1, S2, S3 (phase A), S4, S5, plus subagents S8 and S9 | now | S1's first table exists; S2's function-IR API is merged; S3 phase A passes; S4 and S5 are merged |
| 2 | S3 (phase B), S6, S7, S1 (corpora), S12 design | end of wave 1 | the function tier passes corpus checksums, the lockstep comparison and the unwinding cases |
| 3 | S10 (×2), S11, S12, S1 | end of wave 2 | M2 parity (gex function tier / emcc ≤ pewasm / emcc), eject v1, a PE hello world and winemine on the NT layer |
| 4 | S13, M8 | end of wave 3 | Noita on gex at or above the pewasm port; pewasm archived |

That's at most five long-running sessions at once, plus the coordinator.

### 8.4 Interfaces agreed up front

These decisions let the streams start without waiting on each other. Any
change goes through the coordinator.

- **S2 → S3:** the function-level GIR API. S2 publishes a first cut (the
  types and a printer) on `merge/function-ir` by day 1 or 2, even before
  discovery is complete. Until then S3 works only on blocks.
- **S3 ↔ tier-1:** function-tier entries use the same `(ctx: i32) -> ()`
  signature and the same dispatch-table slots as tier-1 blocks, and read the
  context layout through the header generated from `gex-abi`. Nothing in
  `gex-abi` changes in wave 1.
- **S6 → S3/S10/S11:** the profile format is keyed by (object content hash,
  offset), which is how gex already names code. The profile is advisory:
  consumers must stay correct whatever it contains.
- **S5 → S12:** PE imports resolve to entries in a placeholder table that
  fails loudly. S12 later fills that table from the NT layer and the
  redirects.
- **S1 → everyone:** `pewasm/bench/build.py --gex …` and
  `bench/gex-run.mjs` are the one way the corpus is built and run. Streams
  report results as a row in the status file, never as a private script.

### 8.5 Kickoff prompt for a stream session

```
You own stream <Sx> of the pewasm → gex merge.
Read ~/src/noita-wasm/wt/main/docs/merge-plan.md (all of it, especially §4 and §8)
and ~/src/noita-wasm/MERGE-STATUS.md.
Work in the worktree ~/src/noita-wasm/wt/<stream> on branch merge/<stream>,
created from merge/main. Edit only the files §8.2 says <Sx> owns. For anything
else, message the coordinator session `noita-wasm-51`.
Your gate is the "Gate to integrate" cell of your row in §8.2. When you pass
it, message the coordinator with the commit to merge.
Update your row in MERGE-STATUS.md after every meaningful step: state, last
commit, last measured result, blockers. Record rejected experiments with
their numbers.
Measure with gex's A/B rules and price optimizations before building them.
Don't push, don't touch master, don't edit another stream's files.
```

---

## 9. Revision (2026-10-02): one call-returning ABI for every tier

**Status:** adopted direction, at the user's request after the Noita
measurements below. It replaces the splice-and-nest design of §2 and §4 for
anything function recovery covers. §4's invariants (state correct at unknown
boundaries, patched code never runs) still hold; how they are met changes.

### 9.1 Why

Noita natively (i386 PE, staged Wine prefix, same scene; S3, 2026-10-02):

| | world-up | fps, first 300 s | fps, steady | peak RSS |
|---|---|---|---|---|
| tier-1 only (`--no-cgen`) | 207 s | 16.2 | 19.1 | 5.3 GB |
| units, before the call-out | 308 s | 13.8 | 24.2 | 8.6 GB |

The function tier helps steady state (+27%) but costs load time and 3 GB,
and it gets there through machinery that exists only to bridge two
incompatible execution models:

- **tier-1 never returns.** A block is a wasm function, blocks `return_call`
  each other, and every non-constant transfer, including a guest `ret`, goes
  through a per-page slot table (one slot per 4 bytes of code on x86).
- **the function tier returns.** A guest call is a wasm call.

Bridging them needs nests, roots, leaves, predicted returns, call-outs,
shared-slot bodies, page checks and stale-module retranslation. Noita pays
6.6M switches between the two per second, keeps about 281k tier-1 blocks
next to the units (the same code twice), and gives every thread its own
dispatch and body tables of about 4.8M entries each. Browser Workers cannot
share a wasm table, so those per-thread copies are inherent there.

pewasm, which has one model (whole functions, direct calls, one table for
indirect calls), runs the same program at 60 fps in a browser on a 2 GB heap.

### 9.2 The design

**One ABI.** Every guest function, whichever tier runs it, is a wasm
function of one signature that returns to its wasm caller when the guest
returns to its guest caller. The return is checked (§4.2): the callee
compares the guest return address and SP with what its caller set up; on a
mismatch (longjmp, a stack switch, `ret` used as a jump) it unwinds, and the
code that caught the unwind resumes at the real guest state.

**Code is function-sized in every tier.**

1. **The function JIT (new; replaces tier-1 for recoverable code).** On the
   first call to an address with no code, recover the function that starts
   there (S2's recovery, one function at a time, summaries cached), and emit
   one wasm function directly from GIR, with no clang:
   - blocks as a `loop` plus `br_table` state machine inside the function
     (no relooper needed, cheap to emit);
   - guest registers in wasm locals across blocks, written back only where
     §4.1 requires;
   - calls as wasm calls (see below), and returns as wasm returns.
   The emitter can start from gex-cgen's GIR → C lowering, which already
   makes every decision this needs, emitting wasm instead of C.
2. **The AOT tier (clang) is the same ABI with better code.** Whole-image
   units stay; the builder from S3 stays. A function with an AOT body never
   gets a JIT body.
3. **The region tier (fallback).** Code that is not function-shaped: jumps
   into the middle of a function, runtime-generated code (LuaJIT's mcode),
   code recovery gives up on. A region starts at the target, is translated
   like a function, and follows the same call/return contract. Today's
   call-out is the prototype of this. Block-at-a-time translation survives
   only here, as the last resort.

**Calls.**

- Within a module: wasm `call`.
- Across modules, and into JIT or region code: `call_indirect` through one
  function table, at a known index (bound when the callee is installed, so an
  IAT call or a vtable call that has resolved once costs one indirect call)
  or through the address map below.
- Calling into JIT code costs the same as any indirect call. There is no
  nest, root, leave or dispatch loop on the hot path.

**Tables index functions, not 4-byte slots.**

- The function table has one entry per function. Noita has about 100k
  functions against about 4.8M slots, about 40× fewer, which is what makes
  per-Worker copies affordable in the browser.
- The guest address → function index map is consulted only on indirect
  transfers whose target is not already bound. It is a hash or a sparse map,
  not a slot array per page, so two functions 1 byte apart no longer share
  anything.

**One copy of each function.** AOT if a unit has it, otherwise JIT, once.
Nothing is translated twice, which removes the duplication behind most of
the extra memory.

**Exceptional control flow.** Wasm exception handling (V8 and wasmtime both
support it now) for unwinding: longjmp, C++ exceptions, Windows SEH frames
that unwind past wasm frames, and the failed return checks above. Signals
are delivered at safepoints, as in §4.3. fork, execve and thread migration
unwind every frame first (§4.2's mechanism, with EH as the transport).

**Patched code.** A function's entry checks that its pages are unchanged
(today's page check, per function rather than per block). A patched function
is retired from the table and map; callers bound to it reach the new body
through the retired entry's forwarding stub.

**Ejected builds** are the same modules with the JIT and region tier left
out: a miss fails loudly. That is pewasm's model exactly.

### 9.3 What carries over from today

- S2's function recovery and summaries; the per-function C lowering in
  gex-cgen (the template for the JIT emitter); the whole-image builder, unit
  cache, unit keys and gex-key.
- Patched-page detection, retirement, and the PAGE_STALE lessons (first
  publish wins; a lagging store catches up).
- The call-out (the region tier's starting point), the trampoline-page unit,
  direct cross-image binding (now: install-time binding into the one table).
- The browser's unit loading, shared modules across Workers and per-Worker
  instantiation.

What goes: nests, roots, leaves, predicted-return plumbing between the tiers,
per-4-byte slot tables, body tables, shared-slot bodies, and per-block
tier-1 for recoverable code.

### 9.4 Plan and gates

1. **Measure the current design to the end** (S3, in progress): the call-out
   run, with world-load split into unit placement, tier switches and tier-1
   translation, and the 8 GB broken down into unit code, per-store tables
   and duplicate tier-1 translations. These numbers are the baseline every
   later step is compared with.
2. **Prototype the function JIT on the Tier 0 corpus** (S3):
   - GIR function → wasm function directly, the ABI above, a function table
     plus an address map;
   - run under `gexc run --fjit` beside today's tiers;
   - **gate:** lockstep against tier-1 on all 30 programs on ga64, gx64 and
     gi386; Tier 0 CPU time per kernel against tier-1, the clang tier and
     emcc (paired A/B, the usual rules); translation time per function;
     memory per function.
   - **Go/no-go:** the prototype must beat tier-1 clearly (target: at or
     below 2× tier-1's time-per-kernel ratio to the clang tier, i.e. most of
     the way from tier-1 to clang) and translate fast enough for start-up
     (target: Noita's working set in seconds, not minutes).
3. **Make AOT units use the same ABI** and install into the one function
   table; retire body tables and nests for code that has a function body.
4. **Region tier** for non-function-shaped code, from the call-out; tests:
   detour, pagespan, a JIT-style code generator (write, call, rewrite), and
   longjmp, SEH and C++ unwinding through mixed tiers.
5. **Noita**, native then browser. **Targets:**
   - steady fps above today's units run (24) natively, at or above pewasm's
     60 fps cap in the browser on hardware WebGPU;
   - world-up no slower than tier-1 today (207 s natively), heading for
     native Wine's ~60 s;
   - peak RSS at or below tier-1 today (5.3 GB), heading for pewasm's
     2 GB heap.
6. **Retire** per-block tier-1 except as the region tier's last resort.

### 9.5 Risks

- **JIT emit speed.** Whole functions are more expensive to emit than blocks.
  Mitigation: emit straight to wasm without an optimiser; leave optimisation
  to the AOT tier and V8/wasmtime's own tier-up.
- **Recovery on demand.** Recovering one function at a time at run time must
  not need the whole image's analysis. Summaries for callees not yet seen
  default to the conservative interface (state written back at the call).
- **Wasm call depth.** Guest recursion becomes wasm recursion; the existing
  depth guard (function-tier.md §7.1) carries over, unwinding through EH.
- **Code that is neither function- nor region-shaped,** such as self-modifying
  loops. The region tier with per-entry page checks handles it; measure its
  frequency on Noita first.
