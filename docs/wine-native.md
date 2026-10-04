# Wine as native wasm: plan (draft, 2026-10-03)

Status: proposal for review. Nothing here is built.

## 1. Why

Today gex runs Windows programs with two pieces of its own on each side of
Wine:

- **Wine's PE side as guest x86.** The prebuilt x86 PE DLLs from the staged
  prefix run through gex's tiers like application code. To reach AOT speed,
  each one needs a unit:
  - built at a pinned base (gex-layout, `tools/nt-cgen-core.txt`);
  - re-keyed and rebuilt whenever recovery, codegen or the base changes;
  - recovered correctly, which has its own bug tail: import thunks, switch
    tables, trailing section bytes.

  A DLL missing from the core list silently runs on tier-1. That was the case
  for wined3d, ddraw, dsound, winmm and msacm32, so every D3D game rendered on
  tier-1.
- **Wine's Unix side replaced by gex's NT layer.** The NT layer
  (`crates/gex-kernel/src/nt`, about 32k lines of Rust) reimplements
  ntdll.so, win32u.so, wineserver, and every Unix library a program happens to
  need:
  - opengl32's and crypt32's Unix sides;
  - display, GDI, named pipes, registry, sections and more.

  Wine 11.15 has 40 modules with Unix libraries. Each new program finds the
  next missing one (crypt32 for Hollow Knight, pipes for Unity), and every
  upstream Wine change in those areas needs porting by hand.

Neither side scales. Both exist because Wine has no wasm host. The fix is to
give it one.

## 2. Target: the Hangover model on a wasm host

Wine already supports running foreign-architecture application code with its
own DLLs native. That holds for x64 programs on ARM64EC; for i386 programs
Wine's own 32-bit DLLs are emulated too, via wow64:
- Windows on ARM: ARM64 Wine runs i386 programs through wow64 plus an
  emulator DLL, and x64 programs through ARM64EC plus `xtajit64`.
- The emulator plugs in through the BTCpu interface (`dlls/wow64/syscall.c`):
  `BTCpuSimulate`, `BTCpuGetContext`/`SetContext`, `BTCpuThreadInit`,
  `BTCpuFlushInstructionCache2`, `BTCpuNotifyMemory*`,
  `BTCpuNotifyMapViewOfSection`.
- FEX and Box64 ship as such DLLs; Hangover was the prototype.

The plan does the same with **wasm32 as Wine's host architecture**:

```
 application x86 code (exe + its own DLLs)  ── gex tiers / AOT units (§9)
        │  calls through export adapters ▲ callbacks through the ftab
        ▼                                │
 Wine PE side, native wasm32 modules (ntdll, kernelbase, user32, wined3d, …)
        │  __wine_unix_call: direct call, same memory
        ▼
 Wine Unix side, native wasm32 (ntdll.so, win32u.so, winewayland.drv, …)
        │  POSIX via a musl-based libc → gex-kernel syscalls (direct import)
        ▼
 gex-kernel (Linux ABI), wineserver as a wasm process, gex compositor, gex-gl/Mesa
```

- **gex translates only the application's own x86 code.** All of Wine is
  compiled from source with clang to wasm, once per Wine version. No Wine
  units, no Wine recovery, no core list, and no per-library Rust.
- **The NT layer retires progressively.** Wine's own ntdll.so, win32u.so and
  wineserver provide NT semantics; gex keeps the Linux kernel layer it already
  has.
- **Eject gets simpler.** An ejected program is its own units plus Wine's
  native modules: no x86 Wine and no Wine units to keep in sync.

## 3. Design

### 3.1 Data model (why i386 first)

- **Same pointers:** a guest i386 address *is* the wasm32 linear-memory
  address. Native Wine code and translated x86 code share pointers with no
  conversion.
- **Same structs:** wasm32 is ILP32 and aligns `int64`/`double` to 8 in
  structs, which is MSVC's i386 layout (Linux i386 aligns them to 4, which is
  why Wine's i386 build needs `ms_struct` care). Win32 structs pass unchanged.
- **No wow64 needed for 32-bit programs:** on a wasm32 host, an i386 program
  is the native pointer size, so its syscall arguments need no 32↔64
  thunking.
- **x64 programs come later** (§3.9).

### 3.2 Building Wine for wasm32

- **The arch:** a new host arch `wasm32` in configure (`--host=wasm32-…`,
  clang/wasi-sdk-style toolchain), next to i386, x86_64, arm and arm64.
- **PE modules:** winebuild emits two parts per DLL.
  - **A wasm PIC module** with the code. Its data is placed inside the PE
    image's mapping (`__memory_base` = image base + data RVA), the same way
    gex units and export providers are placed today.
  - **A PE skeleton:** headers, sections, export table, relocations and data,
    with code RVAs pointing at an address range the ftab binds.

  Wine's PE loader in ntdll maps the skeleton. The host then instantiates
  the wasm module at that base and installs its exports in the ftab, which is
  the §9.9 provider install generalised to every module.
- **Imports:** between native modules, imports resolve to direct wasm calls
  (import binding at instantiation). Calls from native code into application
  code go through §3.3.
- **Unix libraries (`.so`):** ordinary wasm PIC modules linked against a
  musl-based libc whose `syscall()` imports gex-kernel directly. gex-kernel
  already serves the i386 Linux ABI, which matches wasm32's ILP32 layouts, so
  its compat paths serve these too.
- **Assembly** in ntdll (signal and syscall dispatch, context switches,
  thunks) gets wasm replacements (§3.4).

### 3.3 Calls across the boundary

- **x86 → native.** Every export's signature is already in Wine's `.spec`
  files (`@ stdcall CreateFileW(wstr long long ptr long long long)`).
  winebuild generates, per export, an entry adapter with the §9 signature
  `F_body(ctx, depth, need, ret)`. It reads arguments from the guest stack,
  calls the native function, sets EAX/EDX/ST0, pops N for stdcall, and
  returns 1. The adapter is bound at the export's address in the ftab, so
  IAT calls, GetProcAddress pointers and app-side function pointers all reach
  it. This replaces hand-written providers with generated ones for all of
  Wine.
- **Native → x86.** Callbacks are window procedures, hooks, thread entry
  points, comparators passed to msvcrt, COM objects implemented by the app,
  and SEH handlers.
  - Every indirect call in Wine's native code may target application code, so
    it goes through a check, as ARM64EC's `__os_arm64x_check_icall` does.
  - An LLVM pass rewrites `call_indirect` sites into a fast inline test of the
    target's ftab entry. A native target is called directly with the C ABI.
    A guest target takes an exit thunk, generated from the call's wasm
    signature, that builds the x86 frame and calls `F_body`.
  - The common case, native to native, stays one load and one compare. That
    matters: Peggle makes 222M indirect calls in wined3d per run.
- **Patched exports:** a guest that patches a Wine export (Detours) writes
  the skeleton's bytes. Those bytes are inside a module gex knows, so the
  write rebinds that ftab entry to the patched x86 code. The provider policy
  (keep the native function and log) is the fallback.

### 3.4 ntdll on wasm: exceptions, threads, syscalls, gex as the CPU

- **`signal_wasm32.c`**, the new per-arch file. It covers:
  - the CONTEXT (the guest's i386 register file, i.e. gex's ctx);
  - the syscall dispatcher: a PE → Unix call is a direct call, with Wine's
    syscall table kept for numbering;
  - user callbacks (`KiUserCallbackDispatcher`) as nested calls;
  - APC and exception delivery.

  wasm has no signals, stack switching or ucontext, so these follow the
  §9.8 nested-dispatch work R has done.
- **gex as Wine's CPU.** gex implements the BTCpu contract for x86 threads:
  - simulate (run guest code from a context);
  - get/set context;
  - suspend/resume consistency;
  - code-cache flush on write;
  - memory and section notifications.

  i386 programs on a wasm32 host don't load wow64, so ntdll calls these
  hooks directly, but the contract is Wine's own. FEX, Box64 and xtajit
  already implement it, and Wine maintains it upstream.
- **Faults in guest code.** Native ntdll gets gex's fault (the resumable-fault
  path, bit 62), builds the EXCEPTION_RECORD and CONTEXT, and runs
  `KiUserExceptionDispatcher`. Native code runs the dispatcher, and the
  handlers run as guest code.
- **Native-code exceptions.** `__TRY`/`__EXCEPT`, setjmp and longjmp need wasm
  exception handling. V8 has it. Natively it requires the wasmtime upgrade R
  priced (wasmtime 49: rustc 1.96 plus gex-cache fixes). That upgrade becomes
  a hard prerequisite.

### 3.5 The Unix side and wineserver

- **Native Unix libraries,** in priority order: ntdll.so, win32u.so,
  winewayland.drv, opengl32, ws2_32, crypt32, secur32, dnsapi, nsiproxy,
  winepulse/winealsa (audio, §6), then winevulkan.
- **wineserver as wasm,** first as a gex-kernel process over a Unix socket
  with SCM_RIGHTS and shared memory, all of which gex-kernel supports.
- **Faster sync:** Wine 11 supports the ntsync driver
  (`dlls/ntdll/unix/sync.c`). gex-kernel implementing `/dev/ntsync`
  removes the server round trip from waits, mutexes and events; that
  matters for Noita's job chain.
- **Later:** wineserver in-process with a direct-call transport.

### 3.6 Graphics and input

- **Display:** winewayland.drv speaks Wayland to gex's compositor, natively
  and in the browser (it exists in both). Xwayland is not needed for Windows
  programs.
- **GL:** opengl32 goes through winewayland's EGL to gex-gl (the guest GL
  library compiled to wasm), then to Mesa wink in the browser or the host GL
  natively. wined3d's D3D→GL path is native wasm, not lifted x86.
- **Vulkan:** winevulkan comes later.

### 3.7 The browser

- **The same modules.** A dlopen is an instantiation in each Worker, sharing
  memory and the per-Worker ftab (stream B's §9 work).
- **wineserver** runs as its own Worker process.
- **Cost:** the per-Worker instantiation cost of a few hundred Wine modules
  needs measuring; precompiled module caching applies.

### 3.8 What gex keeps and what retires

- **Keeps:**
  - x86 translation for application code (tier-1, fjit, AOT units, eject);
  - gex-kernel (the Linux ABI);
  - the compositor;
  - gex-gl and Mesa;
  - the §9 ftab;
  - export providers, as the mechanism every native module now uses.
- **Retires, once the native path passes the same tests:**
  - the NT layer (`crates/gex-kernel/src/nt`), by subsystem;
  - Wine x86 prefix units, the core list and layout pinning for Wine DLLs;
  - hand-written CRT providers where Wine's own msvcrt/ucrtbase is native.
- **Applications' own CRT copies** (msvcr120, msvcp120 shipped with the game)
  are application code and stay translated. ConcRT and the CRT providers stay
  useful for them.

### 3.9 x64 programs (later)

- **Memory:** needs wasm64, i.e. memory64 (stream U's work).
- **Data model:** needs a clang target variant where `long` is 32 bits, since
  Wine's PE code assumes LLP64. That's a small TargetInfo patch: a
  wasm64 "windows" environment.
- **Unix side:** stays LP64, as on x86_64 Linux.
- **Until then,** x64 programs keep today's path.

## 4. Phases and gates (agent time)

| Phase | Work | Gate | Estimate |
|---|---|---|---|
| W0 spikes | (a) wineserver and one Unix library (ws2_32 or crypt32) built for wasm32 on gex-kernel; (b) a winebuild prototype: one small PE DLL as a wasm module plus skeleton, called from an i386 test program through spec adapters, with a callback back into x86 through the ICALL check; (c) clang wasm32 against today's lifted units on a Wine hot path | each spike runs; the boundary cost is measured | 1–2 days |
| W1 toolchain | the configure arch, winegcc/winebuild output, spec adapters, the ICALL LLVM pass, the wasm EH build, the wasmtime upgrade | Wine builds for wasm32; adapter unit tests | 3–5 days |
| W2 core | ntdll (signal_wasm32, loader, exceptions, threads, the BTCpu hooks to gex), kernelbase, kernel32, msvcrt/ucrtbase | tests/nt hello, sync, tls, seh, cxx, apc | 4–6 days |
| W3 Unix side | win32u, winewayland.drv, opengl32 through gex-gl, ws2_32, crypt32, wineserver, ntsync in gex-kernel | all of tests/nt natively, including GL | 5–8 days |
| W4 games | Noita and Peggle on native Wine; AOT share of app code, fps against today's path and pewasm (fixed seed, paired) | ≥ today's fps; Peggle's wined3d native | 3–5 days |
| W5 browser | modules in Workers, wineserver Worker | browser tests/nt; Noita, Peggle and Half-Life in the page | 3–5 days |
| W6 retire | remove superseded NT-layer subsystems and Wine prefix units; eject on native Wine | gate green with the NT layer gone for i386 | 2–4 days |
| W7 x64 | wasm64, the LLP64 clang variant, memory64 | x64 tests/nt, then Unity | 5+ days |

Today's path keeps working throughout. Programs switch per process
(`--wine native`) until W6.

## 5. Risks

- **A long-lived Wine fork.** A wasm arch is a sizeable patch set. Keep it
  arch-shaped (configure, winebuild, `signal_wasm32.c`, per-arch `#ifdef`s),
  rebased on Wine releases, and upstreamable in principle. It would live in a
  fork like mesa-wink's; pushing needs the user's approval.
- **The ICALL check isn't wasm-specific.** Any native-Wine/emulated-app
  split needs it; it's how ARM64EC works.
  - **What is ours to build:** clang emits the check and the entry and exit
    thunks for ARM64EC, but has nothing for wasm32. We write that pass and
    the thunk generator, then validate them.
  - **No precedent for i386:** for 32-bit programs on ARM, Wine uses wow64
    with emulated i386 DLLs, as gex does today. Native Wine DLLs exist only
    for x64 programs (ARM64EC). Native Wine for i386 programs is a new
    combination, built from ARM64EC's design, the spec signatures and the
    BTCpu contract.
  - **Cost:** on indirect-call-heavy code (wined3d, COM) the fast path must be
    a few instructions. If W0(c) shows it doesn't pay, fall back to
    type-based checks on callback types only, which is less general.
- **Exceptions and unwinding through mixed frames** (a guest handler
  unwinding native frames) depend on wasm EH and the wasmtime upgrade.
- **Startup cost** of instantiating hundreds of modules, especially per
  Worker in the browser.
- **Audio:** gex has no Pulse or ALSA server. winepulse needs one, or gex
  needs a small Wine audio driver for its own sink. This is new work either
  way.
- **Programs that depend on Wine's x86 bytes:** anti-cheat, checks of code
  bytes in system DLLs, self-modifying hooks. They are rare in this set and
  handled by rebinding on write (§3.3).

## 6. Open decisions for the user

1. **Go/no-go on W0.** The spikes answer the cost questions before the big
   work.
2. **Where the Wine fork lives,** like mesa-wink's `wink`, and whether it may
   be pushed.
3. **Priority against current streams.** This would be a new stream W. It
   doesn't block F, R, B or E, but it changes what their Wine-side work is
   worth: F's Wine units and the NT layer's per-library work get less
   valuable, while app-code AOT and the browser runtime keep their value.
4. **Whether x64 programs (W7) matter before Unity.**
