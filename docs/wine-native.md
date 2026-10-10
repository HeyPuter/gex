# Wine as native wasm: plan (draft, 2026-10-03)

Status: stream W. W0 done (§4.1); W1 toolchain built (§3.10–3.13, §4.2).

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
- **wineserver in-process, as a thread (chosen design; built in W3.1,
  approved 2026-10-06).** wineserver shares coherent `MAP_SHARED` memory
  with its clients: the user shared data (the clock), session objects,
  and since Wine 9 the queue and desktop state win32u reads directly. gex
  processes have separate linear memories, and in the browser separate
  `WebAssembly.Memory`s cannot alias, so the server lives in the Windows
  process: linked into the Wine loader (the server's objects compiled
  with `server/wasm32_inproc.h`, which renames what both sides define and
  makes its `main` and exits `wineserver_main`/`wineserver_exit`) and
  started by ntdll.so's `start_server` as a **thread**. It has its own
  working directory (`unshare(CLONE_FS)`), shares the process's
  descriptors and memory, takes no SIGCHLD (the process's children are
  ntdll.so's), and maps the user shared data at the client's 0x7ffe0000
  (one wasm memory cannot map a page at two addresses; gex hands a second
  `MAP_SHARED` of the same open file the first mapping's pages, so later
  maps, the session's, simply share the server's address). Its protocol
  stays Wine's (the Unix socket, SCM_RIGHTS, request and reply pipes): the
  server and ntdll.so are otherwise unmodified Wine. No host-side process
  machinery is involved, so it is the same in the browser.
  - **Known gap: child Windows processes.** A second Windows process
    (a launcher starting the game, a crash handler such as Unity's
    UnityCrashHandler, an out-of-process RPC/COM server, a process a
    program spawns) is a separate gex process: it connects to the first
    process's server through the socket and works, but it cannot see the
    server's shared memory. It reads a stale copy of what it mapped: the
    clock in its user shared data does not move, and win32u's session
    state (windows, queues, the desktop) is not shared with it. One
    Windows process per session, which covers most games, but not
    launcher-started ones.
  - **What would lift it** (later, none planned yet): a `MAP_SHARED` that
    stays coherent across gex processes. Natively, the host can map one
    memfd into several linear memories at the same guest address (the
    memories are host mappings). In the browser there is no aliasing
    between `WebAssembly.Memory`s; the options are processes of one
    session as threads of one memory (memory64 for several 2 GiB i386
    address spaces), or copying the shared pages at synchronisation
    points (server requests, waits) for the state that tolerates it (the
    clock), which does not cover win32u's lock-free reads. A direct-call
    transport in place of the socket is a separate, later optimisation.
- **Sync through ntsync.** Wine 11 supports the ntsync driver
  (`dlls/ntdll/unix/sync.c`). gex-kernel implements `/dev/ntsync`, and its
  ioctls (semaphores, mutexes, events, wait-any/all) go to the kernel's
  futex machinery. Waits, mutexes and events then never reach wineserver,
  which matters for Noita's job chain. The server round trip that is left
  (W0(a): 1.07 ms, from the native host's 1 ms fd polling) needs
  event-driven fd wakeups in gex-kernel before W3.

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

### 3.10 Toolchain (decided in W0, 2026-10-04)

**clang with a musl port of our own, not emscripten and not wasi-libc.**

- **Compiler:** the system clang (22), the one gex-cgen already uses, with
  `--target=wasm32-unknown-linux-musl`. clang accepts that triple and
  defines `__linux__`, so Wine's Unix code takes its Linux paths. Flags
  (`tools/wine-wasm/flags.sh`): `-fPIC -matomics -mbulk-memory
  -mmutable-globals -msign-ext -mnontrapping-fptoint -mtail-call`, which
  V8 and wasmtime 27 both run.
- **libc:** upstream musl 1.2.5 with a new `arch/wasm32` (fork
  `wt/musl-wasm`, branch `gex-wasm32`, not pushed; about 600 lines):
  - Syscalls are the **i386 Linux ABI**: the wrapper writes the number and
    the arguments into the thread's gex register context and calls
    `gex.syscall(ctx, pc)`, the import translated code already uses.
    gex-kernel's i386 compat layer serves them unchanged.
  - Where i386 and wasm32 struct layouts differ (wasm32 aligns 64-bit
    members to 8), musl converts: `struct kstat`, `statfs`, packed
    `epoll_event`. `struct flock` matches what gex-kernel reads (see the
    note below).
  - Threads, TLS and the C stack are wasm-ld's: one module instance per
    thread, `__tls_base` and `__stack_pointer` per instance,
    `__wasm_init_tls` for new threads; `clone` starts the child at a fixed
    entry with its function on the new stack.
  - `setjmp`/`longjmp` are stubs (longjmp aborts) until there is wasm
    exception handling (V8 has it; wasmtime 27 natively does not).
  - Signals are not delivered asynchronously; a handler would have to run
    at a syscall boundary, which needs gex support (W1).
- **compiler-rt builtins** from emsdk's copy of the sources (int128 and
  binary128 `long double`; no x87 or bf16).
- **Linux UAPI headers** are the host's, with `asm/` taken down its i386
  branches (`build-sysroot.sh`).
- **Linking:** `wasm-ld --experimental-pic -pie --shared-memory
  --import-memory`: a position-independent module importing gex's memory,
  `env.__indirect_function_table` (the ftab), `__memory_base`,
  `__table_base` and `__stack_pointer`. Its `__wasm_init_memory` start
  function initialises the data once (an atomic flag), so instantiating it
  per thread is safe. `tools/wine-wasm/wcc` is the compiler driver
  (`clang` for compiles; wasm-ld plus the container for links; `-shared`
  for side modules).
- **Running it under gex** (`shims/native/src/wasmprog.rs`, about 90
  lines): the module goes in an ELF32 i386 container
  (`tools/wine-wasm/wasm-elf.py`), so exec, the initial stack, auxv and
  brk are the kernel's as for any i386 program. A header next to the
  entry names the module, its data region and two entries. The shim
  installs the module in the function table as a stateful provider
  (stream E's `libprov`: data at `__memory_base`, a stack per thread),
  bound at those entries. Nothing is translated. The first dispatch of the
  entry finds the binding (`ftab::serve`).

**Why not emscripten.** Its libc is musl too, but its syscalls, threads
(pthread Workers), dlopen and memory growth belong to its JS runtime. gex
owns all four (gex-kernel, its own Workers, the ftab, its own memory map).
Emscripten would also give the browser and native builds different
runtimes. The hostx Xwayland build shows the cost: a hand-written JS bridge
per program.

**Why not wasi-libc.** WASI has no fork/exec, Unix sockets, SCM_RIGHTS,
mmap, futexes or signals. Wine's Unix side and wineserver need all of
them, and gex-kernel already has them.

**How Wine learns the host** (fork `wt/wine-wasm`, branch `wasm32`, from
11.15; not pushed):
- `configure.ac`: `wasm32` → `HOST_ARCH=wasm32` and `__wasm32__`. No
  `-mabi=ms`: wasm32 has one calling convention.
- `configure --host=wasm32-unknown-linux-musl CC=wcc
  --with-wine-tools=<native tools build> --without-mingw` configures
  as-is.
- On wasm32, `winnt.h` takes the i386 `CONTEXT`. The PE side's CONTEXT is
  the guest's register file (§3.4).
- The tools (widl, winebuild) map `wasm32` to i386 for now, which is right
  for data layout. W1 gives winebuild a `CPU_WASM32` that emits the
  wasm module plus PE skeleton (§3.2).
- Without mingw, Wine's makedep already plans its "builtin ELF DLL" layout:
  PE-side code compiled by the host compiler with `-D_WIN32 -fshort-wchar`
  into host-format modules. That is the precedent for PE DLLs as wasm
  modules. The PE side gets its own triple/flags in W1.

**ABI note (for the coordinator).** On wasm32 the gex-kernel ABI is "i386
numbering, gex's compat conversions". Where gex's i386 layer passes a
structure through unconverted, musl's wasm32 layout must match gex-kernel's
native one. The case so far is `fcntl` locks: gex hands F_GETLK64 (12–14)
through unmapped, so wasm32 uses 5/6/7 with the 64-bit `struct flock`.
**Decided (coordinator, 2026-10-04): a `wasm32` personality in gex-kernel,
in W1.** It is additive: a new personality whose structure layouts are
defined to match musl's `arch/wasm32`. The i386 paths stay unchanged, and
tests cover both. As built: §3.11.

**The ICALL check (decided, built in W1).** `tools/wine-wasm/icall/GexIcall.cpp`
is an LLVM pass plugin for the system clang (`-fpass-plugin=GexIcall.so`,
run at the end of the optimisation pipeline, after inlining). Every
non-variadic indirect call becomes:

```
gexw_set_icall_target(p)                        ; a wasm global (gexw_globals.s)
callee = p < 0x400000 ? p : __gexw_exit.<sig>   ; select, no branch
call callee(args)
```

- **The exit stubs** are one per wasm signature (plus `byval` layout),
  `linkonce_odr` and hidden. A stub lays the arguments out as i386 stack
  words: i64 and double take two, `byval` structs are copied in, narrow
  integers are widened. It then calls `__gexw_guest_call` (EDX:EAX) or
  `__gexw_guest_call_fp` (ST0, popped) in `tools/wine-wasm/gexw_rt.c`.
  Those push the words and a return address, run the target through the
  function table, and put ESP back, so one stub serves cdecl and stdcall
  callees.
- **Variadic indirect calls** are left alone and counted
  (`GEXW_ICALL_STATS=1`).
- **Cost in V8** (node 26.7, core 3, tools/wine-wasm/spike-c, 20M calls,
  median of 5):

  | | ns/call |
  |---|---|
  | plain `call_indirect` | 0.49 |
  | the pass's output | 0.99 |
  | W0(c)'s hand-written select form | 0.93 |
  | native → guest through a pass stub | 6.7 |

- **Spike (b)** runs on the pass by default: demo.dll's callbacks, cdecl
  and stdcall, plus a double-returning callback with an int64 argument.
- **Entry thunks (W2, as built):** native function pointers that escape to
  guest code (COM vtables a native DLL hands the program, window procedures)
  are table indices, which x86 code cannot call by themselves.
  - On wasm32 `__stdcall` (so `WINAPI`, `CALLBACK`, `STDMETHODCALLTYPE`) is
    `annotate("gex.stdcall")` (corecrt.h); clang ignores it on types and
    keeps it on functions (`llvm.global.annotations`).
  - The ICALL pass writes `__gexe.<fn>` for every function whose address
    escapes (any use but a direct call or metadata): winebuild's adapter
    shape, arguments by the wasm signature (i64/double 8 bytes, byval
    structs by address, MSVC's struct returns: 1/2/4/8 bytes in EDX:EAX,
    larger through the hidden pointer), popped if stdcall. Varargs
    functions get none.
  - `{function, thunk}` pairs go in the data section `gexw_entries` (pesym
    restores the RETAIN flag `wasm-ld -r` drops); gexrt anchors the section
    and exports its bounds as `__gexw_entries`.
  - gexc (`wasmdll::bind_entry_thunks`, shared mode only) reads the
    relocated pairs once the module's data is initialised and binds each
    thunk at its function's table index as a guest address.
  - Not covered: thiscall/fastcall functions (no annotation yet), and
    per-DLL modules (§3.13).

### 3.11 The wasm32 ABI on gex-kernel (W1, 2026-10-04)

A native wasm32 program (wasmprog, §3.10) is a gex process with
`Kernel::compat` (the i386 ABI) and `Kernel::wasm32` set. The ABI is
**i386 Linux's, with musl's `arch/wasm32` converting where wasm32's own
layouts differ, and one difference in control flow**. The i386 paths are
unchanged.

**Calls.**
- **Entry:** the thread writes the number to `eax` and the arguments to
  `ebx, ecx, edx, esi, edi, ebp` in its gex context (8-byte slots, upper
  half zero), sets `esp` to its C stack pointer less 256 (below
  everything live), and calls `gex.syscall(ctx, 0)`.
- **Result:** in `eax`, negative errno as on Linux.
- **Numbers:** i386's (`asm/unistd_32.h`), including the time64 calls,
  `statx`, `fcntl64` and the direct socket calls. 64-bit arguments are
  split low/high as on i386.

**Structures.** Everything the kernel reads or writes uses the i386 kernel
layout. Where wasm32's C layout differs (it aligns 64-bit members to 8),
musl declares or converts:

| structure | kernel layout | in musl wasm32 |
|---|---|---|
| `stat64` (stat, fstat, fstatat64) | i386, 96 bytes, 64-bit fields 4-aligned | `struct kstat` with `aligned(4)` 64-bit members; `struct stat` is musl's own and converted. `statx` is tried first and is the same on both. |
| `statfs64` | i386, 84 bytes | `struct statfs` packed to 84 |
| `epoll_event` | 12 bytes, packed | packed, as on x86-64 |
| `flock` (F_GETLK/F_SETLK/F_SETLKW = 5/6/7) | the canonical 64-bit layout (`l_start` at 8) | wasm32's natural `struct flock` is that layout. The kernel reads only `l_type`, at 0 in every layout, so i386's F_*LK64 (12–14, mapped since 7ab9dd35) works too. |
| `sigaction` | i386 `k_sigaction`; the handler is a **function-table index** | musl's own conversion |
| `timespec` / `timeval` | time64 calls only: two 64-bit words | `time_t` is 64-bit |
| `msghdr`, `iovec`, `sockaddr`, `cmsghdr`, `dirent64`, `rlimit64`, `siginfo` | i386 | identical in wasm32 (ILP32) |

**Threads and processes.**
- **`clone` with `CLONE_THREAD`:** the child gets the parent's context
  copied, with `esp` = the new stack. The parent sets `PC` to the image's
  thread entry (`AT_ENTRY + 16`) for the call. The child's stack top holds
  `(func, arg, thread pointer, fresh-TLS flag)`. `CLONE_SETTLS` is not
  passed: the thread pointer and `__tls_base` are wasm globals the child's
  entry sets (one module instance per thread).
- **A `clone` without `CLONE_THREAD`** (posix_spawn's
  `CLONE_VM|CLONE_VFORK`) is a new process, a copy of the address space,
  starting at the same thread entry. The native shim makes it inside the
  syscall (the parent's wasm frames cannot be resumed elsewhere).
- **`fork()`** (and `vfork()`, which is `fork`) works by unwinding and
  rewinding the wasm stack (§3.11.1).
- **`execve`** of another wasm32 container works.

#### 3.11.1 fork (2026-10-05)

gex cannot copy a wasm call stack: its frames are the engine's, not
linear memory. A program that links `fork` is instrumented after the
link with binaryen's **asyncify** (`tools/wine-wasm/wcc`), which lets
every function that can reach the `gex.fork_point` import save its locals
and call position to a buffer in linear memory, return, and later be
re-entered and resume from it. That is how WASIX forks. JSPI does not do
it: it suspends one stack in place inside one instance and cannot
duplicate it into another instance or Worker, and wasmtime has no
equivalent.

- **musl** (`src/process/wasm32/__gex_fork.c`; `arch/wasm32/syscall_arch.h`
  sends `_Fork`'s constant `__syscall(SYS_fork)` there): every thread's
  entry runs its function under a driver (`__gex_fork_run`, not
  instrumented). `__gex_fork` maps a 1 MiB record (asyncify's `{cur, end}`
  buffer, the C stack pointer) and calls `gex.fork_point`, where the host
  starts the unwind; every frame up to the driver saves itself and
  returns. The driver stops the unwind, notes its own function in the
  record and makes the child with `clone(SIGCHLD)` (a new process: a copy
  of the address space, record included) whose thread starts at the
  thread entry and calls `__gex_fork_child`. Both start the rewind and
  call the driver's function again; each frame restores itself and calls
  its callee again down to `gex.fork_point`, where the host stops the
  rewind. `fork` returns the pid in the parent and 0 in the child, the C
  stack pointer as it was. Only the calling thread exists in the child.
- **The host** (native `libprov.rs`, browser `#wasmprogImports`) gives the
  program `gex.asyncify(op, arg)`, asyncify's exports on the calling
  instance, and `gex.fork_point(rec)` (start unwind, or stop rewind). The
  copy is the kernel's ordinary fork of a process; in the browser the
  child's Worker installs the same module over the parent's data.
- **wcc:** asyncify instruments only the functions `forkreach.py` finds can
  reach `gex.fork_point` by a type-correct call graph (an indirect call of
  type T reaches the table's functions of type T; any other traps), not
  asyncify's default of everything that makes an indirect call. busybox:
  403 KB uninstrumented, 638 KB with asyncify's own analysis, 540 KB with
  the type-based one. asyncify cannot instrument `try_table` or tail calls,
  so Unix-side code lowers setjmp onto the legacy exception instructions and
  wcc translates them to the standard ones after (`--translate-to-exnref`),
  and no wasm32 code uses tail calls (the libc is shared). A program that
  never forks is not asyncified (only translated).
- **Tests:** `tests/wasm32/fork.c` (fork without exec, separate memory;
  from 20 frames deep through a function pointer, every frame's locals
  intact; in a threaded program), busybox as an MMU build (hush forks for
  subshells, pipelines and `$(...)`), natively (`run.sh`, `busybox.sh`) and
  in the browser (`run-browser.sh`).

#### 3.11.2 What the musl fork changes, and why

The user would rather have no musl fork; it is kept to an architecture
port. Everything below is a file of the `wasm32` arch (new files, which
upstream musl's layout already provides for), except two one-line changes
to shared files. `git diff 0784374d` (musl 1.2.5) in `wt/musl-wasm` is the
whole of it.

| where | what | why |
|---|---|---|
| `configure` (1 line) | `wasm32*) ARCH=wasm32` | the target triple names the arch |
| `include/sys/epoll.h` (1 line) | `epoll_event` packed on wasm32 too | the i386 kernel's 12-byte layout; wasm32 would pad it to 16 |
| `arch/wasm32/*` | types, the i386 syscall numbers, `syscall_arch.h` (syscalls through `gex.syscall(ctx)`, signal frames at the return, `SYS_fork` to `__gex_fork`), atomics, `kstat` | the port itself |
| `crt/wasm32/crt1.c` | entry from gex's `__gex_start`; main under fork's driver; `_init` runs `__wasm_call_ctors` | a wasm module has no `_start` gex can use, no `.init_array` |
| `src/internal/wasm32/gex_thread.s` | `__gex_start` / `__gex_thread_start` (F_body entries), per-thread globals, C stack pointer get/set, handler calls | wasm has no registers to keep these in |
| `src/thread/wasm32/*` | `__clone` (thread entry, new process), the child's start, thread area, `__unmapself`, `syscall_cp` | wasm has no stack switching or `clone` asm |
| `src/env/wasm32/*` | TLS (wasm-ld's `__tls_base`), `main(void)` fallback | wasm-ld's TLS model |
| `src/process/wasm32/__gex_fork.c` | fork (asyncify) | §3.11.1 |

**Not in musl: gex's support library, libgex** (`tools/wine-wasm/libgex`,
built into the sysroot's `lib/libgex.a` by build-sysroot.sh, linked by wcc
before libc with every program; musl branch `gex-wasm32-slim`). What is
gex's protocol rather than the libc's:

| file | what | why it can live outside musl |
|---|---|---|
| `setjmp.c` | the SjLj runtime clang's lowering calls (`__wasm_setjmp`, `__wasm_setjmp_test`, `__wasm_longjmp`, the `__c_longjmp` tag), `setjmp`/`longjmp`/`sigsetjmp` | musl's generic setjmp.c, longjmp.c, sigsetjmp.c are empty; siglongjmp stays musl's |
| `dl.c` | `dlopen`/`__dlsym`/`dlclose` over the closed-world registry (§3.12) | a static musl's dlopen and `__dlsym` are weak stubs; dlclose sits next to dlopen so libc's is never pulled; errors through musl's `__dl_seterr` |
| `signal.c`, `signal_call.s` | `__gex_signal_frames` (below), `__gex_call_handler` | gex-kernel's frame protocol; musl's syscall stub only names it. `__restore`/`__restore_rt` are musl's generic no-ops (never called) |
| `vfork.c` | vfork as fork | first in the link, so libc's (SYS_fork through `syscall()`, which does not reach the fork driver) is not used |

(clang lowers only calls to `setjmp`: a `sigsetjmp` returns 0 once and a
`siglongjmp` to it is an uncaught exception: a known gap, as before the move.)

Removed 2026-10-05: the vfork emulation (a `vfork` macro in `unistd.h`,
`execve`/`_Exit` overrides) and the constructor call patched into
`__libc_start_main`.

**Signals, the one control-flow difference.** gex cannot interrupt wasm
code or move its PC, so signals are delivered at syscall returns:
- The kernel builds the ordinary i386 frame in the context: `esp` = the
  frame, `eax` = the signal, for an `SA_SIGINFO` frame `edx` = `&info` and
  `ecx` = `&uc`, `PC` = the handler (a table index). The syscall then
  **returns normally** (`SyscallOutcome::Continue`) instead of resuming at
  the handler.
- musl's stub sees `PC` changed, calls the handler on a C stack below the
  frame, sets `esp` as the i386 restorer would (+4 rt, +8 legacy) and
  issues `rt_sigreturn`/`sigreturn`.
- `sigreturn` restores the interrupted context (the syscall's result
  included) and also returns normally.
- A signal sent while the program computes without syscalls waits for its
  next syscall. Handlers that `longjmp` out need wasm EH (§3.4).

**Tests:** `tests/wasm32/run.sh` (native output against gex):
- `signals.c`: plain and SA_SIGINFO handlers, the mask during and after,
  blocked-then-unblocked, a syscall's result surviving its handler;
- `threads.c`: threads, thread-locals, mutexes, the heap, stat,
  posix_spawn plus exec plus wait.

The x86 and i386 paths are covered by x86-programs, lockstep and tests/nt
as before.

### 3.12 Unix libraries: a closed world, dlopen by registry (W1)

Wine's Unix side loads libraries with `dlopen`: ntdll.so loads each PE
module's unixlib (`ws2_32.so`, `opengl32.so`, …) and the drivers
(`winewayland.drv.so`), then `dlsym`s `__wine_unix_call_funcs`. Under gex
there is no run-time dynamic linking. **Every Unix library a build will use
is linked into the program** (the eject model, pewasm's whole-program
static build). musl's wasm32 `dlopen` looks the name up in a registry the
link adds:

- **Renaming:** each library is compiled with its exported symbols renamed
  apart (`-D__wine_unix_call_funcs=ws2_32__wine_unix_call_funcs`). Wine's
  unixlibs export only a handful of names; everything else in them is
  `static`, and their calls into ntdll.so are ordinary symbols that resolve
  at link time.
- **The registry:** `tools/wine-wasm/dlreg.py` writes it, one entry per
  `(library file name, symbol, renamed symbol, data|func)`.
- **Lookup:** `dlopen` matches the file name in any directory (Wine asks
  for `…/i386-unix/ws2_32.so`). `dlsym` searches one library, or all of
  them for `RTLD_DEFAULT`/`dlopen(NULL)`. `dlclose` does nothing.
- **Why:**
  - nothing to instantiate per Worker at run time (§3.7's startup risk
    goes away for the Unix side);
  - direct calls and LTO across ntdll.so, win32u.so and the drivers;
  - no shim support for side modules.

  A run-time loader (side modules instantiated per thread, symbol binding
  through the store's instances) stays possible if something needs it.
- **Tested:** the ws2_32 harness finds `__wine_unix_call_funcs` through
  `dlopen("/usr/lib/wine/i386-unix/ws2_32.so")`, and an unlinked library
  fails with dlerror (`tools/wine-wasm/spike-a/ws2test.c -DUSE_DLOPEN`).

### 3.13 The PE side's build (W1, as built)

In the Wine fork (`wt/wine-wasm`, branch `wasm32`):
- **`CPU_WASM32` in the tools** (`tools/tools.h`): i386's data layout, no
  assembly.
  - Spec entries marked `-arch=i386` are wasm32's too (it serves i386
    programs). `-thiscall` and `-fastcall` are kept: they describe the
    guest's calling convention.
  - widl generates i386's stubs and format strings.
  - Skeletons carry `IMAGE_FILE_MACHINE_I386`.
- **Compiling:** with no PE cross-compiler, configure plans Wine's
  "builtin ELF DLL" layout. On wasm32 that is the layout wanted: PE code
  compiled by the host compiler (`wcc`, `-D_WIN32 -fshort-wchar
  -fno-builtin`) into a module, plus a PE file per DLL (makedep's "fake
  module").
  - `wcc` applies the ICALL pass to PE-side code (`-D_WIN32`).
  - `__TRY` uses clang's wasm SjLj lowering (`exception.h`, winecrt0's
    `__wasm_setjmp`), which needs wasm exception handling: V8 has it,
    wasmtime 27 does not.
- **`winebuild --dll` (wasm32)** writes C, not assembly (`wasm32.c`):
  - one entry adapter `gexp_<export>` per exported function. It matches
    the `.spec`'s arguments one by one with the wasm signature of the C
    function, read from the objects' linking sections:
    - 8-byte arguments that the code takes as i32 are structs passed by
      value, so the adapter passes the guest stack address;
    - a leading hidden pointer with a void result is a small struct
      return, which goes back in EDX:EAX;
    - results go in EAX, EDX:EAX or ST0;
    - stdcall pops its arguments; thiscall and fastcall read ECX/EDX;
  - stubs call `__wine_spec_unimplemented_stub`;
  - `gexp__entry` calls the DLL's entry point;
  - the data exports go out as `--export-if-defined` link options (an
    `.rsp` beside the object), so their module offsets are readable.
- **Not adapted yet:** varargs exports (i386's 4-byte stack slots are not
  wasm32's `va_list`) and ntdll's system-call exports (they become direct
  calls into the Unix side, W2). Both bind to `gexp__missing`, which
  names them when called.
- **`winegcc`** links a position-independent module. Undefined symbols
  stay imports (`--unresolved-symbols=import-dynamic`), and no libc is
  linked. The import libraries are empty archives.
- **`winebuild --fake-module --wasm-module M`** writes the PE skeleton
  from the linked module (`spec32.c: output_wasm_skeleton`):
  - `.text`: the entry point and one 4-byte `ud2; ud2` stub per exported
    function;
  - `.wasmdat`: the module's data (`__memory_base`); data exports point
    at their offsets in it;
  - `.edata`: the exports, forwarders included;
  - `.wasm`: the module;
  - `.rsrc`: the resources.

  makedep builds it after the module.
- **winecrt0 (`libs/winecrt0/wasm32.c`)** is linked into every DLL. It
  holds:
  - the adapters' helpers;
  - `__gexw_guest_call` for the ICALL pass's exit stubs;
  - the per-thread wasm globals;
  - the SjLj runtime.

  It uses gex's ABI headers, vendored as `include/wine/gex/`.
- **In gexc (`shims/native/src/wasmdll.rs`):** when Wine's loader maps a
  PE with a `.wasm` section, the module is installed as a stateful
  provider. Its data is at `.wasmdat`, `gexp_<name>` binds at each export,
  `gexp__entry` at the entry point, and `gexp__missing` at the rest.
- **Tested:** `tests/wasm32/dll` builds a DLL through winebuild and wasm-ld
  and calls it from a MinGW i386 program, on `--no-cgen` and `--fjit`. It
  covers each adapter kind:
  - stdcall and cdecl;
  - int64 (EDX:EAX), and float and double (ST0);
  - thiscall and fastcall;
  - a struct by value and a struct return;
  - void;
  - cdecl and stdcall callbacks (through the ICALL pass);
  - a data export, a forwarder and a stub.
- **Imports between DLL modules** (decided: §3.14 links the PE side as
  one module). The original note: Each DLL's module imports
  what other DLLs define, and gex instantiates a module when its DLL maps,
  which can be before the DLLs it imports from. The proposed answer is the
  closed world again:
  - link the PE side as one module, every DLL's bitcode internalized to
    its exports first (`llvm-link` + `-internalize`), so their private
    symbols cannot collide;
  - every cross-DLL call is then direct, with LTO across DLLs;
  - per-DLL modules stay possible for a run-time-loaded DLL.

### 3.14 Wine's PE side as one module (W2, as built)

The alternative to per-DLL modules (§3.13, which stay possible): every DLL
of the PE side linked into one wasm module, as an ejected program is
linked. Cross-DLL calls are direct, and there is one copy of the runtime
and one exception tag.
- **Each DLL as one relocatable object.** winegcc (wasm32) also writes
  `X.dll.so.r.o` (`wasm-ld -r` of the DLL's objects and static libraries)
  and `X.dll.so.exports` (winebuild's list of the DLL's export link
  names). The shared runtime moved out of winecrt0 into `libs/gexrt`,
  which is linked once.
- **`tools/wine-wasm/pesym.py`** rewrites the linking section's symbol
  table of each DLL object:
  - `dll`: the DLL's exports and spec glue become `DLL!name`; every other
    defined symbol becomes local, so private names cannot collide and two
    DLLs may export the same name.
  - `imports`: each undefined symbol that another DLL exports becomes
    `owner!name`, the owner being the first of the DLL's IMPORTS that
    exports it, else any DLL.
  - The stale name section that `wasm-ld -r` wrote is dropped, so the
    final module's function names are the renamed symbols.
- **`tools/wine-wasm/pelink.py`** runs both passes and links everything
  with libgexrt into one PIC module. It works from a Wine build tree, or
  from explicit `--dll NAME R.o EXPORTS [IMPORTS]` inputs. The DLLs' data
  exports are exported as globals (their data offsets); what stays
  undefined is listed beside the module.
- **Skeletons:** `winebuild --fake-module --wasm-shared NAME` writes them
  with no code or data of their own. A `.wasmref` section names the
  module, the DLL and the DLL's data exports (export index, link name).
- **In gexc** (`wasmdll`, shared mode):
  - The first skeleton to map installs the module (its data from the NT
    allocator); every later one binds its exports to the same install's
    slots (`FuncMap::bind`, new and additive).
  - `-import` re-exports are bound to the owner's adapter.
  - Data exports are pointed at the module's data by rewriting the
    skeleton's export address table entries (32-bit RVAs wrap to any
    address).
- **Tested:** `tests/wasm32/dll/shared.sh`. Three DLLs, two of which
  export the same name and each define a private function of the same
  name. One DLL calls another's export directly. A data export reads the
  shared data. All of it from a MinGW i386 program.

### 3.15 ntdll and the boundary at run time (W2, as built)

Native Wine runs inside the guest thread, as Windows' DLLs do:
- **One stack.** Every adapter enters through gexrt's `__gexw_entry`, which
  moves the C stack pointer below the guest's ESP (by 64, aligned to 16).
  SEH frames that native `__TRY` pushes, the TEB's stack limits and
  `is_valid_frame` see one stack. The per-thread globals (context, depth,
  the guest return address `__builtin_return_address` means on wasm32)
  are put back on the way out.
- **Calls into the guest** (`__gexw_guest_call`, the ICALL exit stubs) push
  the arguments below the native stack and run gex's dispatch until their
  own return address comes back (`target ^ 1`, watched as before). A
  fastcall variant sets ECX/EDX (`BaseThreadInitThunk`).
- **Loading a guest state** (NtContinue, NtContinueEx, NtSetContextThread,
  RtlRestoreContext) throws `__gexw_unwind`: every native frame up to the
  innermost `__gexw_entry` goes, and that entry returns 0, so gex carries
  on from the context. A longjmp out of guest frames into a native
  `setjmp` restores the guest ESP and depth the jump skipped (gexrt's SjLj
  runtime keeps them in the buffer).
- **System calls.** ntdll's stubs (`syscall_wasm32.c`, generated per
  argument size from `wine/gex/syscall.h`) call `__gexw_syscall`, which
  writes the words above two return addresses below the native stack (the
  i386 stub shape gex-kernel's NT layer reads), sets EAX and calls
  `gex.syscall` with bit 61 of the pc set (`NATIVE_SYSCALL`, next to
  `RESUMABLE_PC`'s bit 62; merge-plan §9.8). Under that bit a Resume
  writes PC and returns instead of trapping out. If the kernel redirected
  the thread (an APC from an alertable wait), the stub runs it nested
  until it comes back to the syscall's own state; the resume-type calls
  throw instead. Exit, thread exit and NtCallbackReturn still trap out
  (they end host loops).
- **Guest code that leaves native frames** (W2, gooner). A guest handler
  native code called may never return: an i386 catch block continues in an
  outer frame (`continue_after_catch`), an `__except` resumes its function.
  The guest then runs on nested inside the native frames (and the call-out
  holding them), one level per exception, until the call-outs run out
  (32) and the thread livelocks. Three pieces flatten it:
  - gexrt's `run_until` drops the frames (throws `__gexw_unwind` to its
    entry) when the guest stack pointer is at or above the one at the guest
    call (native code's guest ESP is its entry's);
  - an entry made at or above the innermost entry's stack pointer (gexrt
    tracks it per thread, as a hint) issues `gex_nr::NATIVE_LEFT`: the host
    leaves the innermost call-out if it is a native caller's (its return is
    `tramp::NATIVE_RETURN_PAD`) and the guest is above its callee's frame
    (`NATIVE_UNWOUND`, 512 bytes), and the guest calls the function again
    from `tramp::JMP_EAX_32` (`jmp eax`; EAX is free at a call). Re-entering
    at the function's own address would not do: a body leaving for its own
    address has served nothing, and the bridge hands the address to tier-1.
  - a safepoint leaves a native caller's call-out the same way, except at a
    function's own address (the entry about to run does it).
  tests/nt `x86/msvc_cxxeh` (clang-cl i386 C++ and SEH, built by
  tests/nt/msvc/build.sh) throws 20000 times through x86 msvcrt.
- **Exceptions.** A guest fault reaches `KiUserExceptionDispatcher` as
  before (a hand-written entry: no return address), whose
  `dispatch_exception` ends in NtContinue. An exception native code raises
  (`RtlRaiseException`) is dispatched in place: continuing returns, unless
  a handler moved Eip/Esp/Ebp. `RtlUnwind` calls the unwind handlers and
  returns, which is where i386's NtContinue went. Captured contexts are the
  guest's state at the innermost entry.
- **Hand-written entries:** KiUserExceptionDispatcher,
  KiUserApcDispatcher, RtlUserThreadStart (entry in EAX, argument in EBX),
  `_chkstk`/`_alloca_probe`. winebuild skips an export whose `gexp_` the
  objects already define.
- **ntdll's variables and its own base.** gex-kernel's boot writes
  `__wine_syscall_dispatcher`, `__wine_unix_call_dispatcher` and
  `__wine_unixlib_handle` through ntdll's export table, which in a shared
  skeleton points all data exports at one placeholder until the install.
  A native ntdll needs only the handle: wasmdll queues it for the module's
  variable and writes it after the drain's catch-up has instantiated the
  module (its data initialisation would clear it). Unix calls go through
  `__wine_unix_call_wasm32` (gex's private UNIX_CALL system call): the
  i386 trampolines are guest code below the ICALL check's floor (0x400000),
  so a native call through them would be taken for a table index. ntdll
  finds its own image from the guest address `LdrInitializeThunk` was
  entered at (its functions' addresses are table indices), and parses
  `WINEDEBUG` itself when no Unix side has filled the options below the
  PEB (Wine's default otherwise: err and fixme).
- **Forwards across the one module.** `.exports` lists forwards; pelink
  binds a forwarded name to an alias the defining DLL gets
  (`kernel32.dll!HeapAlloc` for ntdll's `RtlAllocateHeap`), or to a
  generated thunk where the declarations differ (kernel32's void
  `EnterCriticalSection` against NTSTATUS `RtlEnterCriticalSection`): wasm
  calls must match exactly. The resolved forwards also go into the module
  as a custom section, `gexw.forwards`, so gex binds a `-import` re-export
  of a forwarded name (kernel32's `HeapFree`, kernelbase's forward to
  `RtlFreeHeap`) to the adapter it reaches. compiler-rt's builtins are
  linked in.
- **A native prefix:** `tools/wine-wasm/stage-native.sh BUILD SRC OUT DLL...`
  links the named DLLs into `winepe.wasm` and writes their skeletons into a
  copy (symbolic links) of the x86 prefix, without its layout pins. The
  other DLLs stay i386 code.

### 3.16 Native Wine in the browser (W2, as built)

- **Install** (`shims/browser/src/cgen/wasmdll.rs`): a skeleton is a
  placed image's only provider (no byte check, no unit). Its publish
  (`gex_wasmdll_publish`) reads Wine's PE module from the prefix through
  the kernel's VFS at the first skeleton and publishes it as a stateful
  provider (data top down from the NT allocator); later skeletons bind into
  the same install (`FuncMap::bind`) and queue their bridges (`BINDS`), which
  every Worker writes once (`#wasmdllCatchUp`, from every `#cgenCatchUp`).
  The publishing Worker's install then writes ntdll's boot variables and
  binds the entry thunks (`gex_wasmdll_installed`).
- **Native system calls:** the module's `gex.syscall` import is JS's
  (`#nativeSyscall`). The shim strips bit 61 and, for a native call, a
  Resume writes the PC and returns (as natively). A trap the shim takes for
  a unit to fetch or a thread to start resumes at the call's own stop
  address: the wrapper serves it (catch-up, `#cgenServe`, `#drainThread`)
  and returns into native code. Other traps (exit, thread exit, a
  callback's return, a resume elsewhere) go on up.
- Needs the units mode on (`--cgen-prefer` at least): the image-mapped hook
  is only set then. Per-DLL modules (`.wasm`) are native only.
- Not yet run: the page itself (Chrome), and threads that start inside
  native code under load.

### 3.17 Process start under W3 (2026-10-09)

The session boot (wineboot, services, plugplay, winedevice, explorer,
rpcss, then the program) took 77 s on edger, 8-12 s for each Wine child.
In 52 s of Undertale's load the program was waiting for it. Where each
child's time went, and the fixes:

| Cost per child (edger, warm cache) | Before | After | Fix |
|---|---|---|---|
| Runtime translations of the i386 DLLs (ole32, rpcrt4, advapi32, combase, shell32, ...) recompiled | 2-3.7 s (about 30 s of compile wall per boot) | 0 | function-tier units, below |
| fork copying Wine's 3 GiB of PROT_NONE reservations (2 forks per child) | 1.0-1.6 s each | 0.03-0.05 s | `MemoryMap::clean` + a pagemap-guided sparse copy |
| anonymous mmap zeroing those reservations | 24 GiB per boot | 13 MiB | the same `clean` ranges |
| the image key (Debug text of the options, which held the whole 5.7 MB ELF) | 0.4-1.4 s | 0.01 s | `xlate_identity` + an in-process memo |
| Wine's PE module read, hashed, parsed, loaded | 0.3 s | 0 after the first | one engine per host process, `wasmdll::parsed_module` |
| the loader (wasmprog) compiled | about 2 s | 0.05-0.1 s | its cwasm in the module cache |
| getaddrinfo of the host name (wineboot) | 5 s, once | 0 | poll skips fd < 0; recvmsg names the sender |

**Function-tier units for Wine's i386 DLLs.** Tier-1's units are not
reusable across processes: each holds the page-table bases and the
"already translated" set of the process that made it, so their wasm, and
so their cache keys, differ per process. About 930 new module-cache
entries were written per warm boot. Wine's ntdll.so already reports every
image it maps (`gex.image_mapped`, after `map_image_into_view`). For an
i386 image (not a skeleton), `nt_cgen::w3_image` now installs the
function-tier unit built for its bytes at its base, as the NT layer's
images get theirs. The unit's compiled code then comes from the module
cache in every process. An image Wine moves (its relocations come later,
from ntdll's PE side) mismatches at map time and is placed at its first
miss instead (`w3_miss`). A mismatch then, or a missing unit, leaves it on
tier-1, which is never fatal.

Units are built at staging. Run once with `GEX_W3_TRACE=1`, then
`$W/cgen-w3.sh GEXC LOG` runs `gexc cgen-image PATH --pe-base BASE` for
every "no function-tier unit" line, with `GEX_CGEN_PRECOMPILE=1` so it
also compiles each unit's `c.cwasm` for that host. Run it on the machine
that runs the program. The session boot's 53 images take 9 minutes on
gooner (12 jobs) and 66 MB of units. Undertale's 35 more take 21 minutes
and 105 MB.

| Variable | Effect |
|---|---|
| `GEX_W3_UNITS` | unset: Wine's own images only (its dll directory); `all`: the program's images too; `0`: no units |
| `GEX_W3_UNITS_SKIP=a.dll,...` | leaves those images on tier-1 |
| `GEX_W3_UNITS_BUILD=1` | builds missing units in the background for the next run |
| `GEX_W3_UNIT_MAX_BODY` | the run-time compile limit below, in bytes |

**Compiling a unit is kept off the guest's path.** A unit with no cwasm
for this host yet is compiled on one background thread of the host
process ("gex-unit-cc"), smallest unit first. Until it is ready, that
process runs the image on tier-1. A unit whose largest function body is
over 640 KiB is never compiled at run time, only at staging.

The limit exists because of d3dx9_43's unit. Its one 828 KB function
takes Cranelift's register allocator about ten minutes. Compiled at map
time, it stopped Undertale's main thread for the whole run, so the game
never reached a frame. Size is only a rough guide: wined3d's 444 KB
function compiles in seconds.

`nt_cgen::prepared` remembers, per host process, what placing an image
needs: its relocated segments, unit key, cwasm and entries. The unit
key's SHA-256 had been about 19% of a session boot's samples.

Result (edger, `wine cmd /c echo`, warm):

| Build | Boot time |
|---|---|
| e (before) | 76.9 / 76.8 s |
| j (units, plus the earlier fixes) | 15.4-16.2 s |
| k (j, plus the engine, PE-module and image-key fixes) | 9.4-9.9 s |
| k with `GEX_W3_UNITS=0` | 31.0 s; 11.5 s when tier-1's per-process keys happen to repeat a previous run's |

A child now starts its Wine PE side 0.25 s after its fork.

**In the page.** The browser's fork slices every VMA (about 2.6 GiB per
fork) and `writeSparse` scans it in the child. The clean ranges need
exporting to the page's fork (the VMA blob) before that cost goes. The
browser has no `gex.image_mapped` for W3 yet. Its units path
(`shims/browser/src/cgen.rs`) takes images only from the NT layer's
section hook. Wiring the import to push a `Pending` gives the page the
same unit placement, with units fetched as assets by key. The kernel fixes
(clean ranges, poll, recvmsg) are shared code.

**Wine's thread loop and safepoints (native only).** Every W3 thread but
the main one runs the program under `gex_call_out(ctx, pc, 0, 0)`
(signal_wasm32.c), a call-out that never returns. The safepoint import
abandoned the innermost call-out once the stack was more than 64 KiB above
its `sp_want`; with `sp_want` 0 that was every safepoint. So such threads
trapped out and re-entered every 1024 back-edges. wined3d's command-stream
thread then ran mostly trap round trips (57M abandoned call-outs in 80 s,
ut30). With wined3d's unit, Undertale drew no frame in 360 s; on tier-1 it
was only slow. The fix: a call-out with `ret` 0 is never abandoned at a
safepoint. The page's call-outs (`shims/browser/src/cgen.rs`) have no such
safepoint check, so this one does not carry.

Tools added with it:

- the `GEX_STALL` dumps give each thread's call-outs (count, how many were
  abandoned, the last target and return);
- `GEX_TRACE_CALLOUT_ABORTS=N` prints the first N abandoned call-outs, with
  the reason and the stack;
- `GEX_CGEN_KEEP=1` keeps a W3 unit's generated C (`<cache>/<key>/work`).

**Session start-up against the NT layer (2026-10-09).** On edger, `wine
cmd /c echo` with the merged gexc and its re-keyed units (gamekit
`boot-h.sh`, gexc-u, warm caches) takes 8.3-8.5 s and starts 7 processes.
The NT layer runs a console program in 2.3 s. The W3 session is serial: one
process at a time, each about 1 s on one core. The sequence:

1. the loader;
2. `wineboot --init` (Wine runs it for every new session; its own work
   before it starts services is the longest single step);
3. services.exe;
4. a winedevice for MountMgr;
5. Plug and Play and the Event Log (svchost);
6. a second winedevice (NDIS and the NSI proxy, which wineboot's
   `initialize_internet` asks for);
7. cmd.

Measured on edger:

| Variant | `wine cmd /c echo` | Processes |
|---|---|---|
| Kit prefix (default services) | 8.3-8.5 s | 7 |
| `tools/wine-wasm/lean-session.reg` (Eventlog, NDIS, nsiproxy, PlugPlay demand-start) | 6.2-6.7 s | 5 |
| A second program in the same warm session (cmd starting cmd) | +0.8-1.0 s | +1 |

So the lean prefix takes 2.1 s off a session start, and a reused session
would cost about 1 s per program: under the NT layer's 2.3 s. Both carry
to the page:

- the lean registry is plain prefix configuration, not code;
- a reused session means the page keeps its kernel (wineserver and
  services) between programs in a tab.

Natively, one gexc runs one session, so a reused session there would
need a server mode.

`<cache>/w3keys` (72eb5dac) remembers each image's unit key by file and
base. The first process to map an image no longer computes two SHA-256s
of it. That was a third of wineboot's host CPU (perf on gooner); edger
time not measured separately.

**Session start-up, round 2 (2026-10-10).** Three fixes changed which
services really start, and so the session's shape:

- pelink (bd98b866) binds names that an i386 DLL defines to that DLL when
  the module's explicit imports list it ahead of the native one
  (ntoskrnl's `ZwLoadDriver`, `ZwUnloadDriver`, `RtlInitUnicodeString`,
  `RtlNtStatusToDosError`, `wcscat`). Before, winedevice's drivers never
  loaded at all. Now nsiproxy and NDIS load. MountMgr, winebus, wineusb
  and winebth fail with STATUS_DLL_NOT_FOUND: they need Unix libraries
  that the W3 loader does not have.
- With drivers loading, the default kit prefix ran 4 winedevice processes
  and booted in 25-37 s. NDIS hung for 10 s on "wine client error: write:
  Resource temporarily unavailable". gex-kernel's `blocking_op` parked a
  full-pipe `write` but not a `writev`, which is what Wine's
  `send_request` uses. b8f708f7 parks the first non-empty vector as a
  `WriteStream`.
- `tools/wine-wasm/w3-session.reg` is now the default for every W3
  prefix. It makes MountMgr and the Event Log demand-start; Wine's
  `ReportEventW` is a stub, so nothing calls the Event Log. It also
  deletes the three root bus devices (`Enum\ROOT\WINE\WINEBUS`,
  `WINEUSB`, `WINEBTH`).
  - wineboot's code creates those devices, not wine.inf. Without a fork
    patch, they can only be deleted after the install.
  - `tools/wine-wasm/reg-apply.py PFX/system.reg FILE.reg` applies a
    REGEDIT4 file offline. It is idempotent.
  - stage-native.sh runs it when the staged prefix has a registry. A
    prefix that wineboot installs afresh runs it once after the install.
    The page's W3 prefix template should take it the same way.
  - Plug and Play, NDIS and the NSI proxy stay on: games need device
    notifications and adapter information, and nsi.dll does not start
    nsiproxy on demand. `lean-session.reg` also drops these three. It
    stays opt-in, for one-shot console programs.

Measured on edger, gexc at b8f708f7 (gamekit `ut36.sh`, `ut37.sh`, warm, units rebuilt for pelink's
load order):

| Variant | `wine cmd /c echo` | Processes |
|---|---|---|
| Before round 2 (kit prefix, gexc-u) | 8.3-8.6 s | 7 |
| w3-session prefix (default) | 5.6-5.85 s | 5 |
| no drivers only (w3-no-drivers.reg, Event Log still on) | 5.75-6.6 s | 6 |
| `lean-session.reg` too | 3.7-4.1 s | 3 |
| lean, cmd.exe by path (no start.exe) | 3.0-3.4 s | 2 |
| lean, a second program in the session | +0.3-0.8 s | +1 |

**The gap left to the NT layer's 2.3 s** is mainly the session itself:
the NT layer runs cmd directly, with no wineboot and no services. In the
lean, by-path case (3.0-3.4 s) the logs give:

- about 1 s of wineboot's own `--init` work: the registry, hardware keys
  and its DLL loads;
- 0.5-0.8 s for services.exe;
- 0.35-0.5 s for each process creation;
- gexc's host start.

Trimming the registry further gains little. A reused session removes
wineboot and services for every program after the first, at about
0.3-0.8 s per program. In the page, that means the page keeps its kernel
(wineserver and services) between programs in a tab. Natively it would
need a gexc server mode.

**Session reuse in the page (design, 2026-10-10; not built).** This needs
W3 in the page (W3.7) first. Under W3, wineserver is a thread of the first
Wine process (`server/wasm32_inproc.h`). The session ends when that
process exits: services, the desktop and the registry flush go with it.
The page's global kernel already outlives programs (`runtime.js`,
`globalKernel`), so a session can outlive one program if something keeps
the server's process alive.

1. **A session keeper.** The first W3 program in a tab for a prefix is not
   the program itself. It is `gexsession.exe`, a small native Wine program
   run as pid 1 of the session. It hosts the server thread and the
   desktop. Wine's normal start runs `wineboot --init`, services and the
   w3-session.reg set, once. The keeper then listens on a Unix socket in
   the global kernel: `/run/gex-w3/<prefix>.sock`.
2. **A program in a running session.** The page's `runProgram` for W3
   connects to the socket instead of starting a process tree. It sends:
   - the command line, the working directory and the environment;
   - the terminal's fds (SCM_RIGHTS, which gex's recvmsg already carries).

   The keeper calls `CreateProcessW` with those as the child's standard
   handles, waits, and replies with the exit code. The child is a
   process like any W3 child: it reads the session through requests
   (`tests/nt` childgui and childcon). It costs one process creation plus
   the program's own start. Natively that is 0.3-0.8 s (lean session,
   second program, edger), against 3.0-4.1 s for a fresh session.
3. **Games stay the server's process.** A child pays for its requests: a
   full 256-key poll is 1.3 ms a frame in a child, against 0.2 ms in the
   process hosting the server. So a game launched from the page's game
   list still starts its own session, as today. Only console programs,
   tools and installers go through the keeper. A game started from inside
   a session (a launcher, an installer's "run now") is a child either way.
4. **Lifetime.**
   - The keeper holds the session while the tab is open, and ends it
     after an idle timeout (no child for N minutes) or when asked.
   - Registry: Wine's server already saves the registry periodically. The
     page also asks the keeper to flush before `persist.js` snapshots the
     prefix, and on `pagehide`.
   - The page holds two resident Workers (the keeper's process and
     services.exe), each with its own memory, for as long as the session
     lives.
5. **Natively** the same keeper works under one long-running gexc, with
   the socket on the host. That would be the gexc server mode. It is
   lower priority: the browser is the target.

Pieces to build, in order:
- `gexsession.exe`: a native PE-side program in pelink, about 200 lines:
  socket, CreateProcessW with inherited handles, exit codes;
- the page's W3 `runProgram` path, which tries the socket first;
- a flush request;
- tests: a `tests/nt` leg that runs two programs through one keeper and
  checks the second skips wineboot (no `wineboot` process in a
  `+process` trace).

**Undertale with these fixes (ut34-ut36).** GEX_PROF of the main thread
in ut34 put x87 soft-float at the top. Two changes followed:

- 028e7a91: x87.h handles zero operands (signs and rounding as the
  hardware gives them) and `fist` of 0 < |v| < 1 inline. Both were
  checked against hardware: 3.16M arithmetic cases and 2M integer
  conversions, none different.
- 3d423c45: windowscodecs' movsb/cmp/jne byte-copy loop is lifted as one
  string copy.

gex.x87 import calls in 240 s fell from 174M to 22M. The rest come from
tier-1 callers, which have their own `emit_x87` without these paths. The
largest groups are zero + normal (9.7M) and narrowing a zero to single
(7.5M). The top back-edge samples are now wined3d's command-stream spin
and two windowscodecs loops.

| Run (edger, 300 s) | First frame | Steady 30 fps from | Last frame |
|---|---|---|---|
| ut34, ut36 w1: gexc-w, w3dist-u, kit prefix | 14.9 s | 120-121 s | 5525 |
| ut36 z1, z2: b8f708f7, w3dist-x (pelink), no drivers | 12.1-14.4 s | 105-106 s | 5950-6025 |

So steady frames arrive about 15 s earlier than with ut34's setup.

**Units follow the prefix's registry.** On the w3-session prefix (ut37),
52 of the game's DLL images loaded at bases that no unit had been built
for. The game process's load order differs from pfx-nodrv's, so it ran
mostly on tier-1: steady frames only from 215 s. With the x87 work in,
`ut38` measures again once units for those bases exist
(`$W/edger-units/build-s.sh`, from `pairs-s.txt`). So a prefix template
and its units have to be staged together: in the page, each template has
its own unit set.

9fe3e898 adds the same zero paths to tier-1's `emit_x87`:
- a zero with a zero or a normal operand, in add, subtract and multiply;
- a zero stored narrow.

`tests/x86-programs/x87-zeros.c` checks every sign rule and rounding
mode against native.

**W3.7: Wine's own processes in the page (2026-10-10).** `wine cmd /c
echo` now runs in the browser runtime under Node (gooner, diagnosis only):
wineserver is a thread of pid 1, and wineboot, services, plugplay,
winedevice and cmd are each a Worker. Every Windows process is a double
fork and an `execve` of the wine loader, so 10 forks in all. Edger
Chromium timing is still to do. What it took, in start order:

- **Socket pairs and descriptor passing cross Workers.**
  - A `SOCK_STREAM` socketpair is made in the global kernel
    (`k_sock_pair`) whenever there is one. Wine keeps one end of a new
    process's pair and hands the child the other.
  - `SCM_RIGHTS` describes each descriptor as text and queues it as a
    token beside the bytes (`k_sock_write_fds`, `k_sock_read_fds`):
    - a pipe end opened for no one;
    - a duplicated global file (`#id:path`) or socket (`sock:id`);
    - a path.
  - The token's reference keeps the object alive in flight. The
    receiver rebuilds the descriptor (`scm_rebuild`). A token dropped
    unread releases its reference.
  - Fork carrying (`carry_name`, `rebuild_carried`) uses the same
    descriptions.
- **The Windows address space split, early.**
  - A W3 program's Worker grows its memory to 2 GiB and puts the shim's
    heap above 0x80000000. This happens before `gex_worker_init`: JS
    reads the GEXWASM header's i386 flag (`isW3Program`).
  - The process's arena is then `[0x0b000000, GUEST_LIMIT)` plus the
    low window. Splitting after the startup heap left the arena starting
    at 0x10750000, and Wine's fixed map at 0x10000000 failed.
  - A forked child adopts its parent's split (`gex_adopt_windows_space`)
    before its worker stack is claimed. An exec keeps the split it
    already has.
- **Wine's PE module per process.** When Wine's loader maps a skeleton,
  `gex.image_mapped(base, fd)` publishes it:
  - the first skeleton instantiates winepe.wasm, with null `GOT.func`
    entries and trapping stubs for the three C `main` names;
  - later skeletons bind their exports;
  - each DLL's variables are written at its first mapping only.
  - The ICALL entry thunks include `__gexw_entries_decl`, as natively:
    a function pointer that winepe hands to i386 code (rpcrt4's) is a
    table index below 0x3f0000.
- **Data the process did not write reads as zero.** Provider data
  (`linux_alloc`) zeroes its dirty pages, as an anonymous `mmap` does. A
  grandchild's memory holds its forked parent's bytes until something
  maps over them. winepe's bss sat at the same address in wineboot and
  in services.exe, so services.exe inherited wineboot's cached
  dispatcher indices and hung.
- **The Unix dispatcher ops** (`gex.unix_dispatcher`):
  - 0/1: the system-call switch;
  - 2/3: the guest Unix-call hook;
  - 4/5: `__wine_unix_call_dispatcher`;
  - an even op sets, the odd op after it gets, as natively. winepe
    caches ops 1 and 5.
  - A guest syscall with EAX = `gex_nr::UNIX_CALL` calls the op-3 hook
    through the new `ftab_call_vi` import.
  - `gex_nr::NATIVE_LEFT` is served as the NT layer serves it
    (`nt::native_left`).
- **The probe cache is reset** at an exec and in a forked child
  (`fcache_reset`). Otherwise the cache holds another image's indices.
- **One boot instant for every process.**
  - gex-kernel based CLOCK_MONOTONIC at each process's first reading
    (uptime 1000 s), so every Worker had its own clock.
  - Wine clients give wineserver absolute monotonic deadlines. With
    separate bases, services.exe's 10 s wait for a new service process
    timed out at once, and every service "failed to start" (1053).
  - The global kernel now holds the base (`k_boot_secs`, set through
    `share_monotonic_base`).
- **The page's prefix template** is an installed prefix, not a staged
  one. `wineboot -u` puts plugplay.exe and system32/drivers there.
  - The template takes w3-session.reg via `reg-apply.py`.
  - `.update-timestamp` is the dist's wine.inf mtime, so wineboot does
    not update again.
  - `tools/wine-wasm/page-prefix.sh INSTALLED_PFX DIST OUT` builds it
    (`GRAPHICS=null|wayland` sets the driver). `$W/page/pfx4` was built
    from gate-w3boot/pfx-w3install. `run-node.mjs --dir` now carries
    directory symlinks (`dosdevices/c:`) as links.

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

### 4.0 W3 plan (2026-10-06, the user's order: W3 before W7)

**Goal:** i386 Windows games on Wine's own Unix side, natively and in the
page, with no NT layer: ntdll.so, wineserver, win32u.so, a display
driver, opengl32 through gex-gl, and a real registry (`system.reg`,
`user.reg`) written by `wineboot`. Today's native path (W2) is Wine's PE
side as native wasm over gex-kernel's NT layer: every NT system call, the
registry (synthesized), windows and hooks (the NT layer's win32u calls)
and GL (the NT layer's command ring) are still the NT layer's, which is
where G's findings come from.

**Acceptance cases** (G runs them on copies of the W prefix and sends
repros):
1. **Retrocycles:** `CoCreateInstance(CLSID_DirectSound)` succeeds (the
   class is registered by `wineboot`'s own registration), and GL texture
   uploads over 8 MiB work (no command ring: opengl32.so calls gex-gl).
2. **Katana ZERO:** `SetWindowsHookEx` (win32u.so's real hooks), then to
   its menu by keyboard.
3. **Undertale** to gameplay, as G's script drives it today.
Plus the standing gates: tests/nt on the new path, W's checks.

**What exists:** the Unix side nearly compiles for wasm32 already (W1's
`make -k`): ntdll/unix fails only on `current_machine` and the CPU-feature
helpers (`system.c`); wineserver ran in W0(a) as a wasm32 program; ws2_32's
unixlib ran through the dlopen registry (§3.12).

**Steps** (agent time):

| step | work | done when | estimate |
|---|---|---|---|
| W3.0 process shape | Decide and spike how a Wine process runs: (A) the wasm32 `wine` loader is the process (a wasmprog) and runs x86 code through a gex `simulate(ctx)` import (Wine's BTCpu contract, as FEX/Box64), x86 faults reaching ntdll.so's handlers as signals to a wasm32 program; or (B) keep today's x86 NT boot and send the PE side's system calls to ntdll.so linked into the process, its Linux calls through the wasm32 ABI. A retires the NT boot itself; B reuses more of W2. Expected: A. | hello.exe's first NT call reaches ntdll.so on the chosen shape | 1–2 d |
| W3.1 ntdll.so | `current_machine`, CPU features, `signal_wasm32.c` (Unix side: the syscall dispatcher as a direct call, contexts from gex's, user callbacks nested, APCs, exceptions from gex faults), `virtual.c` on gex-kernel's mmap (PE images mapped executable, so gex sees the code), threads (pthread plus a guest context), process start through Wine's loader; wineserver in process (`clone(CLONE_VM)` without `CLONE_FILES`, §3.5) | tests/nt hello, sync, tls, seh, apc, cxx on the new path (`--wine unix`) | 5–7 d |
| W3.2 prefix and registry | `wineboot --init` natively: setupapi, rundll32, regsvr32 and the builtin DLLs' self-registration, writing `system.reg`/`user.reg` through wineserver | Retrocycles' dsound class registered; a prefix made from nothing | 1–2 d |
| W3.3 windows and input | win32u.so plus a display driver: winewayland.drv to gex's compositor (libwayland-client and libxkbcommon built for wasm32), or a small gex driver if that is cheaper | tests/nt's GUI tests; Katana ZERO's hooks and menu | 4–6 d |
| W3.4 GL | opengl32.so through winewayland's EGL to gex-gl (natively the host GL, in the page Mesa wink); wined3d native | Retrocycles' uploads over 8 MiB; Undertale's GL | 2–3 d |
| W3.5 audio, sockets, crypto | a gex audio driver behind winmm/dsound (Web Audio in the page; first a null device so games start), ws2_32 and crypt32/secur32 unixlibs | Undertale to gameplay with sound | 2–3 d |
| W3.6 sync | ntsync in gex-kernel (`/dev/ntsync` on the futex machinery), so waits never reach wineserver | job-heavy games (Noita) no slower than W2 | 2–3 d |
| W3.7 page | the same in the page: wineserver's thread in a Worker, the Unix side's modules per Worker | Retrocycles, Katana ZERO and Undertale in the page | 3–5 d |

**W3.1 progress (2026-10-06, merge/w3 + fork `wasm32-w3`):**
- **Fault delivery (done natively):** `gex.fault_hook(idx)`; a memory
  access past the guest memory under a call-out, an illegal instruction,
  int3, a divide error or a #GP in the program's code calls ntdll.so's
  `__wine_gex_fault(ctx, code, addr)` with NTSTATUS codes, which sets up
  KiUserExceptionDispatcher as i386's signal handlers do; the call-out
  resumes there with the native frames kept (`spike-w3/fault.c`, all four
  kinds). PC for a memory fault is the block's start.
- **The dispatcher:** `ntdll/unix/signal_wasm32.c` (i386's frame, context
  and dispatcher-stack code, with gex in place of signal contexts and
  assembly): PE system calls arrive from gexrt (`__gexw_syscall`, which
  asks `gex.unix_dispatcher`) in the i386 stub's stack shape, run the
  service by table and argument bytes, and write the frame back; user
  callbacks run KiUserCallbackDispatcher under `gex.call_out` until
  NtCallbackReturn; threads start at LdrInitializeThunk (i386's
  init_syscall_frame). Unix calls go straight through (one memory and
  function table).
- **The loader runs:** Wine's `loader/main.c` and ntdll's Unix side link as
  one wasm32 program (`$W/link-w3.sh` → `$W/wine-w32-w3/dlls/ntdll/
  wine-w3`, container flag `--gex-guest-i386`: i386 translation in the
  process, no mapping above 0x7ffe0000). `wine --version` works; a run
  starts wineserver (a wasm32 program, spawned) and talks to it.
- **hello_args.exe runs on Wine's own Unix side** (2026-10-06): PE ntdll,
  kernel32, kernelbase and ucrtbase are the native wasm skeletons, bound by
  `gex.image_mapped` when ntdll.so maps them (Wine's virtual.c tells gex
  each image it maps); exit 42 and the expected output. What it took: the
  install layout (`$W/w3dist`: `lib/wine/i386-unix/wine`,
  `lib/wine/i386-windows` with the skeletons plus every other i386 builtin
  of the master prefix as symlinks, since ntdll.so reads apisetschema.dll
  there; `share/wine/nls`; `bin/wineserver`), the loader as its own
  `wineloader`, and each thread's kernel stack top-down (gex's dispatch
  directory takes 0x100000-0x300000, so bottom-up it landed on 0x400000,
  the exe mapped elsewhere, and the loader fell back to start.exe).
  Run: `gexc run --env WINEPREFIX=$W/pfx-w3 $W/w3dist/lib/wine/i386-unix/wine
  X.exe` (`$W/gexc-w3` wraps that for tests/nt/run.sh; `GEX_W3_TRACE=1` in
  gexc's own environment lists the bindings). wineboot.exe is not in the
  master prefix, so every start warns "failed to start wineboot" (W3.2).
- **tests/nt i386 on this path** (`$W/nt-run-x86.sh $W/gexc-w3`, log
  `$W/w3-nt2.log`; the pe-x64 cases in that run are W7's): **24 of 40**
  (2026-10-06, up from 20): hello_args, sync, mtx, crtmem, lowmem, apc,
  tls, cxx, icall, fixedbase(-taken), loadorder-native/-disabled, zfcall,
  window, pagespan, exitproc, seh, seh-traps, hello32, msvc, msvc_cxxeh,
  msvc_x87, msvc_largeint. G's repros: unixpath and iocp pass; sysmisc has
  only the two SPI_GET checks left (W3.3); hooks and keyname are W3.3.
  What it took, beyond the first run:
  - **wineserver in process (§3.5, done):** linked into the loader and
    started by `start_server` as a thread (fork: `server/wasm32_inproc.h`,
    force-included into the server objects by `$W/link-w3.sh`, renames
    the eight symbols both sides define and makes the server's `main` and
    exits `wineserver_main`/`wineserver_exit`; the thread ends instead of
    the process). It has its own working directory through
    `unshare(CLONE_FS)` (new in gex-kernel: a thread's cwd swapped into
    the VFS for its calls), shares the descriptors, takes no SIGCHLD (its
    waitpid(-1) stole ntdll.so's children: a third of the starts hung),
    and maps the user shared data at 0x7ffe0000, the client's address (a
    wasm memory cannot map one page twice; gex hands a second `MAP_SHARED`
    of the same file the first one's pages). The tick count now moves.
    The nls directory is found beside the loader
    (`lib/wine/share -> ../../share`, in `$W/stage-w3.sh`).
  - **exceptions:** the fault hook's C frames go below the guest stack
    pointer with a red zone (the shared shadow stack pointer could sit
    above the live guest stack and the hook overwrote the SEH chain it
    walked); an int3's address is its own; #GP at a privileged
    instruction is EXCEPTION_PRIV_INSTRUCTION (i386's
    `is_privileged_instr`); a branch to memory that is not executable is
    an access violation with kind 8 (the hook got a fourth argument);
    gexrt's NATIVE_LEFT is taken without the NT layer (a C++ catch through
    wasm msvcrt nested a call-out per throw until the limit); the NT
    layer's trampoline page (jmp eax, the vtable wrappers) is mapped at
    0x3f0000 in a W3 process.
  - a new thread's start function at pthread's type; i386 mlock as an
    advisory no-op.

  **Then 29 of 40** (`$W/w3-nt4.log`, same day): suspend, msvc_msvcp,
  detour, jitcode, exitproc and loadorder-builtin too, from:
  - **signals:** SIGUSR1 (suspend), SIGQUIT (thread abort) and SIGINT
    handlers as i386's. One arriving in guest code is taken at a gex
    safepoint: gex-kernel builds i386's frame on the guest stack and gex
    runs it through `gex.signal_hook` (libgex's frame runner; the handler
    gets the frame's ucontext, the guest's state). One arriving in Unix
    code is taken when a system call returns, with the syscall frame's
    state. Threads unblock the server's signals before entering the
    program.
  - **the Unix side's stack and registers:** system calls from the PE
    side run on the thread's own stack below its syscall frame (an
    assembly switch), not on the guest stack just above the arguments
    where callback and APC frames are written; a Unix call puts the
    program's registers back after (every system call the Unix side makes
    passes arguments in EBX..EBP of the same context: msvcp140 lost a
    return address to it).
  - **code changes:** a fixed mmap over code retires it (Wine decommits
    by mapping over), making code writable retires it, and
    `gex.code_changed` serves NtFlushInstructionCache.
  - **thread exits:** the first thread's `exit` while others run no
    longer ends the process (Linux semantics; ExitProcess on another
    thread lost its exit code).
  - **file mappings:** a shared mapping replaced by a fixed mmap ends as an
    unmap (written back, no longer aliased), and write-back stops at the
    file's end.
  - **Unix libraries:** crypt32, ws2_32, msv1_0, dnsapi and nsiproxy's
    Unix sides are linked in (`$W/link-w3.sh`, renamed apart, dlopen
    registry, §3.12); i386 code reaches them through the trampoline page's
    Unix-call entry, which gex hands to ntdll.so (`gex.unix_dispatcher` op
    2). For now wasm callers take the same route (a guest round trip per
    Unix call); a direct path for them needs a winepe rebuild.

  Open, by cause:
  - **the registry (W3.2):** there is none until wineboot runs, and
    crypt32 then spends minutes failing provider lookups (crypt). The
    wasm wineboot.exe now links (programs in the one module: winegcc,
    winebuild's program entry, the CRT table markers; stage-native stages
    a program) and starts, but **a native module's dependencies are not
    loaded**: skeletons have no import table, so the loader never loads
    and initialises what the module calls (ucrtbase's heap is 0 in
    wineboot), and calls to i386 DLLs (`owner!name` left by pelink) have
    nothing to bind them without the NT layer. That is W3.2's first item:
    skeletons that carry their module's imports (or gex loading them at
    install), then wineboot, then the real system.reg.
  - lownull-* (§5.0, low-page protection; not to be fixed here);
  - filemap: change notifications (FindFirstChangeNotification) need
    inotify in gex-kernel and Wine built with it;
  - pipe: SystemSupportedProcessorArchitectures says i386, as 32-bit
    Windows does; the test (and the NT layer) expects WoW64 on x86-64;
  - win32u's Unix side (W3.3): user_win, gdi, input, display, gl*.

**W3.2 progress (2026-10-06, native DLL dependencies, wineboot):**
- **A native module's imports are loaded:** pelink writes each module's
  load-time imports (Makefile IMPORTS plus makedep's default C runtime,
  kernel32, ntdll) into `__gexw_module_imports`, and Wine's PE loader on
  wasm32 loads those before a skeleton without an import table, as an
  import table's would be (fork: ntdll/loader.c fixup_imports).
- **Programs in the one module:** winegcc leaves a program's r.o and
  export list, winebuild's entry adapter calls `mainCRTStartup(void)`,
  gexrt defines the CRT table markers (`__xi_a`..), and crt_main calls
  clang's `__main_argc_argv`/`__original_main` (a 3-parameter `main`
  call trapped). `$W/relink-prog-w3.sh` relinks a program with the W3 host
  tools (`$W/wine-tools-w3`); `W3_PROGRAMS="wineboot.exe services.exe
  plugplay.exe conhost.exe winedevice.exe" $W/stage-w3.sh` stages them.
- **Child processes by clone:** fork() is asyncify's unwind and rewind of
  the whole wasm stack, which cannot cross gex's frames and the PE module
  under a system call (the forking process went on half rewound, its next
  indirect call trapping). spawn_process, fork_and_exec and
  `__wine_unix_spawnvp` start children with `clone(CLONE_VFORK)` and exec.
- **Unix calls:** wasm callers go straight to the native dispatcher
  (gexrt recognises the trampoline address; `gex.unix_dispatcher` op 4/5),
  i386 callers through the trampoline; stateful installs (the program,
  the PE module) are providers, so relocating a skeleton or an mprotect
  over it retires nothing.
- **Threads:** RtlCreateUserThread wraps a native void entry (the
  threadpool's), since every entry is called as LPTHREAD_START_ROUTINE.
- **Registry:** the in-process wineserver saves the registry when the
  process exits (exit_process asks it to flush; its periodic save never
  came). `system.reg` and `user.reg` are written.
- **pelink:** i386 owners from a .spec when a DLL's wasm32 build failed
  (rpcrt4: wineboot's UuidCreate was an undefined, trapping function);
  guest-import stubs get entry thunks (stdcall by the owner's .spec), as a
  native module may hand one to i386 code (services' RPC dispatch table
  holds rpcrt4's NdrServerCall2); the ICALL pass's annotation sections
  are stripped before the final link (wasm-ld 22 crashed on services.exe
  and plugplay.exe).
- **Entry thunks where a function is handed out but not defined:** widl's
  server stubs (svcctl_s.c) list routines another object defines, so
  i386 rpcrt4 called table indices with no thunk. The ICALL pass now
  makes a thunk for an escaping declaration too (`gexw_entries_decl`,
  bound only where no defining object gave one); the plugin is built into
  `$W/sr-w3` (a hard-linked copy of sr-slim with the new GexIcall.so).
  wmainCRTStartup calls `wmain(argc, argv)` on wasm32 (winedevice).
- **Where it stands:** wineboot runs services.exe (RPC to it works),
  winedevice.exe and plugplay.exe natively, and rundll32's wine.inf
  `PreInstall` completes (services installed, registry written and saved
  at exit). Then wineboot opens its "updating" dialog
  (`show_wait_window`: CreateDialog) before `DefaultInstall`, which needs
  win32u's Unix side: it does not come back, so the full install (and the
  registry it writes) waits for **W3.3**, and every start still waits out
  the boot event. The suite's prefix stays without the native programs
  until then (`$W/w3-nt5.log`: 29 of 40, no change).

**W3.3 progress (2026-10-06, win32u.so):**
- win32u.so's Unix side is linked into the loader (`$W/link-w3.sh`:
  the twelve names it shares with wineserver and ntdll.so renamed apart,
  `__wine_unix_lib_init` in the dlopen registry); win32u's DllMain loads
  it on wasm32 (its dispatcher check saw ntdll's variable), each native
  DLL keeps its own `__gexw_image_base` (pesym had shared one, never
  written: unix-call init asked for module 0). NtUser*/NtGdi* calls now
  reach win32u.so.
- The loader references no fork any more (the in-process server never
  daemonizes), so it is not asyncify-instrumented: smaller, and nothing
  can be left half rewound. A call-out's return is never watched in the
  miss slots (gex: the trampoline page's pad, translated first).
- **The session's shared memory, resolved with option (a)** (the user's
  choice, 2026-10-06): win32u (wasm32) reads the session's blocks from the
  in-process server's mapping (`wineserver_session_data`, one 16 MiB
  block), and explorer's desktop runs as a thread of the application
  (`gexp_desktop_thread`, which win32u's `get_desktop_window` starts once
  per process). Both are in the inventory below.
- **The desktop thread, made right:** it creates the desktop window
  itself, as explorer does (it owns its WND: IsChild, the window tree and
  messages to the desktop work). The thread is created suspended so the
  starter knows it: its own `get_desktop_window` before the window exists
  returns at once (it had waited out 10 s, after which the application
  forced a desktop window of its own). Callers wait until explorer names
  the shell window, just before its message loop (a WM_NULL was answered
  in explorer's sent-message waits during set-up, before the desktop had
  its size). explorer's `ClipCursor`/`SetCursor` imports were trapping
  stubs: winebuild's export list now names a renamed `-import` export as a
  forward (user32's ClipCursor is win32u's NtUserClipCursor).
- **FreeType for wasm32** (pinned: **FreeType 2.13.3**,
  `freetype-2.13.3.tar.xz`, sha256
  `0550350666d427c74daeb85d5ac7bb353acba5f76956395995311a9c6f063289`,
  fetched from the SourceForge mirror, savannah being unreachable from
  gooner): a static build with no zlib/png/bzip2/harfbuzz/brotli into a
  directory of its own, `$W/ft/install` (`$W/ft/build-ft.sh`; not in the
  sysroot, which shares files with sr-slim). `$W/link-w3.sh` compiles
  win32u with `HAVE_FT2BUILD_H` and `SONAME_LIBFREETYPE` (no reconfigure),
  links `libfreetype.a` and registers the 31 `FT_*` functions win32u
  dlsym's under `libfreetype.so.6` in the dlopen registry. Wine's own
  fonts (`fonts/*.ttf`) are staged in the data dir's `fonts/`
  (`$W/stage-w3.sh`). Tahoma realizes (13 px: height 16, ascent 13).
  Before it, no font could be realized and every window's minimum track
  size was stack garbage (a 640-wide window came out 4204 wide); a
  stopgap fork patch for that was reverted once FreeType was in.
- Left in the i386 suite (`$W/w3-nt9.log`, 32 of 40, FreeType in):
  `pipe` fails only its SystemSupportedProcessorArchitectures check,
  which wants AMD64 first (the NT layer's x64 host); W3's Wine is
  i386-only (§3.9), so that is an expectation to revisit with x64, not a
  pipe bug. `crypt`: CryptAcquireContext finds no provider (rsaenh's
  registration is wineboot's DefaultInstall, W3.2's open end).
- The test prefix has the null graphics driver as a fixture
  (`$W/stage-w3.sh` writes `[Software\\Wine\\Drivers] "Graphics"="null"`)
  and stages explorer.exe as a native program (`W3_PROGRAMS=explorer.exe`).
- tests/nt `display`: the desktop takes a new mode's size from a notify
  message (Wine's behaviour with explorer in another thread or process),
  so the check polls for up to 1 s.

**Display driver: winewayland.drv, not a small gex driver** (spike,
2026-10-06). The two options, side by side:

| | winewayland.drv + libwayland + libxkbcommon | a small gex driver |
|---|---|---|
| work | build three libraries for wasm32 (wayland 1.23.1 and libxkbcommon 1.7.0 fetched to `$W/wl`); libwayland's one libffi use (`ffi_call` for listener dispatch) replaced by a ~100-line wasm32 `ffi_call` outside any fork (every listener argument is an i32 on wasm32, so it dispatches on the count); link winewayland's Unix side like win32u's; keymap data on the guest side. About 2–3 d | a new Wine driver: window surfaces, input, keyboard layout, cursors, display modes, clipboard (winewayland is ~8000 lines for these) plus new gex host calls in both hosts. About 5–8 d |
| fork patches | winewayland is upstream: expected a few `__wasm32__` build tweaks at most; libwayland/xkbcommon/ffi are dependencies, not forks | the whole driver is a fork addition, tracking upstream's driver interface (`user_driver_funcs`) at every rebase |
| browser | the page already runs gex's compositor on the same wire protocol (`shims/browser/src/wl.rs`): nothing new | new host calls in the page and in the native host |
| GL (W3.4) | gex-gl's EGL already presents to a `wl_surface` (`guest/gl/gexgl.c`: `wl_egl_window`, commit on swap): winewayland's EGL path needs nothing new | its own GL presentation path, new on both hosts |
| jank | one more process-wide socket protocol, but the one every other guest uses | a gex-only driver no upstream test covers |

winewayland wins on every axis that lasts: fewer fork patches (the driver
is upstream), no new host interface, and GL presentation that already
exists. The small driver is only cheaper in dependency count.

**winewayland.drv running (2026-10-06):** a Win32 window painted with
GDI (FillRect, TextOut through FreeType) shows on gex's compositor
(`gexc run --wayland /tmp/wayland-0 --wayland-out ...`). What it took:
- Dependencies, static wasm32, in `$W/wl/install` (`$W/wl/build-wl.sh`,
  sha256-checked): **wayland 1.23.1** (`864fb2a8399e2d0ec39d56e9d9b753c093775beadc6022ce81f441929a81e5ed`,
  sources compiled directly, scanner built for the host), **libxml2
  2.12.9** (`59912db536ab56a3996489ea0299768c7bcffe57169f0235e7f962a91f483590`,
  for xkbregistry, which winewayland requires), **libxkbcommon 1.7.0**
  (`65782f0a10a4b455af9c6baab7040e2f537520caa2ec2092805cdfd36863b247`,
  meson 1.5.2 from a pip wheel). libffi is replaced by `$W/wl/ffi`
  (`ffi_call` by argument count: every listener argument is an i32 on
  wasm32). XKB data is the host's `/usr/share/X11/xkb`.
- The W3 tree is configured with FreeType and Wayland (`$W/configure-w3.sh`:
  sonames given for the static libraries; tools are `$W/wine-tools-w3`).
  `$W/link-w3.sh` links winewayland's Unix side (`get_region_data`
  renamed apart) and the libraries.
- `tools/wine-wasm/stage-native.sh`: a DLL with no .spec (a driver) gets
  an empty one, so its skeleton is a DLL whose DllMain runs (it was an
  exe skeleton: the driver loaded but never initialised), and a `.drv`
  directory keeps its extension.
- gex-wayland: **wp_viewporter** (Wine's driver will not start without
  it): viewports accepted, the destination recorded per surface.
- **No Wine fork patch** for the driver itself.
- The suite's prefix keeps the null driver (`$W/pfx-w3`, 32 of 40);
  `W3_GRAPHICS=wayland W3_PROGRAMS="explorer.exe winewayland.drv"
  $W/stage-w3.sh $W/pfx-w3wl` stages the Wayland one. On it user_win
  (34), gdi (37) and input (20) pass, and display (20 of 24 at
  `--wayland-size 1280x1024`) fails only where the null driver does.
- **Display modes:** a Wayland client cannot set the output's mode, so
  the Wayland prefix sets Wine's own `EmulateModeset=Y` (HKCU
  `Software\\Wine\\X11 Driver`, the fixture in `$W/stage-w3.sh`): win32u
  offers the standard sizes up to the output's and switches among them
  itself. Modes above the output's are not offered, as on a monitor that
  size (at the default 640x480 output, 800x600 is not on offer), so the
  compositor needs no fake modes.
- **VkKeyScan** returned letters with Shift swapped: winewayland makes
  every letter an SGCAPS pair (Caps Lock row, then the normal row) and
  win32u's `kbd_tables_wchar_to_vkey` matched the Caps Lock row. One
  fork patch (inventory), an upstream bug fix to send.

**W3.4 GL (2026-10-06): opengl32 on gex-gl through winewayland.** The
i386 suite on the Wayland prefix (`--gl --wayland ... --wayland-size
1280x1024`, `$W/w3-nt-wl1.log`) is **34 of 40**: glcompat (22) and glfast
(16) pass; left are lownull x2 (§5.0), filemap and display (shared views),
crypt (no provider registered), pipe (the x64 architecture check).
- gex-gl's guest library (`guest/gl/gexgl.c`) is compiled for wasm32
  (`$W/gl/build-gl.sh` → `$W/gl/gexgl.o`) and linked into the loader;
  every function it defines is in the dlopen registry as `libEGL.so.1`
  (win32u dlsym's two names and takes the rest from eglGetProcAddress,
  which looks them up there), and the libwayland functions it finds by
  dlopen are registered too. The tree is configured with OpenGL
  (`$W/configure-w3.sh`: an `egl.pc` naming gex-gl, the host's Khronos
  headers in `$W/gl/include`); `$W/link-w3.sh` links opengl32's Unix side.
  `opengl32.dll` is staged native (`W3_PROGRAMS`).
- gex-gl changes (merge/w3): `eglQueryString(EGL_EXTENSIONS)` lists what it
  has (Wine requires the client get-all-proc-addresses and platform-base
  extensions, and create-context, no-error and no-config on the display);
  `eglGetConfigAttrib` answers buffer size, transparency (EGL_NONE),
  colour buffer type, conformance and caveat (a zero buffer size left
  win32u no format for its internal context: `wglMakeCurrent(0, 0)` and
  `wglDeleteContext` failed); `wl_proxy_marshal` is called as the variadic
  it is (a fixed-argument call is a type mismatch on wasm32).
- Objects compiled before the ICALL plugin gained declaration thunks
  (`$W/sr-w3`, 09:23 that day) lacked them: opengl32's extension table
  handed i386 code a bare table index (`wglGetProcAddress("glFenceSync")`,
  an execute fault at 0x84F8). Rebuilding every older object fixed it; a
  build tree must not mix objects from before and after a plugin change.
- **No Wine fork patch** for GL.

**Child processes without the session's shared memory (2026-10-07).**
The user's decision: no cross-process shared memory in gex-eject; Wine is
patched instead. Scope, as found:
- win32u reads the session in six places: the **user handle table**
  (`get_user_entry*`, `is_valid_entry`: every HWND check, IsWindow,
  GetWindowThreadProcessId, the handle-table walks), **window** objects
  (`get_shared_window`: class, DPI, wndproc, GetWindowLong's extra bytes),
  **class** objects (class.c: names, GetClassLong), the **desktop**
  (`get_shared_desktop`: async key state, cursor position and clip, the
  monitor serial), the **queue** (`get_shared_queue`: wake and changed
  bits, hook counts) and the thread's **input** (`get_shared_input`:
  foreground, active, focus, capture, caret, key state). user32 reads
  none directly.
- Wine 11.15 still has requests for part of it (get_key_state for one key,
  get_queue_status, set_cursor's position and clip, get_window_info and
  get_class_info for one offset, get_window_rectangles), but the forms
  that returned the rest were cut when shared memory came in (9.0's
  get_key_state returned the whole key array, get_thread_input the
  foreground/focus/capture/caret handles, get_window_info the pid, tid and
  atom). A fallback per site would re-add those replies across six files.
- So one generic request instead: `read_session_data(offset, size)`
  returns the bytes of the session mapping, and a child keeps a local copy
  laid out like the mapping; each read sequence refreshes the bytes it is
  about to read (a seqlock acquire: the object and up to 512 extra bytes; a
  user entry), so the seqlock check that follows passes as it would on the
  real mapping. ~60 lines of hand-written change in win32u, server and
  protocol.def (128 with the generated protocol files). The process hosting
  the server keeps its direct read.
- Two more fork patches came with it: a child process is a copy of the
  first, in-process server state included, so `wineserver_session_data`
  answers only in the process the session was created in (a child got a
  frozen snapshot, and a seqlock caught odd spun for ever); and one
  desktop thread per session (a named mutant): with children able to read
  the session each started its own, and both waited for the other's shell
  window.
- Toolchain fixes found on the way (merge/w3-boot): pelink no longer makes
  trapping stubs for clang's other `main` names (crt_main references
  them weakly; services.exe and wineboot.exe called a trap), gex defines
  them null in the PIC module; and a native module now loads its **delay
  imports** at load time (wineboot delay-imports user32 and gdi32: nothing
  loaded them, win32u's syscall table was missing in its process and
  every NtUser call returned STATUS_INVALID_SYSTEM_SERVICE).
- More, found by the tests (fork, `wasm32-w3`): a child's seqlock wait
  refreshes its copy (a copy caught mid-write would spin for ever); a Unix
  call with no library handle returns STATUS_DLL_NOT_FOUND instead of
  trapping (wineps.drv's DllMain failure path, during DefaultInstall).
- Tests: `tests/nt` **childgui** (user_win as a child: CreateProcess, wait;
  `GEX_NT_CHILDREN=1`, native Wine only) passes all 34 checks in the child,
  5 runs of 5. The launcher calls GetDesktopWindow first, as a GUI
  launcher has a desktop: the desktop thread then lives in the process
  hosting the server.
- **Fixed: a desktop hosted in a child** (a console launcher whose first
  GUI is its child; it hung about one run in three: `user_check_not_lock`
  asserting in the display settings restorer, a thread id of 0, a request
  written to fd 0, a thread dying with the loader lock). gex numbered a
  forked child's threads with its parent's counter while the child's main
  thread takes tid = pid, so the child's Nth thread had the main thread's
  tid: pthread mutexes record their owner by tid, and two threads held
  win32u's USER lock and ntdll's locks at once. A child now numbers its
  threads above its pid, as the browser runtime already did
  (`tests/wasm32/forktid.c`). tests/nt **childcon** (the same child under
  a console launcher) passes 10 runs of 10.
- **Fixed, found by the install as children** (all gex, no fork patch):
  - a child process whose memory landed where an exited one's had been
    skipped the data relocations of Wine's PE module (recorded host-wide by
    the memory's address, and W3 processes have no NT pid): ntdll's lists
    held small offsets and function tables wrong indices. Recorded per
    address space now; tests/nt **childloop** (eight children one after
    another) failed at the ninth process, passes;
  - every mapping of a native skeleton wrote that DLL's variables again: a
    second mapping of ntdll as an image section (wineboot's install does
    one) reset ntdll's Unix-call handle to gex-kernel's, and its next Unix
    call trapped. Written at a DLL's first mapping only; tests/nt
    **imageview**;
  - an exec'd process that trapped kept its descriptors open, so the server
    never saw it go and its parent waited for ever; tests/nt **childtrap**.
- wineboot's full install with its steps as children
  (`$W/w3tests/install-as-children.sh`: the launcher runs `wineboot.exe
  -u`, which runs rundll32 for PreInstall and DefaultInstall) **completes
  the install**: the registry has the crypto providers (16 keys), and
  tests/nt `crypt` passes on that prefix (6 of 6), and **wineboot itself
  finishes in a child**: the test now uses a console launcher (wineboot,
  the child, hosts the desktop) and passes on wineboot's exit code 0 and
  the registry.
- **Fixed: gex's function-lookup loop** (crates/gex-kernel ftab.rs; it
  cost wherever an image view or stale lookup retired an address).
  wineboot stalled in wininet (wininet+0x1d594) with 3.5 million (later
  268 million) lookups of TlsGetValue at one address. The WinePe module is
  installed once and bound per image, so several addresses map to one
  table index; retiring any one of them logged Retire for the index while
  the map still answered it for the others. Each thread's table reset the
  slot to the resolve stub, gex.lookup returned the same index, and the
  stub ran again for ever. FuncMap now counts the addresses per index and
  retires the index only with the last one (test
  `a_shared_index_stays_while_another_address_maps_to_it`). With the fix,
  TlsGetValue is looked up 3 times in the whole install
  (`GEX_FTAB_RECHECK=1` prints each relookup).
- **Fixed: wineboot's garbage wine.inf mtime** (5242148, so every first
  process reinstalled the prefix): pelink bound wineboot's `_wopen` to
  ucrtbase and its `_fstat32` to msvcrt, two descriptor tables; a module's
  calls now go to its own C runtime first. `.update-timestamp` now holds
  wine.inf's real mtime.
- The registry-fixture idea is dropped.
- Cost, on edger through bench.sh (native gexc, `$W/rtt/bench4.txt`; a
  Windows server call, GetExitCodeThread, is 29.53 us there). A child
  refreshes only the bytes a read uses, and keeps its last 16 reads per
  thread: a read is skipped while it is covered, younger than 1 ms and no
  request other than read_session_data has gone to the server since
  (`__wine_session_writes`, ntdll). In a child: IsWindow 1.02 us (0.28 us
  in the process hosting the server; before the cache 97.8 us),
  GetAsyncKeyState 5.13 us (0.79 us; was 572 us), a frame polling all 256
  keys 1334.0 us (201.6 us; was 157 ms). GetForegroundWindow 28.16 us in a
  child and 55.30 us in the host (a server request either way).
  A child also keeps its own USD clocks (a private page and a 1 ms clock
  thread): its copy of the USD had stopped clocks, and every message call
  then sent set_queue_mask. The transport alone, as a pipe-and-epoll round
  trip like a server request's: 20.46 us between threads natively and
  40.30 us in the browser runtime (Node, `run-node.mjs`); between processes
  14.09 us natively, 26.65 us in the browser runtime (`$W/rtt/bench-rtt.sh`).
  Not yet measured in a Chromium page: W3 does not run in the page yet.
- Who would feel it: only a **child** process (a game started by a
  launcher). A game started directly is the process hosting the server
  and reads directly. A full 256-key poll in a child is 1.3 ms a frame,
  6.6 times slower than in the host (201.6 us) but playable.

#### Fork-patch inventory (W3, branch `wasm32-w3` over `wasm32`)

Every change is behind `__wasm32__` (or only built for it). What, why, and
what would let it go:

| patch | why | dropped when |
|---|---|---|
| ntdll/unix `signal_wasm32.c` (new file) and `current_machine`/CPUID | the per-CPU port: gex in place of signal contexts and assembly | never (it is the port) |
| ntdll.so's dir is the loader's; no re-exec; `wineloader` is itself | one closed-world program, no preloader | never |
| `gex.image_mapped` from virtual.c | native DLL skeletons bound when ntdll.so maps them | gex learns image maps another way (unlikely) |
| kernel stacks top-down | gex's dispatch directory sits under 0x400000 | gex moves its directory |
| thread entry at pthread's type; SIGCHLD not taken by the server; no fork in the server | wasm traps on a mistyped indirect call; in-process server | never / with the in-process server |
| wineserver as a thread (`server/wasm32_inproc.h`, `start_server`, `wineserver_exit`, `unshare(CLONE_FS)`), USD at 0x7ffe0000, one 16 MiB session block, registry flush at exit | §3.5: shared memory must be one memory | cross-process coherent shared memory (below) |
| faults as i386 reports them; SIGUSR1/QUIT/INT handlers; unblock at thread start | signals and faults through gex hooks | never (port) |
| Unix side on its own stack; Unix calls keep guest registers | wasm32 syscalls use the guest context's registers | never (port) |
| NtFlushInstructionCache → `gex.code_changed` | gex translates code | never |
| Unix calls from i386 code through the trampoline; wasm callers direct (gexrt) | one pointer serves both | never |
| programs in the one module (winegcc r.o, winebuild exe entry, CRT table markers); crt_main/crt_wmain call clang's main | native programs | upstream wasm32 support (never) |
| native modules' imports loaded (ntdll/loader.c `fixup_imports`) | skeletons have no import table | skeletons that carry real import tables (winebuild) |
| child processes by clone, not fork | asyncify fork cannot cross gex's frames | never under W3 |
| RtlCreateUserThread wraps a native void entry | every entry is called as LPTHREAD_START_ROUTINE | never |
| win32u DllMain always loads win32u.so | one module: the dispatcher variable is ntdll's | separate win32u module (no) |
| user callbacks return through gex's pad | gex watches call-out returns by dispatch slot | never (port) |
| **win32u reads the session from the in-process server** (`wineserver_session_data`) | a wasm memory cannot map the server's pages twice | coherent shared memory (below) |
| **explorer's desktop as a thread** (`gexp_desktop_thread`, win32u starts it once, suspended so it knows the thread, which does not wait for itself; callers wait for the shell window; no shell in that thread; returns, no ExitProcess) | a second Windows process cannot see the session (§3.5) | coherent shared memory across gex processes, so explorer can be its own process again |
| **a child process reads the session through requests** (`read_session_data`, server/mapping.c; win32u keeps a local copy laid out as the mapping, refreshed per seqlock acquire and per user handle entry; the handle-table walk skips entries the process has no object for) | a child has its own wasm memory and cannot map the session the server thread writes (user 2026-10-07: gex-eject never ships cross-process shared memory) | never under that decision; the process hosting the server keeps its direct read |
| `wineserver_session_data` answers only in the process the session was created in | a child process starts as a copy of the first, server state included: it read a frozen snapshot | with the request path (never, under the no-shared-memory decision) |
| one desktop thread per session (a named mutant held by its host process); the wait for its shell window is 60 s | with children reading the session, each process started a desktop thread of its own | explorer as its own process (needs cross-process shared memory) |
| a child keeps its last 16 session reads per thread and skips a read covered, younger than 1 ms, with no other server request since (`__wine_session_writes`, ntdll server.c) | a request per read cost 0.1-0.6 ms a call | with the request path (never, under the no-shared-memory decision) |
| a child process's USD is a private page kept by a 1 ms clock thread (ntdll virtual.c) | a child's copy of the USD had stopped clocks, and win32u then sent set_queue_mask every call | coherent shared memory (never, under that decision) |
| ntdll's Unix-call dispatcher returns STATUS_DLL_NOT_FOUND for a null library handle | a DLL whose Unix library is not linked into the closed-world program still makes calls (wineps.drv's DllMain failure path) and trapped | every such Unix library linked in |
| winebuild (wasm32 export list): a renamed `-import` export is a forward | pelink resolves a call by the export list (user32's ClipCursor is win32u's NtUserClipCursor) | never (tool side of the port) |
| win32u `kbd_tables_wchar_to_vkey` skips an SGCAPS pair's Caps Lock row | VkKeyScan swapped Shift for letters under winewayland (every letter an SGCAPS pair) | the fix upstream (it is not wasm32-specific): **not being upstreamed (user 2026-10-07)**: kept as a fork patch; the ungated patch stays in `docs/wine-upstream/0001-win32u-VkKeyScan-skip-SGCAPS-caps-row.patch` for reference |

**Future option for the last two (and the server's 16 MiB block)**, not
built: a SharedArrayBuffer-backed extra memory (wasm multi-memory) holding
the session objects, which Wine's Unix side reaches only through its
shared-object read/write macros (`memory.copy` between memories). It would
be coherent across processes natively and in the browser (every process
imports the same extra memory), so explorer could run as its own process
and win32u could map the session normally. Decided later, as a gex-wide
question (it touches fork and more than Wine).

**G's repros as acceptance tests** (`scratch/G/wrepro/`, i386 programs
from tests/nt/src on merge/games2; today each stops in the NT layer):

| repro | what it needs | W3 step |
|---|---|---|
| hooks.exe (Katana ZERO, Spelunky 2) | NtUserSetWindowsHookEx and hook calls | W3.3 (win32u.so) |
| iocp.exe (Spelunky 2) | I/O completion ports | W3.1 (ntdll.so + wineserver) |
| unixpath.exe (Dicey Dungeons) | `\??\unix\` paths | W3.1 (ntdll.so's file code) |
| keyname.exe (Spelunky 2) | NtUserGetKeyNameText | W3.3 (win32u.so, the driver's keyboard layout) |
| sysmisc.exe (OneShot) | NtSetThreadExecutionState, SystemParametersInfo 0x5d/0x5e/0x2000/0x2001, VirtualLock | W3.1 (ntdll.so) and W3.3 (win32u.so) |
| com.exe (N++, OneShot, 5D Chess, Retrocycles) | DirectInput8, MMDevice, WIC, dsound classes registered | W3.2 (wineboot's registration) |
| glbig.exe (Undertale, Retrocycles) | GL calls carrying over 8 MiB | W3.4 (opengl32.so, no ring) |
| glcore.exe (5D Chess, Cruelty Squad) | 3.3 core-profile contexts | W3.4 (winewayland's EGL) |

Also seen in G's games (no repro yet): audio drivers loaded by name and
secur32's Unix side failing soft (W3.5); a synchronous NtNotifyChangeKey and
NtAreMappedFilesTheSame (W3.1); NtSetContextThread on another thread
(W3.1, wineserver); NtCreateUserProcess (W3.1, a crash handler's process).
Momodora's missing glGetShaderSource/glGetAttachedShaders and wined3d draw
errors are W3.4.

**Prefixes are copy-on-write** (merge/w3): `stage-native.sh` links only the
large read-only files (DLLs, programs, fonts, NLS) to the staged prefix and
copies every other file, so a run that writes the registry, win.ini or
INetCookies/index.dat writes its own copy. `$W/pfx-*` were fixed in place
(2026-10-06).

**W3.0 findings so far (2026-10-06, branch merge/w3, fork branch
`wasm32-w3`):**
- Every ntdll/unix object compiles for wasm32 (fork 436bf6eda79:
  `current_machine` is i386; CPUID is the x86 lifter's sysreg -100 on the
  thread's context; XGETBV reports x87 and SSE only).
- **A wasm32 program calls i386 code through the function table**
  (`tools/wine-wasm/spike-w3/gc.c`: it maps `add` as x86 and calls it,
  `add(40, 2) = 42`), once the process's translator decodes i386: a wasm32
  program leaves the class alone today (an x86-64 parent keeps its own), so
  the container needs a flag saying its guest code is i386
  (`GEX_W3_COMPAT=1` stands in for it on merge/w3).
- **Faults in that x86 code do not reach the program**
  (`spike-w3/fault.c`): a guest access past the memory traps the whole wasm
  stack, native frames included, and the run ends. Wine's model needs the
  fault delivered to ntdll.so's SIGSEGV handler with the i386 state, the
  guest resumed where the handler leaves the context, and the native frames
  under the guest code kept. That is W3.1's core (a resumable fault for
  guest memory accesses in a wasm32 process, delivered synchronously as a
  signal; the W2 NT path dispatches only after the trap). Also: an access
  to the low guest pages (a NULL dereference) does not fault under gex at
  all (the contexts live there), on any path.
- Option A (the wasm32 loader is the process) stands; B is not needed.

**Total:** about 20–31 agent days, against the 5–8 W3 was given before
W2 showed what stays on the NT layer. Order: W3.0 → W3.1 → W3.2 (closes
Retrocycles' dsound) → W3.3 (Katana ZERO) → W3.4 (Retrocycles' GL) → W3.5
(Undertale) → W3.6 → W3.7. Each step lands on its own (`--wine unix` is
opt-in until W6 retires the NT layer).

### 4.1 W0 results

**(a) wineserver and a Unix library on gex-kernel: works natively.**

- **wineserver** from Wine 11.15, compiled for wasm32 with two source
  changes (i386 machine on wasm32; the CONTEXT in `winnt.h`), runs under
  `gexc run`. The module is 3.0 MB (with debug info) and compiles in
  0.16 s.
- **A test client** (`tools/wine-wasm/spike-a/wsclient.c`, also wasm32)
  `posix_spawn`s it, as ntdll's `start_server` does. It then:
  - connects to the master socket;
  - receives the request fd and protocol version 958 over SCM_RIGHTS;
  - sends its reply and wait pipes back;
  - runs `init_first_thread`, which returns pid/tid, session 1 and
    machine 0x14c (i386);
  - creates, sets and queries an event.
- **ws2_32's `unixlib.c`** compiles unchanged and runs through its
  `__wine_unix_call_funcs` table: `gethostname` returns "gex", and
  `getaddrinfo("127.0.0.1", "80")` gives AF_INET, port 80.
- **What it took in gex:**
  - `wasmprog.rs` (new);
  - a Linux fallback in `libprov::alloc`;
  - a wasm program's fork made inside the syscall rather than by trapping
    out of its frames, which native code cannot survive.
- **Findings for W1–W3:**
  - **Server round trip: 1.07 ms per request natively, now 11 µs.** gex's
    native host waited on fds by polling with 1 ms sleeps (`perform_block`).
    Since 5c92f7d6 a syscall that can change descriptor readiness wakes
    parked threads at once (`shims/native/src/ready.rs`): 20,000
    `event_op` requests take 0.22 s, 11.0–11.3 µs each over three runs
    (cores 3 and 9). The browser shim already woke parked Workers on a
    global counter every syscall bumps (`park_until_ready`). ntsync (§3.5)
    takes waits off the server altogether.
  - **gex-kernel bugs this hit, all fixed with tests in
    tests/x86-programs:**
    - mkdir ignored its mode on hostfs and tmpfs, so wineserver's 0700
      check failed (076e010b, `mkdir-mode`);
    - `shutdown` on unix sockets did nothing, so a peer never saw
      end-of-file after `SHUT_WR`, which wineserver's `sock_check_pollhup`
      tests (6aa24358, `shutdown-poll`);
    - i386's F_*LK64 reached the kernel unmapped, and F_GETLK was
      unhandled (7ab9dd35, `fcntl-locks`);
    - and in musl's wasm32 port, `struct epoll_event` was not packed, which
      wineserver reported as "Error on master socket".
  - **Shared memory:** wineserver shares coherent `MAP_SHARED` memory
    with its clients (session objects; since Wine 9, queue and desktop
    state that win32u reads directly). gex processes have separate
    linear memories, and browser processes are separate
    `WebAssembly.Memory`s, which cannot alias. So wineserver has to live
    in the client's memory: a thread of the Windows process (CLONE_VM
    with its own fd table), not a separate process. §3.5's "later" option
    becomes the plan for W3. Multi-process Windows programs then share
    one address space per session; that fits i386 programs (2 GiB each
    at most) only with memory64.
  - **dlopen** of Unix libraries: the same install with a data region,
    plus symbol binding between modules. Not built yet; ws2_32 was linked
    statically for the spike.

**(b) A native DLL behind a PE skeleton, called from i386 code, calling
back: works natively** (`tools/wine-wasm/spike-b`, `specgen.py`).

- **The DLL** `demo.dll` is plain C (`demo.c`) compiled to a wasm32 PIC
  module.
- **`specgen.py`, a winebuild prototype,** reads `demo.spec` and the
  compiled code and generates:
  - an entry adapter per export (`gexp_<name>`, an `F_body`). It reads the
    i386 arguments from the guest stack, calls the native function, and
    returns in EAX, EDX:EAX or ST0, popping for stdcall;
  - `gexp__entry` in place of DllMain;
  - the exit stubs per wasm signature;
  - the PE skeleton's source and `.def`: each export is `ud2`, built with
    MinGW.

  The spec gives the convention and the argument words. The argument and
  return types come from the compiled module's wasm signatures, because a
  spec has no return types. The generator cross-checks the word counts.
- **Binding:** `GEX_WASM_DLLS=demo.dll=demo.wasm` (gexc) makes it a
  stateful provider. `libprov` binds the adapters at the skeleton's
  export addresses when Wine's loader maps the DLL. The skeleton's bytes
  never run.
- **Callbacks:** `demo.c` calls function pointers through `ICALL()`,
  the select form written out as the pass will emit it (`icall.h`). A
  guest target reaches the exit stub. The stub pushes an i386 frame and
  calls the target through the function table. Its return address is
  `target ^ 1`, as `gp_call0` does, so tier-1's call-out can watch it.
  It restores ESP afterwards, so the same stub serves cdecl and stdcall
  callees.
- **Per-thread state** (the ICALL target, the guest ctx and depth of the
  innermost adapter) lives in wasm globals defined in assembly
  (`gexw_globals.s`), because clang cannot address `address_space(1)`
  globals in PIC code.
- **The i386 test** (MinGW `test.exe`) passes, through LoadLibrary and
  GetProcAddress, with:
  - stdcall `int` and `int64` (EDX:EAX) returns;
  - a cdecl `double` (ST0) return;
  - a cdecl sort calling an x86 comparator 18 times;
  - the same sort given a native export's guest address (exit stub →
    table → adapter → native);
  - a stdcall x86 callback.
- **Timings** natively, wasmtime, cores 3 and 9 (each figure includes its
  loop's work; the browser figures are (c)'s):

  | | `--no-cgen` (tier-1 app code) | `--fjit` |
  |---|---|---|
  | x86 → native, stdcall adapter, per call | 44.0 ns | 18.2 ns |
  | x86 → x86 through a pointer, same loop (baseline) | 31.1 ns | 13.0 ns |
  | native → x86 callback (exit stub), per comparator call | 644 ns | 22.4 ns |

  On tier-1 every callback is a call-out (a nested tier-1 run). With
  function bodies (fjit, AOT) a callback is an ordinary table call.
- **Learned for W1:**
  - The adapters must call through the wasm signature (casts), not the
    C prototype.
  - libprov caches `<module>.cwasm` beside the module: a rebuilt module
    needs it removed.
  - Exit stubs need the guest ctx. They take it from the innermost entry
    adapter, or `gex.provider_ctx()` on a thread no adapter has entered.
  - A callee that leaves without returning (longjmp, SEH unwind) traps
    for now; W2 unwinds it.

**(c) Boundary and ICALL check costs in V8** (node 26.7,
`tools/wine-wasm/spike-c`, gex-cgen's unit flags, 20M calls per case,
median of 5, one core). Each case is a trivial `int f(int, int)`, so
these are per-call overheads:

| call | ns/call |
|---|---|
| native → native, direct `call` | 0.46 |
| native → native, `call_indirect` (V8 inlines the monomorphic target) | 0.74 |
| native → native through the ICALL check, branch form | 2.18 |
| native → native through the ICALL check, select form (below) | 1.37 |
| native → guest (exit thunk: guest frame, fcache probe, `F_body`) | 7.4–8.3 |
| guest → guest (a unit's cross-module call today) | 6.50 |
| guest → native (unit call site → spec adapter → native function) | 6.50 |

- **The check's form matters in V8.** The branch form
  `p < LIMIT ? p(...) : exit(p, ...)` puts a real call on the cold path.
  V8 has no callee-saved registers, so every loop value is spilled around
  it (seen in TurboFan's code). The select form avoids that: the index is
  `p < LIMIT ? p : exit_stub[sig]`, the guest address goes in a wasm
  global, and there is one `call_indirect`. V8 still inlines a monomorphic
  native target. With V8's wasm inlining off, both forms cost the same as
  a plain `call_indirect` (4.4 vs 4.5 ns). The LLVM pass emits the select
  form.
- **LIMIT** is 0x400000. ftab indices stay below it, and every guest
  image and allocation in an NT process is at or above it (nt-layer §1.2).
- **Worst case for the check:** +0.6 ns per indirect call in a tight
  loop. At Peggle's 222M wined3d indirect calls that is about 0.14 s per
  run. That is if every one of them were in a loop this tight: real call
  sites sit in larger functions with their own register pressure.
- **The adapters cost what a cross-image call costs today** (6.5 ns), and
  Wine-internal calls drop from that (calls between lifted Wine images)
  to 0.5–1.4 ns.

### 4.2 W1 results (2026-10-04)

**The toolchain** (§3.10–3.13), every piece built and tested:
- the musl wasm32 port;
- the wasm32 personality in gex-kernel;
- closed-world dlopen;
- the ICALL pass;
- winebuild `CPU_WASM32` adapters and skeletons;
- winegcc and makedep for wasm32;
- winecrt0's wasm32 runtime;
- `wasmdll` in gexc.

**Tests:**
- `tests/wasm32/run.sh`: signals, threads, posix_spawn (native against
  gex);
- `tests/wasm32/dll/run.sh`: every adapter kind through winebuild,
  wasm-ld and the skeleton, from a MinGW i386 program, on `--no-cgen` and
  `--fjit`;
- the coordinator gate on each change to gex-kernel or the shims.

**Wine builds for wasm32:** `configure --host=wasm32-unknown-linux-musl`
plus `make -k`.
- 741 of 852 PE modules link, each as a module plus skeleton, kernel32,
  user32 and setupapi among them. All 741 were rebuilt with the final
  toolchain.
- Of the rest:
  - 51 are Win16 (`.dll16`, `.exe16`, `.drv16`, `.vxd`), which wasm32 has
    no use for.
  - The Unix-backed drivers configure turned off (winex11, winemac,
    wineandroid, winegstreamer, opencl, sane, gphoto2, wpcap, winscard,
    capi2032) and wow64/xtajit64.
  - **W2's core:**
    - **ntdll:** signal_wasm32.c, the system-call path and `current_machine`;
    - **win32u:** system-call stubs become direct calls;
    - **the CRTs** (msvcrt and its versions, ucrtbase, msvcp*, concrt140,
      vcruntime140_1): `DISPATCHER_CONTEXT` and C++ exception frames for
      wasm32; vtables written in assembly (`__ASM_VTABLE`);
    - **rpcrt4:** `ndr_stubless`'s generic call with stack arguments;
    - **the rest:** winedbg, winevdm, ntoskrnl, w32skrnl, wow32.
  - **W3:** winewayland.drv, icu, dmsynth.

**Boundary costs in V8** (node 26.7, core 3), per call:
- **x86 → native through a winebuild adapter** (`spike-c/adapter_bench.mjs`,
  a unit's call sequence into `gexp_abi_add`, cross-module): 7.0 ns. A
  hand-written adapter in one module is 6.0 ns; the first generated version
  was 8.2 ns.
- **The ICALL check:** 0.99 against 0.49 ns for a plain `call_indirect`
  (§3.10).
- **Native → x86 through a pass stub:** 6.7 ns.

**Open, with what they need:**
- **Native runs of code that uses setjmp (`__TRY`)** need wasm exception
  handling, i.e. the wasmtime upgrade (§3.4). V8 runs it today. The SjLj
  runtime and its tag are archive members of their own (winecrt0, musl),
  so only modules that use setjmp carry exception handling: 38 of the 1,188
  modules the build links.
- **The longjmp tag:** each DLL module defines `__c_longjmp`, so a longjmp
  between two DLL modules would not be caught. The single PE-side module
  (§3.13, W2) removes that. A per-DLL build would need the tag imported
  from one place.
- **Cross-DLL imports** are wasm imports of each DLL's module; binding them
  at instantiation is W2's single-module link.
- **Varargs exports and ntdll's system calls** have no adapters yet; they
  bind to `gexp__missing`.
- **Native function pointers handed to guest code** (COM vtables,
  WNDPROCs) need entry thunks, ARM64EC-style (§3.3).

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
  - **Eject avoids it.** An ejected program is a closed world: no JIT, every
    target known at build time. Gluing every used module into one wasm
    resolves the check at link time:
    - for each C signature Wine calls through a pointer, every possible target
      gets a correct table entry: Wine functions directly, program functions
      through a generated guest-entry wrapper;
    - the call site is a plain `call_indirect` after the existing
      address-to-slot lookup.

    The same build also removes per-module instantiation cost and allows LTO
    across Wine and the program, as pewasm's single static module did. The
    runtime check remains only for dynamic gexc runs.
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

### 5.0 Cross-cutting: low-page protection (open, for a later decision)

A guest access to the lowest guest pages (a NULL dereference, `[0]`,
`[eax+8]` with EAX = 0) does not fault under gex on any path: those pages
hold gex's own state (thread contexts, the dispatch directory), and
translated code reads and writes them like any memory. Linux and Windows
both keep the first 64 KiB unmapped, and programs rely on the fault:
Windows SEH `__try` around a possibly-NULL pointer, runtimes that turn an
access violation into a NullReferenceException (Mono: stream U works around
it with `MONO_DEBUG=explicit-null-checks`; .NET, the JVM), crash handlers.
Found by W3.0's spike (`tools/wine-wasm/spike-w3/fault.c`). Options to
decide between later: move gex's low state above the guest's range and
guard the low pages in the translators' address check (a cost on every
access without a static offset), or a guard only where a static base is
zero. Not fixed for W3.

### 5.1 Queued (user, 2026-10-05): the guest's Unix userland as native wasm

After the current W2 items (user32/gdi32 native, `_vsnprintf_s`,
NtWow64*64, `__control87_2`): native wasm32 builds of the guest's libc,
coreutils and whatever other Unix libraries Wine's Unix side depends on, on
the musl fork (`wt/musl-wasm`, `gex-wasm32`), the wasm32 personality
(§3.11) and the closed-world dlopen registry (§3.12).

Steps:
1. **Survey** what actually runs: what Wine's Unix side (ntdll.so,
   win32u.so and the other `*.so` unixlibs) links and dlopens (libc, libm,
   libpthread, libdl/ld.so, and if built freetype, fontconfig, gnutls,
   libpng, libz...), and what a prefix's scripts and wineserver exec (sh and
   the coreutils `wineboot` and the staging scripts call). Record each with
   who loads it and how (static link, `dlopen`, `execve`).
2. **Order** (proposal, to be confirmed by the survey):
   1. libc/libm/libpthread as musl (the sysroot already has them; what is
      new is shipping them as the guest's, under the wasm32 personality);
   2. Wine's Unix side itself (ntdll.so, win32u.so) as wasm32, against that
      libc, with the unixlibs in the dlopen registry;
   3. the dlopen'ed third-party libraries Wine needs at run time
      (freetype first: fonts; then fontconfig, gnutls as games need them);
   4. a shell (busybox's `sh` or dash) and the coreutils the scripts call
      (busybox would cover most in one binary).
3. Each lands with a test under tests/wasm32 (a program using it, natively
   and in the browser).

**Survey (2026-10-05, `gexc run --strace` on gooner; scratch/W/survey):**
- **Windows programs (Wine on gex) use no Unix userland at all.** gex
  replaces Wine's Unix side: NT system calls and the few unix calls
  (`unixcall.rs`: ntdll's, ws2_32, opengl32, crypt32) are gex-kernel's Rust;
  there is no ntdll.so, wineserver, ld.so or libc in a Wine run. A
  `hello_args.exe` trace makes no Linux file system call outside the prefix.
  What Wine would get from Unix libraries is either in gex already (sockets,
  GL through gex-gl) or missing:
  - **TrueType**: gex's GDI draws only the prefix's bitmap `.fon` fonts
    (`nt/gdi/font.rs`); Wine uses **freetype** (and fontconfig to find
    fonts). Candidate 1 for a native wasm library: freetype as a provider
    behind gex's GDI text.
  - **Crypto/TLS**: crypt32/bcrypt/secur32's unix calls answer
    NOT_SUPPORTED (`unixcall.rs`); Wine uses **gnutls** (or a smaller TLS and
    hash library would do for bcrypt).
  - Audio (winepulse/winealsa), joysticks (winebus: SDL/udev), media
    (gstreamer) are not wired in gex's NT layer yet; they are host
    integrations more than libraries.
- **Linux programs** (x86-64/i386 glibc ELF, e.g. Unity's Linux players)
  load, from the host's (or the page bundle's) root:
  - `ld-linux-x86-64.so.2`, `libc.so.6`, `libm.so.6`, `libdl`, `libpthread`,
    `librt`, `libanl`, `libgcc_s` (every Unity/FNA player's NEEDED);
  - by dlopen: `libGL`/`libEGL`/`libGLES*`, `libvulkan`, `libX11`,
    `libXext`, `libXrandr`, `libXcursor`, `libXinerama`, `libXi`, `libXss`,
    `libXxf86vm`, `libX11-xcb`, `libasound`, `libpulse-simple`, `libudev`
    (strings of UnityPlayer.so); FNA bundles SDL2, FNA3D, FMOD.
  - These are x86 code called by x86 code: "native wasm" for them means
    **export providers for ELF images** (as gex-cgen's PE providers for
    msvcrt: `crt_mem`, `crt_math`, `crt_heap`), not replacing the library
    wholesale. gex-cgen's providers are PE-only today
    (`provider::for_image`).
- **Shell and coreutils** (`/bin/sh -c` with ls, sort, head, tr, cat, wc,
  mkdir, cp, rm, date, uname, sed, grep, awk, find, xargs, env, id): dash
  and GNU coreutils, each a dynamic glibc program, opening `libc.so.6` and
  for some `libacl`, `libattr`, `libselinux`, `libpcre2-8`, plus
  `/etc/ld.so.cache`, `/etc/localtime`, `/proc/filesystems`. Every one
  is a full x86 translation per exec. Found on the way: `splice` (x86-64
  275) had no canonical form, so `grep` from a pipe failed with ENOSYS
  (a gex-kernel fix, next).

**Proposed order** (revises step 2):
1. **busybox** (sh + coreutils in one binary) built with `wcc` as a wasm32
   program (§3.11 personality, musl): scripts' `sh`, `sed`, `grep`... run
   natively instead of translated. Map `/bin/sh` and the coreutils names to
   it in the guest root (natively by a gexc option, in the page's bundle).
   Biggest win per effort: every script exec today translates dash/coreutils.
   **State (2026-10-05, merge/wine-fork):** busybox 1.37.0 builds with
   `tools/wine-wasm/build-busybox.sh OUT` (config `busybox.config`: an MMU
   build with real fork (§3.11.1), LFS, hush as sh, ~50 applets incl.
   sed/grep/awk/find/xargs, fancy tail; 540 KB of wasm). `gexc run --busybox B ...` mounts B read-only at
   `/bin/<applet>` and `/usr/bin/<applet>` for every applet
   (`BUSYBOX_APPLETS`; `GEX_BUSYBOX_APPLETS` overrides) and runs B for a
   program named `/bin/sh` etc.; `RunOptions.exe` makes `/proc/self/exe`
   busybox. In the browser, `run-node.mjs --busybox B` bundles B as
   `/bin/busybox` hard-linked at the same names (`busybox.applets`, one
   list for both).
   tests/wasm32/busybox.sh (needs `BUSYBOX`) compares pipelines, command
   substitution, redirections, externals and exit statuses with the host
   sh, and runs busybox under a translated x86-64 shell.
   - **fork** is real (§3.11.1); the earlier vfork emulation (a setjmp
     macro, exec and _exit as posix_spawn) is gone.
   - The program's constructors run (crt1's `_init` calls
     `__wasm_call_ctors`; they never ran on wasm32 before).
   - Fixed on the way: a wasm32 child switched the host-wide translator
     compat (i386) mode, breaking an x86-64 parent after its wait
     (`wasmprog::is_wasm32`). A translated *i386* child of an x86-64 parent
     still has that limitation (documented in `run_program_with`).
   - Browser: native wasm32 programs run in the browser runtime
     (`shims/browser/src/cgen/wasmprog.rs`): their system calls never give
     the stack back except to end the program's frames, so a blocking call
     parks the Worker in place, and a thread or a child is staged and handed
     to the host from the program's `gex.syscall`. A wait on the page itself
     (its keyboard, the Worker's own network) is polled, since the frames
     hold the Worker's event loop.
   - Not yet: the prefix scripts' use of it.
2. **ELF export providers** for glibc's hot entry points (`memcpy`/`memset`/
   `str*`, `malloc` family, libm), the PE providers' model on ELF images;
   needs gex-cgen's provider binding by ELF symbol (gex-cgen re-keys units:
   coordinate).
3. **freetype** as a stateful provider behind gex's GDI (TrueType text in
   Windows programs), built with `wcc` against the musl sysroot.
4. **gnutls** (or a smaller crypto library) behind crypt32/bcrypt's unix
   calls, when a game needs TLS or hashes.
5. X11 client libraries stay as they are: the guest's X11 goes to the
   integrated Xwayland.

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

## 7. State and how to resume (stream W)

**RESUME HERE (2026-10-10, evening): W3.7.** `wine cmd /c echo` runs in
the page under Node on gooner. See §3.17, "W3.7".
- **Driver script:** `$W/page/run-w3.sh TAG EXE|cmd [args]`, with
  `PFX=pfx3` (the installed template) and `TRAPSTACK=1`.
- **Next:**
  1. Chromium timing on edger once the edger reservation ends. Edger was
     reserved for DH on 2026-10-10; no edger jobs until the
     coordinator says so.
  2. Milestone (b): a GUI program on the page compositor.
  3. Milestone (c): Undertale.
  4. The session keeper (design below in §3.17).

**Earlier (2026-10-10).** merge/w3-audio, on top of d63ceee2 (all in
§3.17, "round 2"):

- 3d423c45: copy-loop idiom;
- 028e7a91: x87 zero and `fist` paths;
- c9579c74: content-hash memo;
- bd98b866: pelink i386 binding and native-only eager delay imports;
- b8f708f7: blocking writev;
- 2f26cf99 and the commit after it: w3-session.reg, reg-apply.py, and
  stage-native.sh applying them.

The gate on 2f26cf99 (`$W/gate-z.sh`, gate-z.txt) shows the baseline
failure lists: W3 null 40 of 53, Wayland+GL 38 of 49; install as children,
audio and x86-programs (86) ok.

- **Edger state.**
  - The units for pelink's load order are in `boot-xdg`, built on gooner
    as `$W/edger-units/xdg-x` (build-x.sh and build-y.sh, from pairs.txt
    and pairs-y.txt).
  - The dist is `$W/w3dist-x`: w3dist-u plus pelink-x's winepe.wasm.
  - gamekit `pfx` is now pfx-full plus w3-session.reg. Also there:
    pfx-full (the old default), pfx-nodrv and pfx-lean2 (lean too).
  - `run-game-g.sh` takes `PFX=`; `boot3.sh` takes `BOOT_PROG` and
    `BOOT_CMD`.
- **A new pelink load order changes DLL bases.** Units must then be
  rebuilt for the new pairs:
  1. on edger, run with `GEX_W3_UNITS=all GEX_W3_TRACE=1`;
  2. on gooner, build every "no function-tier unit" pair with
     `GEX_CGEN_PRECOMPILE=1`;
  3. rsync the units to edger's `boot-xdg/gex/cgen`.
- **Next.**
  1. Tier-1's `emit_x87` (gex-emit/src/tier1.rs) lacks x87.h's zero
     paths; 22M of the remaining import calls come from there.
  2. wined3d's command-stream spin is the top back-edge sample.
  3. A reused session (page-side) for console one-shots.

**Earlier (2026-10-09, late; the safepoint fix for Wine's thread loop is §3.17).** merge/w3-audio has master 9e5ac0ff
merged (8230816d). Since the morning (§3.17):

- f43fba88 inet without a network stack; it landed in master.
- 3636e93c clean ranges.
- 74e29561, 0fc5ff1b, b3e0df43: W3 units.
- e6a6b8d5 child start costs and the stall tool.

**Session boot (`wine cmd /c echo`, edger, warm).**

| Build | Boot time |
|---|---|
| Before | 76.9 s |
| gexc-k with units | 9.4-9.9 s |
| Units off (`GEX_W3_UNITS=0`) | 31.0 s; 11.5 s when tier-1's keys happen to repeat |

**Undertale on edger.** Units are on for Wine's images. Rows below the
first two are gexc-s: gexc-o's tree plus the safepoint fix (§3.17), in
the gamekit `ut32.sh` run after a warm-up run. Before the fix, wined3d's
unit gave no frames, so the gexc-n row ran with the game's DLL units
still compiling (on tier-1).

| Run | First GL frame | Steady 30 fps from |
|---|---|---|
| ut16 (gexc-l, units off) | 63.6 s | 289 s |
| gexc-n (Wine's units, game DLLs on tier-1) | 20.5-52.5 s | 232-277 s |
| ut32 s1 (all of Wine's units, wined3d's included) | 24.5 s | 197 s |
| ut32 s2 (`GEX_W3_UNITS=all`: UNDERTALE.exe on its unit too) | 16.8 s | 176 s |

The session boot inside the game's start went from 52.1 s (ut13) to
6.5-7.0 s.

**The load after the boot is compute.** GEX_STALL_EVERY (`ut21.sh`)
shows the main thread at 100% of a core from 60 to 255 s, outside system
calls (its last call was sched_yield). Its profile:

- tier-1 code (`jit:small-module`), 18%;
- trap unwinding for call-outs, 6%;
- x87 soft-float (`gex_f80`/`x87_op`), about 5%.

The exe's unit (s2) moves steady state 21 s earlier than s1 (176 vs
197 s).

**Open issues.**

1. **The remaining 176 s.** It is still mostly the main thread computing
   before the first steady frame. Next, profile s2's main thread with
   GEX_PROF.
2. **Units re-keyed by 09b.** f8bea133 merges master ff69f7e4. The gooner
   units are rebuilt for its keys (`w3units/rebuild-09b.log`; 94 placed
   in a boot, as before). The edger cache `boot-xdg` still holds keys for
   gexc-o/gexc-s. Rebuild it there with `$W/cgen-w3.sh` from a
   GEX_W3_TRACE log before running Undertale on a merged gexc
   (d3dx9_43's unit takes about 8 minutes to compile).
3. **The page.** It still needs `gex.image_mapped` wired to its units
   path, and the clean ranges exported to its fork (§3.17).
4. **From DH (2026-10-09).** In the page, `any_signal()` checked the
   signal state of the last thread to make a syscall, not of the parked
   thread, so a tgkill never woke a thread waiting on a futex. The fix is
   research/dual-heap 041a87fd, due in master after the next gate. Suspect
   it first if Wine threads in the page ever hang idle (SIGUSR1 suspend,
   APCs).

**Where things are.**

- Runs:
  - gamekit `boot-f.sh` and `boot-g.sh`: session boot A/B;
  - `ut14.sh` through `ut32.sh`: Undertale. ut24-26 bisect the units (wined3d), ut27-31 diagnose the stall, ut32 measures the fix. gexc-s is built from `$W/wt-o` (3341103b plus the patch) so it keeps the edger unit keys.
- Units: gooner `scratch/W/w3units/xdg/gex/cgen`, and edger
  `scratch/W/boot-xdg/gex/cgen` (cache for the bench runs, XDG_CACHE_HOME).
- Diagnosis runs on gooner: `w3units/run.sh TAG [K=V]`.

**RESUME HERE (2026-10-09, early).** merge/w3-audio has master 41917e73
merged (bc6c6e2c) and three commits on top: 39bdabd8 `GEX_GL_WAITS`,
4d3bd189 the compositor read-buffer fix, dbf61bc7 `GEX_PROF` per-thread
totals and `GEX_SYS_WAITS`. Suite: `$W/check-w3-gate.sh` (W3 legs through
`gate-w3boot/run.sh` with `W3_GATE_DIST=$W/w3dist-g`, the audio leg on
w3dist-g too); 39bdabd8 clean in `$W/w3chk5.out` (the same failure list
as integ-e2), dbf61bc7 clean in `$W/w3chk6.out` (W3 null 40 of 53,
Wayland+GL 38 of 49, install as children and audio ok; x86/childloop
passed this time, it is load-dependent).
- **Install as children passes again.** Wine's DefaultInstall runs
  `control.exe appwiz.cpl install_mono`, which waits for an answer; it
  used to return at once only because control.exe was missing from the
  corrupted prefix copy. install-as-children.sh passes
  `WINEDLLOVERRIDES=${INSTALL_OVERRIDES:-mscoree,mshtml=}`. One dist from
  one build: `$W/w3dist-g` = `$W/w3dist`'s suite dll dir (no wineboot)
  with w3dist-n's loader.
- **Where the games' time went: the compositor.** The native host pumps
  the compositor at every syscall of every guest thread, and each pump
  allocated a zeroed 64 KiB buffer per client (calloc's memset, glibc
  growing and trimming its heap: `syscall` and `_dl_mcount_wrapper`, i.e.
  libc's memset/memcpy, were over half the process's CPU). 4d3bd189 keeps
  the buffer (native and browser). On edger, against the same loader and
  prefix: Retrocycles gameplay 3.0-3.5 fps -> 38-46 fps (the game's own
  counter 37), Katana ZERO menu 4.4 -> 34 fps, Undertale plays its intro
  at 30 fps (its own cap) once loaded (before: still loading at 420 s).
- **Undertale's "white window" was the frame dump, not the game.** Its
  d3d9 output is a 640x480 gex-gl subsurface drawn on the host; the PPM
  frames came from the decorated toplevel, which is white. Run it with
  `GEX_PPM_GL_ONLY=1` (gamekit `ut9.sh`, frames in edger's
  `gameout/undertale`). Its load is still long: the first GL present at
  107 s, the next 24 by 350 s, then 30 fps.
  Runs: gamekit `prof2-rc.sh` (before), `prof3-rc.sh` and `run8.sh`
  (after); `run-game-g.sh` takes `GEXC=`.
- Tools: `GEX_GL_WAITS=1` (wall time in synchronous GL submits, by the
  batch's last opcode: 1-3% in all three games, so GL round trips are not
  the cost; Undertale's 3303 glGetInternalformativ at wined3d init are
  89 ms in all), `GEX_SYS_WAITS=1` (wall time per thread and syscall),
  `GEX_PROF` writes `<stem>.threads`, `GEX_PROF_MIN_PCT`.
- **The fix in the browser: no measurable change, as expected from where
  the page pumps.** The page pumps the compositor once per scheduler turn
  (a thread on its way to waiting), not at every syscall. `$W/page/`:
  wlbench (a static x86-64 guest holding a Wayland connection, syscall
  rates) in nt.html on edger, shim built before (A) and after (B) 4d3bd189
  from one tree: getppid, sched_yield and poll(0) at 550k-920k/s
  whether connected or not and on either shim (they never reach the
  scheduler); nanosleep(1 us) parks, about 1.15 ms each: 866-874/s on A
  and 877-884/s on B with a connection, 878-895/s without. The page's
  games so far open no in-guest Wayland client anyway (hostx's Xwayland
  is beside the kernel, the NT layer draws directly); Half-Life is the one
  that does (no Steam runtime GTK2 on gooner or edger to bundle it).
- **usd_clock_thread every 16 ms** (wine fork bf5af1fc23e, mw): each child
  process refreshed its USER_SHARED_DATA clocks every 1 ms (about 900
  wakeups a second per process, six in Undertale); now every 16 ms, as
  wineserver does (server/fd.c). Loader `$W/w3dist-u` (w3dist-n plus that;
  the wasm differs in that constant only). Undertale, one run each:
  62 wakeups a second per thread, process CPU 217 -> 210 s over 600 s,
  first frame 107 -> 74 s, steady 30 fps from 350 -> 311 s.
- **Undertale's load is waiting, not working.** Over the first 300 s
  (gamekit `ut12.sh`): process CPU 104 s, the game's main thread 47.5 s,
  no other thread above 2.1 s. GEX_SYS_WAITS (ab813f3e, time outside
  syscalls) has the main thread outside syscalls nearly all the time, so
  it waits where no syscall shows: a wasm32 futex wait is
  memory.atomic.wait, and the in-process wineserver is called without
  one. The run-to-run spread is large (first frame at 74 and 104 s, and
  none in 240 or 300 s with WINEDEBUG or GEX_PROF on).
  WINEDEBUG +sync with timestamps (`ut13.sh`, trace in `$W/ut13/log`):
  the game's main thread waits 52 s at start for `__wineboot_event`, the
  session's boot: wineboot, services and five more processes, each taking
  8-12 s to start (pids 0028 to 00ac at 6, 19, 27, 37, 45, 54 and 66 s).
  After that its NT waits are short (6.4 s over 11 waits, plus
  NtWaitForAlertByThreadId under 1 s), yet it is neither in a syscall nor
  on the CPU most of the time. Next: what it is in (host calls? the Unix
  side's own waits?), and process start time under W3 (each child
  translates its code afresh).

**RESUME HERE (2026-10-08 evening, after the reboot).** Branch
**merge/w3-audio** (wt/w3-boot; master 7a4a62c8 merged): W3.5's OSS null
audio device (/dev/dsp, /dev/mixer), inet socket fixes (port at bind,
SHUT_WR, MSG_PEEK), tests/nt attachwin. Wine fork `wasm32-w3` 3710abbd462
(mw): **explorer's desktop is its own process again** (child processes see
the session through requests now); `WINEGEX_DESKTOP_THREAD=1` keeps the old
desktop thread. Why: a DllMain that makes a window holds the loader lock,
and the desktop thread needed that lock to start (Retrocycles and Katana
ZERO hung, "loader_section wait timed out"). Fix in the same commit: the
desktop process (explorer /desktop) does not start another explorer when its
lazily loaded null driver asks for the desktop window (it did: a chain of
seven explorers).
- Dists: `$W/w3dist-a` (the current loader + every W3 program skeleton,
  wineboot included: games), `$W/w3dist-s` (the current loader over the
  suite's dll dir without wineboot.exe, so a test doesn't boot the prefix:
  `W3_DIST=$W/w3dist-s $W/check-w3-gate.sh wt/w3-boot`, output
  `$W/w3chk4.out`). `$W/w3dist` stays the merge/w3-boot gate's.
- Games: `$W/gamekit` (gexc, dist -> w3dist-a, the installed prefix,
  inputs), `gamekit/games.sh` runs Retrocycles, Undertale and Katana ZERO
  on edger through bench.sh; results in edger's `$W/gameout/<slug>`
  (summary, log, frames/, frames.txt: frame number and time, for fps).
- **Pitfall: never stage a W3 prefix with symbolic links into a shared
  prefix.** stage-native.sh links the DLLs into ~/.cache/gex/wine-11.15;
  under W3, wineboot writes system32 with CREATE_ALWAYS, which follows the
  link and overwrites the shared file, and wineserver deletes on close by
  the realpath() of the name it opened (server/fd.c), so a DeleteFile on a
  linked file deletes the shared target (2026-10-08: 4 DLLs overwritten and
  22 programs deleted in gooner's default prefix; the coordinator restored
  them from edger's). `$W/deref-pfx.sh PREFIX` replaces those links with
  copies; stage-w3.sh calls it. Any symlinked prefix is unsafe under
  native Wine. (gooner's and edger's default prefixes have no
  drive_c/windows/Fonts though the manifest lists 56 fonts: never staged,
  not from this.)
- **Gate artifacts for merge/w3-boot** (frozen, nothing shared with the
  other runs): `$W/gate-w3boot/run.sh TREE GEXC [stage]` stages its own
  prefixes (pfx-w3 null driver, pfx-w3wl Graphics=wayland) and dists
  (w3dist-null, w3dist-wl: copies of `$W/w3dist`, the 03:37 loader),
  generates nt-run-x86.sh from TREE's tests/nt/run.sh, and runs the null,
  Wayland+GL and install-as-children legs (its own install prefix, dist and
  log under gate-w3boot). The Wayland leg needs no compositor running:
  `gexc run --wayland PATH` serves its own at that guest path. On
  integ/2026-10-08e's gexc: null 37 of 51, Wayland+GL 36 of 47 (w3chk3:
  37 of 50, 34 of 46). x86/childloop times out at 120 s under load
  (gooner's load average 21; 3 runs: 1 pass, 2 timeouts, the same with
  merge/w3-audio's gexc): `gate-w3boot/childloop.sh`.
  The install-as-children leg no longer passes on today's artifacts with
  any gexc: integ-e2's, merge/w3-audio's and 36dd674f's own
  (`wt/gate-36dd`) all write the registry (16 crypto provider keys) and
  then stop after DefaultInstall's rundll32, so wineboot never exits
  (`gate-w3boot/inst-{i,b}`). The 03:37 loader's PE side was relinked since
  (`$W/wine-w32-w3`, the W3.5 work), so the 03:37 loader no longer has a
  matching dll dir; with the current loader (w3dist-a) explorer's window
  dies with "indirect call type mismatch" there (`inst-a`). Next: one
  dist whose loader and dll dir come from the same build.
- **Guest GL (2026-10-08 night):** the loaders linked `$W/gl/gexgl.o` from
  10-06 (wt/w3's gexgl.c); `$W/gl/build-gl.sh` now compiles
  `${GL_TREE:-wt/w3-boot}`'s. 7ef0f203 adds glNormalPointer (the host had
  GL_NORMAL_ARRAY enabled with no pointer: Retrocycles' host SIGSEGV at
  gameplay start, core analysed on gooner from edger's,
  `$W/core/`; the GL capture decoder is `$W/core/capdec`). Loader with it:
  `$W/w3dist-n` (gamekit/dist points there).
- Undertale and Katana ZERO (both GameMaker, d3d9 over wined3d) called
  address 0 in wined3d's caps probe: match_allows_spec_alpha calls
  glSecondaryColorPointerEXT, which wined3d assumes on GL 1.4, and the guest
  library had no secondary colour (or fog coordinate) array, so the
  extension was withheld and wglGetProcAddress returned NULL. 47f830a4
  adds both (loader `$W/w3dist-n`, 20:22). Found with `gamekit/dbg-ut6.sh`
  (WINEDEBUG trace+d3d) and `gamekit/glprobe` (glprobe2.exe lists the
  extension functions wglGetProcAddress cannot resolve).
- Retrocycles on w3dist-n (edger, 2026-10-08 20:0x): menus, then gameplay
  for the rest of the 420 s run, no crash; composited 10-12 fps in the
  menus and 1-6 fps in gameplay (the game's own counter: 1). Frames:
  `$W/shots/r7`.
- With those, Undertale and Katana ZERO got past wined3d (two more
  fixes: 0a525987, program option extensions claimed only with their
  base; 552cca51, gex-wayland releases a buffer once per attach, which
  Wine's buffer queue counts) and then hit the release bug at the first
  swap-chain resize. gexc 22:25 has the fix; run `gamekit/run7.sh`.


**RESUME HERE (2026-10-06, after a Claude Code restart).** `$W` =
`~/gex-sandbox/scratch/W`. Run `. ~/gex-sandbox/scratch/env.sh` first.
`gexc run` does not pass the host environment: `--env HOME=...`.

Heads:
- master e3917bac. musl `gex-wasm32` = `gex-wasm32-fork` 155f77bf (mw);
  `gex-wasm32-slim` 93c5dfda (mw); wine-wasm `wasm32` 87f806ced36 (mw).
- merge/wine-slim 10e816c3 is in integ/2026-10-05g (gating). Leave
  `$W/sr-slim`, `$W/bb-slim`, `$W/pfx-gui-slim`, `$W/sr-gate`,
  `$W/bb-gate`, `$W/pfx-gui-fork` alone until the coordinator says done.
- **merge/elf-prov** (wt/elf-prov): the development branch (all commits,
  several gex-cgen ones).
- **merge/elf-prov-gate2** (wt/elf-prov-gate2, 02a3ab7a): the gate-ready shape of
  elf-prov at d7b7f462, on wine-slim 10e816c3 plus the stdin fix: ONE
  gex-cgen commit (ceb54180, incl. glibc_math), then gex-ld (incl. 32-bit
  dynsyms), native, browser (incl. the scheduler fixes), gex-kernel TCP,
  docs. `$W/check-elfprov-gate.sh` runs its pre-gate checks (output
  `$W/gchk2.out`): **all green on 02a3ab7a** (elf-prov 5/5 native and in
  the browser, x86-programs 74, nt --no-cgen 81/81 on x86 and pfx-gui,
  cgen-prefix 81/81, wasm32 4/4 + busybox both, browser nt 49/49). Gate
  order (coordinator): integ-g, U's fjit fix, E's ejected page, then gate2
  with F's final head (both re-key). integ-g landed (master c5620656);
  **gate2 head 004538a9** (that master merged in) is green again
  (`$W/gchk3.out`: the same counts, browser nt 49/49) and was sent to the
  coordinator.
- **W3** (the user's order: before W7): plan in §4.0. gex branch
  merge/w3 (wt/w3, off merge/elf-prov) with the W3.0 spike; Wine fork
  branch `wasm32-w3` (wt/wine-w3; build tree `$W/wine-w32-w3` via
  `$W/build-w32-w3.sh [targets]`, sysroot **`$W/sr-w3`** for every W3 build (a hard-linked copy of sr-slim whose only change is the ICALL plugin with declaration thunks; sr-slim stays read only); loader link
  `$W/link-w3.sh`, prefix and dll dir `$W/stage-w3.sh`). hello_args.exe
  runs on Wine's own Unix side, wineserver in process. **W3.3 in
  progress** (§4.0): win32u.so linked in, the session read from the
  in-process server, explorer's desktop as a thread (option (a)); test
  prefix `W3_PROGRAMS=explorer.exe $W/stage-w3.sh` (null-driver fixture).
  tests/nt i386 **32 of 40** (`$W/w3-nt8.log`; user_win and gdi pass; left:
  lownull x2 (§5.0), filemap and display (shared views), crypt, pipe,
  glcompat/glfast (W3.4)). FreeType 2.13.3 linked in (`$W/ft`); winewayland.drv draws on gex's
  compositor (`$W/pfx-w3wl`). Output modes (Wine's EmulateModeset) and VkKeyScan done; W3.4 GL: opengl32 on gex-gl through winewayland, glcompat and glfast pass (34 of 40 on `$W/pfx-w3wl`). Wine fork head a39bcafaedd (mw; the child-session patches, the read cache and the child's USD clocks).
  **gate2 landed** (master 36f55bb6), and w3-gate1 (wp_viewporter, master
  1b4bd047, merged into merge/w3). **merge/w3 gate head** sent after
  `$W/check-w3-gate.sh` (output `$W/w3chk.out`; the child-session head on
  master ac269692: `$W/w3chk2.out`, the same counts plus childgui and the
  install-as-children leg): crates green, elf-prov 5/5
  native and in the browser, x86-programs 82, nt --no-cgen 83/83 on x86 and
  pfx-gui, cgen-prefix 83/83, wasm32 4/4 + busybox both, browser nt 49/49,
  W3 i386 32 of 40 (null driver; 33 of 41 with childgui) and 34 of 40
  (Wayland + GL); wineboot's install as children leaves the full registry. **merge/w3-boot** (wt/w3-boot, master 8c2c9a7d merged): the function-lookup fix (gex-kernel ftab.rs); child thread ids above the pid (the desktop-in-a-child hang); data relocations per address space; a DLL's variables at its first mapping only; an exec'd process that traps closes its descriptors. Checks (`$W/w3chk3.out`, on 6ec27387): crates green, elf-prov 5/5 native and in the browser, x86-programs 82, nt --no-cgen 85/85 on x86 and pfx-gui, cgen-prefix 85/85, wasm32 + forktid + busybox both, browser nt 49/49, W3 i386 37 of 50 (null driver, with children: childgui, childcon, childloop, childtrap, imageview pass) and 35 of 46 (Wayland + GL; the same known failures), install as children passes on wineboot's exit 0 (16 crypto keys). The old merge/elf-prov-gate is deleted. To
  rebuild after new dev commits: same path split (`git checkout <dev
  head> -- <paths>` per commit), rerun the checks, merge master once
  integ-g lands, send the head.
- Since the last notes: the browser scheduler no longer reports "idle"
  for a wait4/sleep/futex wait (a child in another Worker), and a thread
  whose process exited ends at its next syscall (the pause() one);
  glibc_math (floor/ceil/trunc/round and float forms, bitwise);
  `gex_ld` reads 32-bit dynamic symbols.
- After gate2 (dev only, next landing): f9c5a9ec TCP between a page's
  processes (listeners/loopback streams in the global kernel; abstract
  names freed on close).
- Also dev only: cb1431d0 UDP between guests (natively and within one
  page process) and a blocking recvfrom that waits (it answered EAGAIN).
- Open: UDP between a page's processes (the global kernel has only
  streams); AF_INET sockets in a page without a network stack
  (EAFNOSUPPORT, so loopback TCP needs GEX_NETWORK there); libm sin/cos
  (needs glibc's own bits); a paired page run of the providers on edger
  (E holds the lock).

**Where things are** (all relative to `${NW_ROOT:-~/gex-sandbox}`; on gooner since 2026-10-04, `$W` = `~/gex-sandbox/scratch/W`):

| what | where |
|---|---|
| gex side (this plan, tools/wine-wasm, shim, tests) | `wt/wine`, branch `merge/wine-wasm` |
| wasmtime 49 upgrade (rustc 1.96 pinned) | `wt/wasmtime49`, branch `merge/wasmtime49` |
| Wine fork | `wt/wine-wasm`, branch `wasm32`, remote `mw` |
| musl fork | `wt/musl-wasm`, branch `gex-wasm32`, remote `mw`; patches: §3.11.2 |
| Wine 11.15 reference | `wt/nt-wine-src` (read-only) |

**Rebuilding the outputs** (`$W` = stream W's scratch directory):
1. Sysroot: `GEX_CRT_SRC=<llvm-project 22>/compiler-rt/lib/builtins
   tools/wine-wasm/build-sysroot.sh $W` gives `$W/sysroot` (musl, UAPI
   headers, compiler-rt builtins, the ICALL plugin). Export
   `GEX_WASM_SYSROOT=$W/sysroot` for everything below.
2. Wine's tools, a host build of the fork, in `$W/wine-tools`:
   `<fork>/configure $OPTS && make -j4` (winebuild, winegcc and the other
   tools are what the wasm32 tree uses), where `$OPTS` is
   `--without-x --without-freetype --disable-tests --without-mingw --enable-archs= --without-gstreamer --without-pulse --without-alsa --without-cups --without-sane --without-gphoto --without-v4l2 --without-wayland --without-vulkan --without-opengl --without-dbus --without-krb5 --without-gssapi --without-netapi --without-pcap --without-usb --without-unwind --without-sdl --without-oss --without-udev --without-capi --without-inotify`.
3. The wasm32 tree, in `$W/wine-w32`: `<fork>/configure
   --host=wasm32-unknown-linux-musl --with-wine-tools=$W/wine-tools $OPTS
   --without-gnutls --without-ffmpeg --without-fontconfig
   CC=<wt/wine>/tools/wine-wasm/wcc`, then `make -k -j4`. Each DLL leaves
   `X.dll.so.r.o` and `.exports` beside it.
4. A native prefix: `tools/wine-wasm/stage-native.sh $W/wine-w32
   <fork> $W/pfx-native ntdll.dll kernelbase.dll kernel32.dll`.

**On gooner (2026-10-04 night):**
- Sysroot rebuilt (`$W/sysroot`, emsdk's compiler-rt via env.sh's `GEX_CRT_SRC`;
  9e3d7632 fixes the script for Debian's asm symlink and llvm-config-22).
- `$W/wine-tools`: only `make __tooldeps__` is needed (the full host Wine build is not).
- `$W/build-w32.sh` configures and builds the wasm32 tree (`make -k -j4`, nice 19);
  `$W/browser-nt.sh [WORKTREE]` builds the x86 shims and runs tests/nt/run-browser.sh.
- `$W/wine-tools/nls/locale.nls` must link to the fork's `nls/locale.nls`
  (wrc/wmc read it from the tools tree; build-w32.sh makes the link).
- **wt/wasmtime49** recreated (`git worktree add`), master merged (86734783):
  browser NT **49/49**. Coordinator's full gate queued; don't touch the branch.
- **tests/nt with native ntdll, kernelbase, kernel32: 60/60** (38c4542b, fork
  a25afcc0bd4): x86/display needed entry thunks (§3.3), since builtin window
  classes' procedures are ntdll's `RtlRetrieveNtUserPfn` table.
- **`--cgen` with native skeletons: 60/60** (`GEX_WINE_PREFIX=$W/pfx-native
  tests/nt/cgen-prefix.sh`); skeletons get no unit (`nt_cgen::is_wasm_skeleton`).
- **Browser (node) with native ntdll, kernelbase, kernel32: x86 23/23**
  (`GEX_CGEN_CACHE=target/nt-test-cgen GEX_WINE_PREFIX=$W/pfx-native ARCHES=x86
  tests/nt/run-browser.sh --cgen-prefer`; the cache supplies crt_mem/crt_math,
  which a native run builds). See §3.16. Committed 7e02468b.
- **master merged** (fd8be1d1, wasmtime49 landed; wt/wasmtime49 removed).
  `$W/check.sh` runs W's pre-gate checks: tests/nt both prefixes, tests/wasm32,
  browser NT both prefixes.
- **Native msvcrt and ucrtbase (merge/wine-crt 5e9b0618, fork f454cff22f8):**
  tests/nt **61/61** with ntdll, kernelbase, kernel32, msvcrt and ucrtbase
  native (`$W/pfx-crt`), including x86/msvc_cxxeh through native msvcrt.
  - msvcrt's except_i386.c builds for wasm32 with gexrt doing its assembly
    (wine/gex/runtime.h); vtables in C with a `.set`/`.hidden` alias;
    thiscall calls into guest objects with ECX; vtbl wrappers in the
    trampoline page (tramp::VTBL_WRAPPERS_32, re-keys the trampoline unit).
  - minwindef.h had defined __stdcall empty on wasm32 (so entry thunks never
    popped): now annotate("gex.stdcall"/"gex.thiscall"/"gex.fastcall").
  - varargs adapters: the guest's stack is the va buffer; a printf (by name)
    is relaid by its format (8-byte args 8-aligned).
  - pelink: stubs for imports from i386 DLLs (LoadLibraryA/GetProcAddress
    once, then a guest call); exports bound to the DLL that defines them.
  - Since: msvcp140/concrt140/vcruntime140 native (vtables in assembly with
    `.type`'d function references; thiscall methods always get entry
    thunks); x87/SSE2 register-convention entry points (_CI*, _ftol,
    __libm_sse2_*: gexrt's x87 pop and guest XMM access); `_chkesp` and
    ntdll's `_allshl`/`_alldvrm`... as hand-written entries; win32u's
    system-call stubs; winecrt0's image base from gex (`__gexw_image_base`,
    a data export of every DLL that wasmdll writes). tests/nt 77/77 on
    `$W/pfx-native` (3 DLLs), `$W/pfx-w32u` (9: + msvcrt, ucrtbase,
    vcruntime140, msvcp140, concrt140, win32u) and the x86 prefix.
  - `$W/noadapter.sh DLL` lists a DLL's function exports with no adapter.
  - **user32 and gdi32 native (8157fa81, fork ed2d0f5375b/87f806ced36):**
    tests/nt 77/77 on `$W/pfx-gui` (11 DLLs: the 9 above + gdi32, user32),
    browser x86 23/23. winebuild now makes adapters from the .spec when the
    objects lack the signature, for renamed `-import` exports (user32's
    GetKeyState is win32u's NtUserGetKeyState), and for 64-bit parameters
    spelled as two words (NtWow64*64); pelink thunks caller/definition
    signature mismatches and stubs (trapping, listed) what nothing defines.
    FP control words are the guest's (`_controlfp`, `__control87_2`...);
    va_list printfs are relaid like varargs; `_vsnprintf_s` fixed.
  - W's check prefixes: `$W/pfx-native` (3 DLLs), `$W/pfx-dev` (9),
    `$W/pfx-gui` (11); `$W/pfx-w32u` is the coordinator's gate copy (do not
    restage while a gate runs). `$W/check.sh` runs them all.
  - §5.1 item 1 (busybox as /bin/sh) natively on merge/wine-unix; real
    fork (§3.11.1) and wasm32 programs in the browser on merge/wine-fork
    (musl `wt/musl-fork`, branch `gex-wasm32-fork` until gex-wasm32 takes
    it; sysroot `$W/sr-fork`, busybox `$W/bb-fork`, prefix
    `$W/pfx-gui-fork` built with that toolchain; `$W/check-fork.sh`).
    **Resume (2026-10-05, after the restart):** merge/wine-fork (off
    dbabed6b) has real fork natively and in the browser, wasm32 programs in
    the browser, run-node `--busybox`, and the docs (§3.11.1 design,
    §3.11.2 patch list). `$W/check-fork.sh` ran to the end: nt --no-cgen
    77/77 on x86, pfx-native, pfx-dev, pfx-gui, pfx-gui-fork; cgen-prefix
    77/77 (x86, pfx-native); tests/wasm32 incl. fork and busybox; dll
    abi/shared; browser nt x86 49/49, pfx-gui 23/23; wasm32 in the browser
    (fork, signals, sjlj, threads, busybox). Fixed after the restart:
    pelink translates legacy EH in winepe.wasm (winebuild's glue is
    compiled as Unix-side code; pfx-gui-fork failed every x86 test), and
    build-sysroot.sh reconfigures when the musl tree changes (`$W/sr-fork`
    had been configured against wt/musl-wasm). **Gate artifacts:**
    `$W/sr-gate` (fresh, from wt/musl-fork 155f77bf, binaryen 133) and
    `$W/bb-gate` (busybox built with it); master's gate keeps
    `$W/sysroot` and `$W/busybox`.
    **Gating:** merge/wine-fork 414f43dc (master b7fead07 merged; check-fork
    green: nt 81/81 on all five prefixes and both cgen-prefix legs).
    **musl patch reduction in progress** on merge/wine-slim (wt/wine-slim)
    + musl `gex-wasm32-slim` 93c5dfda (wt/musl-slim, pushed to mw): libgex
    (§3.11.2) holds the setjmp runtime, dlopen registry, signal frames and
    vfork; sysroot `$W/sr-slim`, busybox `$W/bb-slim`; tests/wasm32 (new
    dlopen.c, vfork in fork.c), busybox.sh and run-browser.sh pass on it.
    Wine tree rebuilt with it (`$W/wine-w32-slim`, same make -k failures as
    wine-w32-fork) and staged as `$W/pfx-gui-slim`: nt 81/81 on it, on
    pfx-gui and on x86; browser nt pfx-gui-slim 23/23; dll abi/shared ok.
    Follow-up (browser shim): wait4 for a child in another Worker is not a
    classified wait in the scheduler (it hands the stack back every turn),
    so a long one prints "[gex] 1024 turns with nothing runnable" (fork.c's
    vfork case; run-browser.sh now drops `[gex]` lines). Also pre-existing:
    "pid 1 made a <syscall> after exiting" from a thread still parked in
    pause() when its process exits.
    Next steps:
    1. gate of merge/wine-fork (coordinator).
    2. When it lands: fast-forward musl's gex-wasm32 to gex-wasm32-fork and
       push.
    3. musl patch reduction (the user's standing goal, §3.11.2): only two
       shared-file lines remain (configure, epoll.h); look at whether the
       SjLj runtime, dlopen registry and signal frames can live outside musl
       (a gex wasm32 support library linked by wcc) without jank. musl's
       generic dlopen/__dlsym are weak stubs, so a library linked before
       libc can replace them.
    4. Then §5.1 item 2 (ELF export providers: own branch, tell the
       coordinator before gex-cgen re-keys).
  - wine-w32's Makefile now uses wt/wine-crt's wcc (sed); merge into
    merge/wine-wasm after its gate.
- **CRTs (plan, kept for reference):** native msvcrt needs an i386-semantics `except_wasm32.c`
  (cxx_frame_handler, `_except_handler3/4`, `_setjmp`/`longjmp` on the guest
  context; catch blocks and filters as guest calls with EBP set,
  `continue_after_catch` as a guest-state load), CXX_USE_RVA off on wasm32,
  `__ASM_VTABLE` as C (`name_vtable_full` plus `.set name_vtable, +4`, tested),
  `__thiscall` annotated for thiscall entry thunks, and CALL_VTBL_FUNC on
  guest objects through small x86 `vtbl_wrapper_N` thunks (pop/pop ecx/push/
  jmp [eax+off]) in RWX memory.
- `$W/cycle.sh` rebuilds the three DLLs and restages `$W/pfx-native`;
  `$W/elemmap.py MODULE IDX...` names a table index's function.

**State (2026-10-04):**
- **W0 and W1:** done (§4.1, §4.2).
- **wasmtime 49:**
  - The gate is green on a64, x86, nt (both modes), glibc, lockstep, ppc
    and wasm32.
  - The browser nt rerun is still to do; it needed the wasm32 target in the
    toolchain (fixed).
- **W2:**
  - The one-module PE side works (§3.14, tests/wasm32/dll/shared.sh).
  - The runtime boundary is built (§3.15), and native ntdll, kernelbase
    and kernel32 link into one module with nothing unresolved.
  - **tests/nt (`--no-cgen`, wasmtime 49) with native ntdll, kernelbase
    and kernel32** and the rest of Wine x86: 59 of 60 pass, including the
    W2 gate (hello, sync, tls, seh and seh-traps, apc, cxx), the x86 GUI
    tests and pe-x86/msvc. (x86/display: fixed on gooner, 60/60.)
    - The ICALL floor is 0x3f0000, gex-kernel's NT trampolines: native code
      calls the unix-call trampoline through the check.
    - A guest call from native code returns to a pad at 0x3ffff1 in the
      trampoline page, not `target ^ 1`: tier-1 watched a slot in the
      callee's own page that its code then needed (msvc.exe).
    - `--cgen` with native skeletons is untested.
  - **Next:**
    - the CRTs (msvcrt/ucrtbase: `DISPATCHER_CONTEXT`, `__ASM_VTABLE`);
    - win32u's system-call stubs (the same `wine/gex/syscall.h`);
    - varargs adapters;
    - entry thunks: thiscall/fastcall (msvcp vtables), per-DLL modules.
- **Open:** the `relocate_once` workaround in wasmprog goes once E's
  29a2c2a1 is on main.
