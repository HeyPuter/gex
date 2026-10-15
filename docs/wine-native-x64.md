# 64-bit native Wine on gex (stream W64): plan

Status: plan, 2026-10-14. This replaces docs/wine-native.md §3.9 and phase
W7. Stream W's i386 work ("W3") is the model throughout: read
docs/wine-native.md §3.10-3.17 first.

## 0. Decisions this plan rests on

- **Lowered wasm32 first, then memory64 per game.** Both halves of Wine
  are compiled for clang's wasm64 target, which gives the 64-bit *data
  layout*. The order (the user, 2026-10-08 and 2026-10-14):
  1. **Lowered wasm32, the default.** binaryen's `--memory64-lowering`
     lowers the modules to wasm32 before they ship. They run in the
     process's own 4 GiB wasm32 memory, beside the translated x86-64
     program.
  2. **A memory64 flavour, chosen per game at run time,** for what needs
     more than 4 GiB. Unity goes here (§0.1, phase W64.7).
  3. **The dual heap is undecided.** Its measurements are not in. It is
     only a possible later alternative, not part of this plan.

  No memory64 work comes before W64.1 and the browser runtime path. The
  default path must not close the flavour off.
- **One address step.** The lowering pass wraps each i64 address to i32 at
  the access. In C, `GEX_ADDR` and `gex_addr_t` (include/wine/gex/addr.h,
  and libgex's dl.c) are the only place a pointer becomes the address a gex
  import takes. Wine's address-space limit (virtual.c) is a setting, not a
  constant. Nothing else may assume that addresses are below 4 GiB.
- **Cost of the lowering.** C measured LP64 lowered to wasm32 at 7-13%
  slower than plain wasm32 on pointer-heavy code (docs/gexcc.md §7.3: zlib
  321.4 vs 283.5 ms). The user measured memory64 at 11.3% slower than
  guarded wasm32 in V8.
- **Program code** is x86-64, translated by gex as today: tier-1, fjit and
  units.
- **No new NT-layer work.** W64 replaces the NT layer for x64 programs, as
  W3 did for i386.

### 0.1 The memory64 flavour (phase W64.7)

None of this is built yet. It is recorded so the default path keeps it
possible.

- **Build.** `wcc64 -mgex-memory64` links the same objects and skips
  `--memory64-lowering`. It ships the wasm64 module unlowered and defines
  `GEX_MEMORY64` while compiling. That makes `gex_addr_t` 64-bit, so the
  gex imports take i64 addresses (their 64-bit variants, as `door_call64`
  will be). Every object except the import declarations is the same as
  the lowered flavour's.
- **Runtime.** The process runs with a 64-bit guest memory, using the
  per-process choice in docs/mem64.md §5 (at exec, from a flag or a
  per-binary record). The native and browser hosts already instantiate an unlowered
  wasm64 provider in such a process (the `m64()` paths in `ftab.rs` and
  `gex.mjs`). The translator's x86-64 code uses the mem64 flavour
  (docs/mem64.md §3, §7).
- **Address limit.** Wine's user-space limit (virtual.c) is a value the
  process gets at start: 4 GiB in a lowered process, with code below gex's
  2 GiB code reach (§2, "Pointers"), and the full x86-64 limit in a
  memory64 one.
- **Fixed-base images.** x64 images at 0x1_4000_0000 with no relocations
  map at their base, which the lowered flavour cannot do (§2, "Pointers").
- **What stays the same.** The LLP64 and LP64 layouts, the kernel
  wineserver, signal frames (already 64-bit) and the layout gate.

## 1. Toolchain

### 1.1 The Unix side: LP64, the x86-64 Linux layout

This covers the loader, ntdll.so, win32u.so, winewayland.drv, the unixlibs,
FreeType, libwayland, xkbcommon and gex-gl's guest library.

- **Compiler flags.** `--target=wasm64-unknown-unknown -D__linux__ ...`,
  the same shape as item 3's `A64W_CFLAGS` (tools/gexcc/item3/flags.sh),
  but with x86-64's signed `char`. clang's wasm64 is already LP64.
  `long double` is a 16-byte quad here, where x86-64 uses x87. The size and
  alignment are the same, and no Unix-side structure crosses to the guest
  with one.
- **libc.** musl's gex `wasm64` arch (U's `wt/musl-wasm64`, branch
  `gex-wasm64`) already has what we need:
  - the x86-64 Linux syscall ABI through `gex.syscall(ctx)`;
  - the kernel's x86-64 layouts, with nothing converted;
  - every register slot put back after a syscall;
  - crt1, clone and `gex_thread.s`.

  That arch was built as a provider inside an x86-64 process. W64 needs it
  as a *program*, the way wasm32 is one: program start, threads by clone,
  signal frames at syscall returns, and libgex (setjmp runtime, dlopen
  registry, vfork) built for wasm64. These are additive arch files. They
  are to be agreed with C and U, since the branch is U's.
- **Linking.** `wasm-ld -mwasm64 --experimental-pic -pie --shared-memory
  --import-memory`, then binaryen 133's `--memory64-lowering -O2`. gexcc's
  binaryen is in `scratch/CC/sr/sysroot/binaryen`; the system's binaryen
  108 is too old. Item 3 already ships modules built this way (`lib_link` in
  tools/gexcc/item3/lib.sh), and gex loads them as providers.
- **The container.** An ELF64 x86-64 executable with the same `GEXWASM`
  header (wasm-elf.py `--elf64`) and a new header flag, `GUEST_X86_64`:
  - exec, auxv and brk follow x86-64 rules;
  - the translator runs in x86-64 mode;
  - gex-kernel uses its native (non-compat) personality;
  - the process gets W3's treatment (Windows' fixed addresses left free).

  gex-kernel's wasm-program signal protocol (§3.11.2: the frame is built in
  the context, and the stub runs the handler at the syscall's return) needs
  an x86-64 frame variant beside the i386 one.

### 1.2 The PE side: LLP64, Windows x64's data model

clang has no wasm64 Windows triple: `wasm64-unknown-windows*` is rejected.
**A clang plugin makes wasm64 LLP64**, so no clang fork is needed. A spike
works with the system clang 22 (`scratch/W64/spike-llp64`, about 60 lines).

- **`GexLLP64.so`** is an `-fplugin` that runs before the main action. In
  `ParseArgs`, before any parsing, it sets the target's data model:
  - `long` is 32 bits;
  - `long double` is IEEE double, as in MSVC;
  - `size_t`, `ptrdiff_t`, `intptr_t`, `intmax_t` and `int64_t` are
    `long long`;
  - `wint_t` is `unsigned short`.

  `TransferrableTargetInfo`'s fields are public. The type enums are reached
  through a member pointer from a derived class, which is legal C++.
- **Predefined macros.** These are computed before plugins run, so a
  force-included `llp64.h` redefines the 109 type-model macros (`__LONG_MAX__`,
  `__SIZE_TYPE__`, `__INT64_TYPE__`, `__LDBL_*`, ...) to x86-64 MSVC's
  values. It is generated from `clang --target=x86_64-pc-windows-msvc -dM`.
- **Other flags.** `-mms-bitfields` gives MSVC's bitfield layout, and
  `-fshort-wchar` is set as on wasm32.
- **Checked.** 34 sizes, offsets, alignments and type identities are
  identical to `x86_64-pc-windows-msvc`, including `long`, `long double`,
  mixed-type bitfields, `#pragma pack(4)`, `wchar_t` and `LARGE_INTEGER`. A
  lowered module reads a `long` field as an `i32.load offset=16` through a
  wrapped 64-bit pointer, and its function table comes out 32-bit.
- **Wine's headers.** On `__wasm64__` they take the AMD64 `CONTEXT` and
  `_WIN64` layouts, the way `__wasm32__` takes i386's, but without x86
  inline asm or intrinsics. `include/` gets guards beside the wasm32 ones.
- **`CPU_WASM64` in Wine's tools** (W64.1, Wine fe342afc4bc): widl,
  winebuild and winegcc know `wasm64`, with x86-64's data layout, NDR stack
  layout and NT header machine (AMD64), and no assembly. A spec file's
  `-arch=x86_64` entries reach wasm64 too, as `-arch=i386` ones reach
  wasm32: an x86_64-only assembly export writes `-arch=x86_64,!wasm64`.
- **winebuild `CPU_WASM64`** (W64.2) has a new file,
  `tools/winebuild/wasm64.c`, beside `wasm32.c`. It writes one entry adapter
  per export, reading the **Win64 ABI** from gex's x86-64 context:
  - arguments 1-4 in RCX, RDX, R8 and R9, or XMM0-3 by position; then the
    stack at RSP+0x28;
  - a structure that is not 1, 2, 4 or 8 bytes is passed by reference;
  - a large return goes through a hidden pointer;
  - the result goes in RAX or XMM0.

  There is one calling convention, so x64 has no stdcall, thiscall or
  fastcall cases. For varargs, the adapter spills RCX-R9 to the home slots,
  and the guest stack is then a Win64 `va_list`. Native callees get it
  through the two-kind `va_list` (C's `gexva.h`: wasm's own varargs, or the
  guest's) or relaid by format, as W3 does for printf.
- **Skeletons** are PE32+ with `IMAGE_FILE_MACHINE_AMD64`. pesym and pelink
  learn wasm64 objects: the 64-bit relocation types, and an `-mwasm64` link
  before the lowering.
- **The ICALL pass** gets wasm64 exit stubs: GexIcall.cpp writes Win64
  frames. These have four register arguments, 32 bytes of shadow space,
  8-byte slots and a 16-byte-aligned RSP. They call a 64-bit
  `__gexw_guest_call` that returns in RAX or XMM0. The test stays
  `p < floor ? p : stub`, on i64. Entry thunks (`__gexe.<fn>`) read the
  Win64 registers. C's GexA64Icall (AAPCS64) is the same shape: one pass
  with an ABI table, rather than three copies, if C agrees.
- **As built (W64.2, first part).** `tools/winebuild/wasm64.c` writes the
  adapters from the ICALL pass's `gexw.sig64` custom section (each defined
  function's signature with what the wasm types lose: pointer or integer,
  a byval struct's size, a struct return's size), else from the wasm
  signature. `libs/gexrt/wasm64.c` is gexrt64. Guest calls (exit stubs) go
  through `gex.call_out`, as the Unix side's callbacks do: the return pad's
  page has no dispatch entry under W64 until a call-out translates it.
  `wcc64 -D_WIN32` compiles with the LLP64 plugin, `llp64.h`, Wine's own
  libc headers (not the sysroot's LP64 ones) and the ICALL pass, all in
  the sysroot's `lib` (build-sysroot64.sh); `-shared` and `-r` link
  wasm64 and lower. A non-printf varargs export reads Win64's 8-byte slots
  as they are, so only 8-byte variadic arguments are right until the
  two-kind `va_list`. `tests/wasm64/dll` (the `dll` case of
  tests/wine64/nt-x64.sh, with w64-dist.sh's W64_TOOLS and W64_SRC): 22
  checks pass under the W64 loader.
- **gexrt64** provides:
  - `__gexw_entry`: the C stack goes below the guest's RSP, past its home
    area;
  - guest calls, ending at the return pad;
  - `__gexw_syscall` with 64-bit words;
  - `__gexw_unwind`;
  - the per-thread globals.

## 2. Where x64 PE code and the native Unix side meet

- **The register context** is gex's x86-64 context: RAX-R15, RIP, RFLAGS,
  XMM0-15, x87, and the FS and GS bases (GS base = TEB).
- `dlls/ntdll/unix/signal_wasm64.c` is to x86_64 what `signal_wasm32.c` is
  to i386. It keeps x86_64's frame, context and dispatcher-stack code, with
  gex in place of ucontext and assembly:
  - `CONTEXT` (AMD64) is built from gex's context and written back to it;
  - faults arrive through `gex.fault_hook` as `__wine_gex_fault`;
  - user callbacks run under `gex.call_out`;
  - APCs and `KiUserExceptionDispatcher` are set up as on x86_64.

  `current_machine` is AMD64.
- **System calls, phase W64.1 (PE side translated).** Wine on Linux sets
  `USER_SHARED_DATA.SystemCall = 1`. So the x64 PE stubs call
  `[0x7ffe1000]`, which holds `__wine_syscall_dispatcher`. ntdll.so stores
  there the address of an x86-64 trampoline. The trampoline enters the
  native dispatcher through a gex escape, as W3's i386 Unix-call trampoline
  does (`gex.unix_dispatcher`). Wine's PE code is unchanged, and the
  `syscall` instruction is never intercepted.
- **System calls, phase W64.2 (PE side native).** The stubs call
  `__gexw_syscall` directly, as on wasm32. The Unix dispatcher takes the
  number from RAX and the arguments as `ULONG_PTR` words. W3's
  typed-wrapper rule applies: wasm traps on a mistyped indirect call.
- **Pointers.** These are 64-bit values in both halves and in the guest.
  In the lowered flavour they are all below 4 GiB. Wine's 64-bit
  `virtual.c` gets an address-space limit, a value the process receives at
  start: under 4 GiB when lowered, and x86-64's own limit in the memory64
  flavour (Windows' own `/LARGEADDRESSAWARE:NO` x64 layout is
  similar). Images are placed low, as the NT layer packs them: code below
  `MMAP_TOP`, so the dispatch directory can name it.
  **As built (W64.1b):** a lowered process's data uses the whole 4 GiB;
  its code stays below gex's code reach, `DISPATCH_MAX_PAGES` *
  `DISPATCH_PAGE` = 2 GiB (`w64_code_space_limit` in virtual.c, a setting
  of the flavour), mem64's policy (docs/mem64.md §3). Every executable view
  (images, `PAGE_EXECUTE*` allocations and section views) is placed below
  it; an image whose base is above it is relocated, as any image whose base
  is taken. A page above it made executable later (a JIT's data turned to
  code) is refused with `STATUS_INVALID_PAGE_PROTECTION` and an `err:virtual`
  line naming the range, never moved. **Known gap:** data lands above
  2 GiB when allocated `MEM_TOP_DOWN` or once the low 2 GiB fill up, and a
  JIT that writes first and protects after (`VirtualAlloc(PAGE_READWRITE)`
  then `VirtualProtect(PAGE_EXECUTE_READ)`: LuaJIT's mcode) is refused there.
  A `PAGE_EXECUTE*` reservation is placed low, so its later commit is fine;
  a `PAGE_NOACCESS` one committed as executable is not, if it landed high.
  The code space also has a bottom, 0x400000 (`w64_code_space_start`,
  W64.2): native PE code takes a function pointer below 0x3f0000 for a
  function-table index (the ICALL floor), and gex's trampoline page is
  0x3f0000-0x400000. Executable views go at or above it (an image whose
  base is below is relocated); data allocated below it and later made
  executable is refused the same way, with a "below" log line. Data without an address is found by
  address (`map_free_area`): gex places a mapping without one below its
  code space only. `tests/wine64/highdata.c` allocates 2.5 GiB and checks
  all of it. Growing the directory to 4 GiB instead doubles it to 4 MiB,
  which no longer fits between `RESERVED_END` and `EXEC_BASE` (0x400000,
  where i386 executables load), so it would have to move: an ABI change for
  every generator, unit and translation cache. An x64 image that
  cannot move (a fixed base at 0x1_4000_0000 with no relocations) fails to
  map when lowered, as it does under the NT layer
  (`pe-x64/highbase-fixed`). Such a program runs in the memory64 flavour. The
  server's 64-bit DLL pool (0x6000_0000_0000) moves below 2 GiB.
- **Exceptions: the one new mechanism compared with i386.** x64 SEH unwinds
  by table (`.pdata`, `RtlVirtualUnwind`), not by a chain on the stack.
  - **Guest frames** unwind from their images' `.pdata`, which is in
    memory, so native ntdll can read it.
  - **Native `__TRY` frames** are TEB-list frames on the one stack.
    Wine's x86_64 dispatcher already interleaves TEB-list frames with
    table-unwound frames by stack address (`call_teb_handler`), so they
    carry over.
  - **Native frames in between** have no `.pdata`. When the virtual unwind
    reaches the return pad, the next caller is the guest state saved at
    the innermost `__gexw_entry`: its RSP and return address. ntdll's
    lookup on wasm64 answers that one address that way.
  - A target frame found past native frames is loaded with W3's
    `__gexw_unwind` throw.
  - msvcrt's `except_x86_64.c` (C++ EH and catch funclets) needs gexrt's
    guest calls, as `except_i386.c` did.
- **As built (W64.2, native ntdll with the other DLLs translated).**
  - *System calls.* `dlls/ntdll/syscall_wasm64.c` is generated by
    `tools/wine-wasm/mksyscalls64.py` from `ntsyscalls.h` and the
    declarations in Wine's headers (clang's AST), one stub per call at its
    declared type. Each hands its arguments as 64-bit words to gexrt64's
    `__gexw_syscall64`. That sets the state Wine's x64 stubs leave (RAX the
    number; R10, RDX, R8, R9 the first four; the rest from RSP+0x30; the
    return pad at [RSP]) and enters the Unix side through
    `gex.unix_dispatcher` op 1, at the entry the Unix side registered with
    op 0 (`__wine_gex_native_syscall`, void(int ctx)). Unix calls do the same
    through ops 5 and 4 (`__wine_gex_native_unix_call`). NtContinue,
    NtContinueEx, NtSetContextThread and NtQueryPerformanceCounter (no
    system call: the USD) are in `signal_wasm64.c`.
  - *Guest calls from native code* run under `gex.call_out` to the return
    pad (0x3ffff1). Each live one is a `struct gexw_callout` on the native
    stack, chained from the TEB (`include/wine/gex/gexrt64.h`:
    `SystemReserved1`, which 64-bit Wine leaves unused). The x64 unwinder
    in PE `signal_wasm64.c` reaches the pad and continues at the caller
    the record names: the return address and RSP the native code was
    entered with, or the CONTEXT a `KiUser*Dispatcher` was entered with.
    Each entry from the guest (`__gexw_entry`) puts the chain back as it
    was when it ends, less the call-outs the guest is now above: their
    records were on native frames that are gone, and stack reused under a
    stale head read as a call-out (a cycle; seh's int3 case hung).
  - *Dispatchers re-entered.* A `KiUser*Dispatcher` entry that leaves the
    guest at its own address (NtContinueEx delivering the next APC) runs
    again in place: gex's bridge takes an entry that leaves at its own
    address for one that ran nothing, and hands the address to tier-1,
    which lifts the skeleton's `ud2`.
  - *Threads.* A thread whose start is native (RtlCreateUserThread's
    wrapper, a function-table index) is started by ntdll itself, not by
    kernel32's BaseThreadInitThunk, which is still guest code.
  - *Far longjmp.* A wasm longjmp cannot cross gex.call_out's host frame.
    `__gexw_far_longjmp` (winecrt0's `__TRY` unwind target, and RtlUnwindEx
    to a native target below 0x3f0000) records the jump in the TEB, leaves
    the innermost entry with the guest at the pad of the call-out below the
    target frame, and that call-out's native frame makes the jump.
- **As built (W64.2, the native PE core: ntdll, kernelbase, kernel32,
  msvcrt, ucrtbase in one module).**
  - *pelink for wasm64.* `pelink.py` takes a wasm64 build tree (its
    Makefile's `HOST_ARCH = wasm64`, or `--wasm64`): `wasm-ld -mwasm64`,
    its generated C (forward thunks, guest-import stubs, module imports,
    gap stubs) through `wcc64 -D_WIN32`, compiler-rt from
    GEX_WASM64_SYSROOT, and the module lowered to wasm32 and optimised as
    wcc64 does a DLL's own side module. `pesym.py` needed nothing.
    `stage-native64.sh BUILD SRC OUT DLL...` links the module and writes a
    PE32+ skeleton per DLL naming it (`winebuild --wasm-shared`);
    w64-dist.sh's `pen64` step does that for W64_NATIVE_DLLS and prints
    `W64_NATIVE`, which `nt-x64.sh NATIVE=DIR` puts in place of the
    install's DLLs.
  - *gex.* `wasmdll::install_shared` takes an LLP64 module from its
    skeleton's PE32+ header: the entry pairs are 16 bytes, `__gexw_image_base`
    8, and the module's data exports are i64 globals (offsets below
    4 GiB). The data goes above 2 GiB (`libprov::alloc_above_code`): a
    PE32+ export's RVA is added to its DLL's base in 64 bits, and every
    image is in the code space below 2 GiB, so data below a base was read
    past 4 GiB (ws2_32's `__wine_unix_call_dispatcher` lookup faulted).
    gex-kernel's i386 boot variables are not written into a 64-bit
    module: Wine's own Unix side sets them.
  - *The loader* asks the module for a DLL's imports
    (`__gexw_module_imports`, pelink's) on wasm64 too; gexrt64 has a weak
    one that answers none for a DLL linked as its own module.
  - *msvcrt and ucrtbase* build as x86-64's: `except_x86_64.c` (catch and
    unwind funclets are guest calls through the ICALL exit stub),
    `__C_specific_handler`, handler4, the SSE control word through
    `__gexw_guest_fpu` (gexrt64), sqrt as builtins. setjmp and longjmp
    are the guest's, so the CRTs forward `_setjmp`, `_setjmpex`, `setjmp`
    and `longjmp` to ntdll's on wasm64 (an adapter calling `longjmp` by
    name is taken by clang's wasm SjLj lowering). Vtables are `.int64`
    table indices, each method given an entry thunk by `__thiscall`'s
    annotation; the exception and RTTI data's RVAs are from 0 (the
    module's data is outside every DLL's image, and all addresses are
    below 4 GiB), which RtlPcToFileHeader's 0 for them matches.
  - *Leaving a call-out.* A catch block continuing in an outer frame
    leaves the guest above the native frames of the call-out the throw was
    dispatched in (the frame handler is reached through one). An entry that
    ends that way, or one the guest makes from there, sends gex
    NATIVE_LEFT as wasm32 does: the call-out is abandoned and its native
    caller drops its frames, so nothing piles up per throw (x64/msvc_cxxeh
    throws 20000 times; it died near 8470 without this).
  - *Fibers* (kernelbase): a switch saves the guest's state as
    SwitchToFiber returns to it and loads the other fiber's with
    NtContinue; a new fiber starts at `start_fiber`'s entry thunk.
  - tests/wine64/nt-x64.sh with `NATIVE=` (the five DLLs native, the rest
    translated) adds `msvc_cxxeh` (tests/nt/msvc/build-x64.sh: clang-cl
    x64 C++ EH, SEH, setjmp with a frame) and `fiber` (tests/nt/src/fiber.c)
    to the set.

## 3. wow64: no

- **Same as W3.** An i386 program keeps W3, the wasm32 loader. An x64
  program runs on the W64 loader. Neither loads wow64.dll or a BTCpu DLL.
  ntdll calls gex's hooks directly, as on W3.
- **Mixed sessions come later (W64.8).** These are an x64 launcher with an
  i386 child, or an i386 installer with an x64 child. Wine 11 still has
  "old wow64": a 32-bit ntdll.so in a 64-bit prefix, `is_old_wow64()`. So
  the W3 loader could be the 32-bit client of an AMD64 session. Until that
  is checked, a prefix and session have one bitness.

## 4. The kernel wineserver and W3's fast paths

**The server.** It stays the one wasm32 (ILP32) build. Its protocol
already carries `client_ptr_t`, `mod_handle_t` and `lparam_t` as 64 bits,
and the session, USD and wake structures are fixed-width. What assumes a
32-bit client:

| where | what | change |
|---|---|---|
| `server/registry.c:1806-1826` | `supported_machines` = {I386} under `__wasm32__` | {AMD64, I386} for a win64 prefix, chosen at run time (from the prefix, or `WINEARCH`) |
| `server/registry.c:1888` | a new prefix's bitness is `sizeof(void*)` | the same run-time choice |
| everything keyed on `native_machine` (`mapping.c:906, 1730`, `process.c:613`, `thread.c` contexts) | | follows from the two rows above; `context_t` is already a tagged union |
| `server/mapping.c:227-236` | the 64-bit DLL pool sits above 4 GiB | a pool below 2 GiB |
| door: `__server_request_info` parsed with ILP32 offsets (`shims/native/src/door.rs:467-527`, `shims/browser/js/gex.mjs:4820-4880`) | under LP64, `reply_data` is at 72 and `data[]` at 80, in 16-byte entries | a layout bit in `door_call`, on both hosts |
| client `unix_private.h:54` | `current_machine` = I386 | AMD64 on wasm64 |
| `FLAG_GUEST_I386` / `isW3Program` | marks a Wine process | also `GUEST_X86_64` |

**The fast paths.**

| fast path | 64-bit plan |
|---|---|
| door calls into the kernel server | **64-bit variant.** The client-side layout comes from the table above. The area, the server's own syscalls and its stack stay as they are (the server is wasm32). First runs use `WINEGEX_DOOR=0` (the pipes). The door comes in W64.3. |
| NtQueryPerformanceCounter through `gex.clock_ns` | **Carries over** in the native PE ntdll (W64.2). Its every-64th-call fallback goes through the 64-bit syscall stub. |
| USD through the door | **Carries over** with the door: the page layout is fixed. The import declarations take 32-bit addresses (below). |
| fd-cache section (`server_enter_section_nosig`) | **Unchanged** C. It applies once the door is on. |
| win32u's session copy | **Carries over**: the shm structures are fixed-width and the offsets are bytes. The 16 ms copy without the door comes first; `session_copy` comes with the door. |
| opengl32's id mapping | **Unchanged**, but skipped at first. It is only a performance path; it comes with W64.4. |
| typed syscall wrappers | The wrappers are **unchanged**. The dispatcher and `wine/gex/syscall.h`'s stubs **need a 64-bit variant**: RAX, `ULONG_PTR` words, dispatch by argument count. |
| spawn by clone and vfork | **Unchanged** C, provided musl wasm64's `clone` follows gex's vfork contract. The W64.0 tests check it. |

**gex imports that take pointers** are `door_call`, `session_copy`,
`usd_copy`, `usd_publish` and `wake_write`. They are declared with i32
addresses, which is the wrapped form. The wasm64 build declares them that
way: one header, `wine/gex/imports64.h`, whose wrapper casts each pointer
to a 32-bit address. That makes it the same single address step as the
lowering pass.

The `#ifdef __wasm32__` guards on gex glue that does not depend on the
data layout (door, USD, session, spawn) become `defined(__wasm32__) ||
defined(__wasm64__)`. That is a no-op for i386. Code that does depend on the
layout gets its own `__wasm64__` file or branch. I will agree each shared
file with W first (§6).

## 5. Phases and gates (agent time)

Each phase lands on its own. `--wine` W64 stays opt-in, and W3 and the NT
layer are untouched.

| phase | work | gate (runs on gooner unless noted) | estimate |
|---|---|---|---|
| W64.0 toolchain | the GexLLP64 plugin and `llp64.h` in `tools/wine-wasm/llp64/`; musl wasm64 as a program, lowered; libgex64; ELF64 container plus `GUEST_X86_64`; gex-kernel's x86-64 wasm-program signal frames; configure `--host=wasm64-unknown-linux-musl` (Unix side) | `tests/wine64/layout.sh`: about 80 Win32 and Wine structures (CONTEXT, TEB, PEB, EXCEPTION_RECORD, MSG, OVERLAPPED, ...) compared field by field with `x86_64-pc-windows-msvc`, clang only. `tests/wasm64/run.sh`: tests/wasm32's signals, threads, posix_spawn, dlopen and sjlj, built LP64 and lowered, run natively and in the browser runtime | 2-3 d |
| W64.1 first proof: Unix side native, PE side translated | `signal_wasm64.c`, `current_machine`, virtual.c limits, `link-w64.sh` (loader, ntdll.so), the server's AMD64 personality, `WINEGEX_DOOR=0`; Wine's x86_64 PE DLLs built from the fork with MinGW and translated (Wine's own `[0x7ffe1000]` path) | tests/nt **x64** hello_args, then sync, mtx, tls, seh, cxx, apc, crtmem and exitproc on the W64 loader (`tests/wine64/nt-x64.sh` over `tools/wine-wasm/w64-dist.sh`'s install): **10 of 10 pass** (2026-10-14). mtx takes about 7 minutes: every contended wait is a system call through the `ud2` entry | 3-5 d |
| W64.2 PE side native | winebuild `CPU_WASM64` adapters and skeletons, ICALL x64 stubs and entry thunks, gexrt64, x64 table unwinding across native frames, pelink and pesym for wasm64; native ntdll, kernelbase, kernel32, ucrtbase and msvcrt, with the other DLLs still translated; QPC fast path; typed dispatcher | `tests/wasm64/dll`: every adapter kind (register and stack integers, XMM arguments, a structure by reference, a structure return, varargs, callbacks, a data export, a forwarder) from a MinGW x64 program; the W64.1 tests/nt set with the native core; `x64/msvc_cxxeh`. **Native ntdll, the other DLLs translated: tests/nt x64 and `dll`, 12 of 12** (2026-10-15; `nt-x64.sh` with `NTDLL=`, w64-dist.sh's `W64_NTDLL`). **The native PE core (ntdll, kernelbase, kernel32, msvcrt, ucrtbase in one pelink module), the other DLLs translated: 14 of 14 with `msvc_cxxeh` and `fiber`** (2026-10-15; `nt-x64.sh` with `NATIVE=`, w64-dist.sh's `W64_NATIVE`) | 4-6 d |
| W64.3 prefix and door | wineboot's install as children for a win64 prefix; the door with the LP64 request layout (both hosts), USD, the fd-cache section, the session copy | the install-as-children leg on win64; every console tests/nt x64 case, door on and off | 2-3 d |
| W64.4 GUI and GL | win32u.so; winewayland with libwayland, xkbcommon and FreeType built LP64; opengl32 and gex-gl's guest library LP64; the opengl32 id fast path | x64 window, user_win, gdi, input, display, glcompat and glfast on gex's compositor. **Started: `x64/window` passes (2026-10-15; `nt-x64.sh` with `NATIVE=`, its frames checked by `frame.py`), and Wine's x64 notepad comes up with its menus and text drawn.** As built: w64-deps.sh (FreeType, libwayland with ffi64's typed call table, libxml2, xkbcommon at w3.pins' versions), link-w64.sh's win32u and winewayland Unix sides, win32u's typed system calls (syscall_wasm64.cpp), the session mirror through requests (no door yet), winewayland.drv's PE side native (in `W64_NATIVE_DLLS`; the prefix is made without it, so `nt-x64.sh` installs its file), Wine's fonts staged into share/wine/fonts. GL not yet | 3-5 d |
| W64.5 page | the ELF64 wasm program in the browser runtime, the door's LP64 parsing in `gex.mjs`, the page prefix template for win64 | browser tests/nt x64 (both builds, SP leg) | 2-4 d |
| W64.6 games | G's x64 games that fit 4 GiB, through bench.sh on worldmachine | a game to gameplay; fps against the NT layer's x64 path | open |
| W64.7 memory64 flavour | per game, for what needs more than 4 GiB (Unity, fixed-base x64 images): the unlowered wasm64 modules (`wcc64 -mgex-memory64`, `GEX_MEMORY64`, the gex imports' i64 variants); the translator's mem64 flavour for the x86-64 code (docs/mem64.md §3, §7); the runtime choice at exec (docs/mem64.md §5); the address limit as x86-64's; fixed-base images above 4 GiB at their base | the W64.1 tests/nt set and `pe-x64/highbase-fixed` in the memory64 flavour, natively and in the browser; a Unity game past its menus | open |
| W64.8 mixed sessions | the W3 loader as an old-wow64 client of an AMD64 session | an x64 launcher starting an i386 child | 2-3 d |

**Risks.**

- **Unity needs more than 4 GiB of address space.** docs/mem64.md
  measured 16-19 slots of 256 MiB at the menus, about 4-4.75 GiB of
  reservations. So Unity on W64 runs in the memory64 flavour (W64.7).
  Programs and smaller x64 games, lowered, come first.
- **The plugin uses clang internals.** TargetInfo fields can move between
  LLVM releases. The layout gate catches that at once. The fallback is a
  roughly 30-line wasm64-windows TargetInfo in a clang we build ourselves.
- **Varargs exports and x64 C++ EH** (catch funclets) are the long tail of
  W64.2, as they were for W2.
- **Unix-side speed.** The 7-13% lowering cost applies to every Unix-side
  call. The fast paths keep the count of those calls down.

## 6. Coordination

- **W.** Batched questions on the shared files go to W before W64.1:
  - Wine: configure.ac, `tools/winebuild` (main.c and spec32.c switches),
    winegcc, `include/winnt.h` guards, `ntdll/unix` (`unix_private.h`,
    `virtual.c`, `loader.c`, `server.c`), `server/registry.c` and
    `mapping.c`, `win32u/syscall.c`;
  - gex: `door.rs`, `wasmprog.rs`, `lib.rs`'s flag checks, gex-kernel's
    wasm-program signal frames, and `gex.mjs`'s door.
- **C and U.** musl wasm64 as a lowered program, libgex64, binaryen and
  lowering pitfalls, and a shared ICALL pass with an ABI table.
- **Branches.** Wine on `w64/main` (wt/wine-w64, from mw/wasm32-w3
  b57ea7f7d69). gex on `w64/main` (wt/w64). Work files are in
  `scratch/W64` and `hot/W64`.
