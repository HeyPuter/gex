# Unity's managed code on upstream .NET's wasm runtime

Stream U, 2026-10-04. Design and feasibility; nothing is built yet.

Direction (user, 2026-10-04): Unity games run their `Managed/*.dll` on
upstream .NET's wasm runtime, not by translating Mono's x86 JIT output.
UnityPlayer's `mono_*` calls are bound to that runtime through export
providers (merge-plan §9.9, eject-pe §5.1). The fjit writable-and-executable
work (addac37e) is reverted: with no x86 Mono, there is no JIT output to
translate.

## 1. Decisions

1. **Runtime: Mono from r58Playz/dotnet-runtime, branch `wasm-jspi-jit-10.0.3`**
   (user, 2026-10-04). That is dotnet/runtime v10.0.3 plus a runtime wasm
   JIT for Mono (`mini-wasm.c`, `wasm-encoder.c`: mono IR to one wasm module
   per method, tiered), and changes to CoreLib's Reflection.Emit (so CoreLib
   is built from the same tree). It is pinned to commit 6812ea26 in
   `tools/unity-dotnet/mono/build.sh`. The interpreter runs first; the
   fork's JIT is off (`MONO_WASM_JIT_AUTO=0`) until its emitter produces
   wasm64. Mono's LLVM AOT of every managed assembly is the other speed
   tier. CoreLib is not CoreCLR's: the user ruled CoreCLR out ("no coreclr").
2. **BCL: upstream's, not Unity's.** Upstream Mono can only use
   `System.Private.CoreLib` as its corlib (`MONO_ASSEMBLY_CORLIB_NAME`). Its
   icall table (`icall-def.h`) is CoreLib's, for example `RuntimeType`
   icalls that take `QCallTypeHandle`. The legacy mscorlib table is gone, so
   Unity's `mscorlib.dll` cannot run on it. Instead, Unity's and the game's
   assemblies bind to .NET's compatibility facades (`mscorlib.dll`,
   `System.dll`, `netstandard.dll` and others, which type-forward into
   CoreLib), as any .NET Framework assembly does on .NET. This is **feasible**, measured
   with `tools/unity-dotnet/apiscan` (Cecil resolves every TypeRef and
   MemberRef against .NET 10.0.10, with the game's BCL files replaced
   wherever .NET has an assembly of that name):

   | game | refs | unresolved in game code (Assembly-CSharp*, UnityEngine.*, plugins) | unresolved elsewhere |
   |---|---|---|---|
   | Muck | 18,084 | **0** | Mono.Security (22, Mono TLS internals), System.ServiceModel.Internals (10, CAS attributes) |
   | Rain World | 52,062 | 2: `NetDataContractSerializer` (firstpass) | WinForms, System.Design, Web.Services, Soap, Mono.Posix, MonoMod/Cecil.Rocks (BepInEx; disabled) |
   | Hollow Knight | 42,854 | **0** | WinForms, Soap, Mono.Posix, Mono.Security |

   The leftovers are in assemblies that the game references but are never
   loaded at run time, or that serve modding. Each `UnityEngine.*Module`
   resolves completely. The other risk is APIs that resolve but throw on
   .NET (`BinaryFormatter` since .NET 9, `Thread.Abort`, AppDomain creation,
   `BeginInvoke`). The prototype finds those at run time. Each can be
   handled by a small CoreLib-compatible replacement assembly, never by
   changing the game.
3. **Pointer width: wasm64.** The runtime is built for **memory64**
   (`wasm64`, 8-byte pointers). This has two reasons:
   - All three games already need a 64-bit guest memory (mem64.md §1). The
     runtime shares that memory, so object pointers are guest pointers.
   - x86-64 UnityPlayer reads managed objects at 64-bit offsets. In upstream
     Mono on a 64-bit target, the object, string and array headers have the
     same layout as in Unity's 64-bit Mono: the header is 16 bytes, a
     string's length is at 16 and its characters at 20, an array's bounds
     are at 16, its length at 24 and its data at 32. Blittable structs with
     `IntPtr` fields also match. wasm32 would need every icall's structures
     marshalled, which is not workable.

   Upstream has no wasm64 target, so the port is:
   - CMake: `TARGET_WASM` with `TARGET_SIZEOF_VOID_P 8`;
   - `mini-wasm.c`/`.h` at 64 bits;
   - the interpreter's native-call path (§3), replaced, not ported.

   The managed side is the 64-bit Mono CoreLib and BCL, **built from the
   same tag** for `linux-x64` (Muck) and `windows-x64` (Rain World and
   Hollow Knight). CoreLib's icall table must match the runtime exactly.
4. **Toolchain: clang and a musl of our own for wasm64, not emscripten**
   (revised in P2, following stream W's decision for Wine, docs/wine-native.md
   §3.10).
   - musl has a gex `wasm64` arch (fork `wt/musl-wasm64`, branch `gex-wasm64`,
     from W's `gex-wasm32`):
     - syscalls are the **x86-64 Linux ABI** through `gex.syscall(ctx)`, and
       every register slot is put back afterwards, since the context is the
       calling guest thread's;
     - the kernel layouts are x86-64's own, so nothing is converted;
     - `brk` stays the guest's.
   - Mono is configured as **Linux on a wasm64 CPU** (`HOST_GEX`):
     - wasm for the architecture (no MonoContext, the interpreter);
     - POSIX for the OS: pthreads, mmap, files and dlopen go through musl to
       gex-kernel;
     - suspend is cooperative only, as on every wasm host.

   Emscripten would bring its JS runtime's syscalls, Workers and memory model
   into a process gex owns. Its build stays only as P1's node smoke test.
5. **Binding: the whole Mono library is one stateful provider.** When an
   image named `mono-2.0-bdwgc.dll` or `libmonobdwgc-2.0.so` maps (UnityPlayer
   `dlopen`s or `LoadLibrary`s it), every export of that image is bound in
   ftab to the provider:
   - the binding is by name and export set, not by content hash: three
     Unity versions, one provider;
   - an export with no implementation goes to `gexp__missing`, loudly;
   - no x86 Mono code ever runs, so there is no x86 JIT output and no
     writable-and-executable code.

   `GEX_UNITY_DOTNET=0` keeps the x86 Mono, for A/B and debugging.

## 2. The mono_* API UnityPlayer uses

The 313 names UnityPlayer looks up (246 are common to all three games) are
listed in `tools/unity-dotnet/mono-imports.txt`:

| class | count | what binding them takes |
|---|---|---|
| upstream public API | 205 | an adapter per ABI (SysV x86-64, Win64) calling the upstream function |
| upstream internal | 13 | the same, calling the internal function (`mono_thread_suspend_all_other_threads`, the old `mono_profiler_install*` → the new profiler API) |
| absent upstream | 30 | small shims: class userdata (a side table keyed by `MonoClass*`), the incremental GC knobs (no-ops), the appdomain-ref push/pop (no-ops), `mono_set_assemblies_path_null_separated`, `mono_set_find_plugin_callback` (the DllImport resolver, §3) |
| `mono_unity_*` | 65 | written against upstream internals. The liveness, heap-walk and snapshot functions (Unity's memory profiler) start as stubs that report empty. The object, array and string header queries are constants. `mono_unity_class_get_userdata` and related functions use the side table. `install_unitytls_interface` stores the table (used only by managed TLS) |

These semantic gaps are mapped as follows:
- **AppDomains.** `mono_domain_create_appdomain` returns the root domain,
  and unloading it is a no-op. A player never reloads its domain.
- **Assembly loading by path or from memory**
  (`mono_image_open_from_data_with_name`, `mono_assembly_load_from_full`)
  goes into the default ALC.
- **Name lookups on the corlib.** `mono_class_from_name` on corlib
  follows the facade's exported-type forwarders, which upstream already
  does. A lookup of a Mono-only type (if UnityPlayer makes one) is mapped in
  the shim; the prototype logs every failed lookup.
- **GC handles** (`_v2`: the pointer-sized handle forms) map directly.
- **Strings and arrays** map directly. `mono_array_addr_with_size` is
  `base + 32 + size * idx`.
- **`mono_runtime_invoke`** goes through the interpreter's `runtime_invoke`.
  A managed exception comes back through `exc`.
- **`mono_raise_exception`**, called from an icall running in guest code, is
  a non-local exit back to the managed caller of that icall (§3).
- **Callbacks into the guest** go through `gp_call` with the guest ABI:
  `mono_trace_set_log_handler`, `mono_unity_set_vprintf_func`, the crash
  and unhandled-exception hooks, the profiler callbacks and the
  find-plugin callback.

## 3. Calls between managed code and UnityPlayer

**Managed to guest** (icalls, P/Invokes, unmanaged function pointers): one
marshaller, the **guest native call**.
- Its input is the method's native signature, after Mono's marshalling
  wrapper has run, and a guest address.
- It places the arguments by the guest ABI: SysV x86-64 or Win64 (registers,
  shadow space, stack, structs by value or by hidden pointer, and an
  `xmm0`/`rax:rdx` or by-memory return).
- It does what `gp_call` does: it pushes a return address and calls
  `ftab[probe(addr)]`.
- The classification logic is `mini-amd64.c`'s `get_call_info`, which
  already handles both conventions, adapted to write `ctx` rather than
  machine registers.

It replaces the wasm build's interp-to-native signature tables, which
call wasm functions and do not apply here.

Where native function pointers come from:
- **Icalls** are registered by UnityPlayer through `mono_add_internal_call`
  (InternalCall methods: 7,686 in Muck, CoreModule 2,737, ParticleSystem 781, Animation 644;
  about 8,200 in Rain World and 9,750 in Hollow Knight, not counting Mono.WebBrowser). They go into upstream's icall hash and are tagged as guest
  addresses.
- **P/Invoke targets** (`steam_api64`, `libsteam_api`, Rewired's user32 and
  XInput, InControlNative, `kernel32` from the Windows BCL) are resolved
  through Unity's find-plugin callback (`mono_set_find_plugin_callback`),
  then through the guest's `dlopen`/`dlsym` or
  `LoadLibrary`/`GetProcAddress`, called with `gp_call`.

Unity's generated bindings pass structs by reference (`_Injected`), so
icalls rarely take a struct by value. P/Invokes do, and the classifier
handles them.

**Exceptions out of an icall.** Each guest native call is a wasm EH
`try_table`. `mono_raise_exception` saves the exception and does a `throw`,
and the catch restores the guest RSP that was saved at the call. Native C++
frames are skipped without running their destructors, as Unity's Mono also
skips them, so Unity's bindings already raise only after their cleanup.
Natively, wasmtime 27 has no wasm EH, so the call goes through
`provider_try`/`provider_throw`, a host round trip. That is fine for
debugging. Only the browser counts for performance.

**Guest to managed:**
- `mono_runtime_invoke`: the adapter calls the interpreter.
- A managed delegate's function pointer (`GetFunctionPointerForDelegate`,
  `[MonoPInvokeCallback]`, `UnmanagedCallersOnly`: Steamworks.NET, EOS and
  Rewired callbacks) must be an x86 address that a guest can call:
  - the provider reserves a guest page of **reverse-thunk slots**;
  - each slot is bound in ftab, through a new dynamic bind import
    (`gex.provider_bind(addr, index)`), to one generic adapter;
  - the adapter finds its slot from `ctx::LAST_TARGET`, unmarshals by the
    guest ABI and invokes the interpreter.

## 4. GC

The GC is SGen, configured to behave like Boehm, which is what Unity's
native code was written against:
- **Non-moving and non-generational.** Allocation goes straight to the
  mark-sweep major heap: SGen's degraded-mode path, made permanent. Major
  evacuation is off. UnityPlayer then keeps the guarantees it had:
  - raw `MonoObject*` and `mono_array_addr` pointers stay valid;
  - native stores into managed memory without a write barrier are safe,
    since every collection is a full mark.

  A generational mode can be tried later, behind a flag.
- **Roots, scanned conservatively:**
  - each attached guest thread's stack, from its `mono_thread_attach` RSP to
    its top;
  - its registers in `ctx`;
  - the provider's own C stack.

  Precisely tracked:
  - interpreter frames;
  - handles in the runtime's C code. The upstream wasm runtime already keeps
    object pointers out of wasm locals across GC points (coop handles);
    the shim layer follows the same rule.
- **Register write-back.** Every provider export that can allocate or
  collect has a `NEED_ALL` summary. Its callers, at every tier, then write
  every guest register (callee-saved ones included) to `ctx` before the
  call. Without this, a pointer held only in a function-tier local would be
  invisible to the scan.
- **Stop-the-world:** hybrid suspend.
  - Threads in managed or runtime code stop at the interpreter's safepoints
    (cooperative).
  - Threads running guest code are suspended by gex at the next guest
    safepoint, with the registers flushed to `ctx`. This is the mechanism
    that NtSuspendThread/NtGetContextThread and Boehm's signals already use.
  - Threads blocked in runtime locks are GC-safe.

## 5. Threads

- **Provider state.**
  - The data, heap and C stack per thread are as eject-pe §5.1.
  - It adds thread-local storage: `__tls_base` per instance, with a block
    from the provider region, set up at instantiation.
  - Locks inside the runtime are wasm `memory.atomic.wait`/`notify` on the
    shared memory, which works natively and in Workers.
- **Threads Mono creates** (finalizer, thread pool, `new Thread`) are guest
  threads. `pthread_create` binds a guest stub address to the provider's
  thread-entry adapter. It then creates an ordinary guest thread at that
  address: Linux `clone`, or NT `NtCreateThreadEx`. Every guest thread
  already gets a Worker in the browser. A new platform layer,
  `mono-threads-gex.c` (beside `mono-threads-wasm.c`), supplies thread ids,
  stack bounds and suspend/resume through host imports.
- **`mono_thread_attach` / `detach`** from Unity's job workers register the
  calling guest thread.

## 6. Performance (browser only)

The interpreter is the bring-up tier. The jiterpreter is not used (it
generates wasm32 traces from JS); the build turns it off (`-jiterp`).

The user's decision (2026-10-04): both speed tiers, after P2-P4
correctness, with the interpreter kept as the fallback.
- **Normal gex runs (not ejected): the fork's runtime wasm JIT**, ported to
  wasm64/memory64. It is tiered, one module per method, through
  `WebAssembly.Module` in the browser. The port needs:
  - an emitter whose addresses are i64, as gex-emit's M64 work was;
  - a host hook to compile and instantiate a module in the calling guest
    thread's store. gex's own JIT path has one, natively and in the
    browser: the module is published like any other, and each thread's
    store instantiates it.

  The jiterpreter stays off unless a profile says otherwise.
- **Eject: .NET AOT, no JIT.** At build time, Mono AOT (LLVM, to wasm64)
  compiles CoreLib, the BCL, `UnityEngine.*` and the game's `Managed/*.dll`.
  The game's assembly set is fixed and recorded in the session. The output
  fits stream E's model (docs/eject-pe.md):
  - the AOT code links into the Mono provider module: one stateful
    provider, its data and heap in the provider region;
  - interpreter fallback only for what AOT could not see (generic
    instances), which the coverage report names;
  - the provider is a precompiled module like any unit (`.cwasm`
    natively, `c.wasm` in the page);
  - UnityPlayer's own x86 code is units with region entries, as for any
    image.

Native gexc runs the same provider on wasmtime. It is for correctness only.

## 7. Prototype plan

| step | what | done when |
|---|---|---|
| P1 | The fork's Mono built for wasm64 with the interpreter; CoreLib from the same tree for linux-x64; System.Native for wasm64. **Done** (§9). | `mono_jit_init` + CoreLib load, run natively |
| P2 | Stateful providers for x86-64: SysV adapters, M64, TLS, guest-thread creation, Linux shared objects; the Mono runtime packaged as one. **Done** (§10). | tests/unity-dotnet/prov64, tests/unity-dotnet/embed |
| P3 | Calls in both directions, raises through guest frames, the Boehm-like GC with guest roots and stopped guest threads. **Done** (§11). | tests/unity-dotnet/embed |
| P3 | `tests/unity-dotnet/embed.c`: a small x86-64 Linux program that dlopens a stub `libmonobdwgc-2.0.so` (exports only) and, like UnityPlayer, calls `mono_jit_init_version`, opens a test assembly, finds a class and method, registers an icall, calls `runtime_invoke`. The managed code calls the icall back, which raises; it also uses GC handles, strings, arrays and a GC with an object held only on the x86 stack. | it prints the same as the same program on host Mono |
| P4 | Muck: the shim layer for the 272 names, the find-plugin callback, failed-lookup logging. | the player log reaches "Completed reload", then the menu, natively |
| P5 | The browser: the provider in the browser shim, wasm EH for icall exits. | Muck's menu in Chromium. Then the AOT tier, and fps measured paired with Muck on x86 Mono (`GEX_UNITY_DOTNET=0`) |
| P6 | Rain World and Hollow Knight (windows-x64 BCL; Win64 ABI). | first scene |

## 8. Risks

- **What UnityPlayer assumes about runtime internals.** It may read
  `MonoClass` or `MonoVTable` fields directly rather than through the API.
  If it does, the offsets are the legacy Mono fork's. Mitigation: the API
  and `mono_unity_*` header queries cover the known cases; the P4 trace
  (accesses from UnityPlayer into the provider's data region) finds the rest.
- **BCL behaviour on .NET 10:**
  - APIs that throw on .NET (`BinaryFormatter`, `Thread.Abort`,
    `NetDataContractSerializer` in Rain World's firstpass);
  - culture data: start with invariant globalization; ICU is the guest's
    libicu on Linux, NLS through Wine on Windows;
  - crypto (the guest's OpenSSL, or bcrypt through Wine).

  Each is handled in a replacement assembly or the shim, never in the game.
- **The wasm64 port of Mono** is new upstream territory: the interp
  transitions, `mini-wasm`, and later the AOT compiler's wasm64 output.
  The interpreter is pointer-size clean on x64 and arm64, which reduces the
  risk.
- **V8's memory64** performance and limits. The guest is M64 regardless.
- **Size:** the provider (about 5 MB of runtime) plus the AOT output of every
  managed assembly (tens of MB for Hollow Knight).
- **Icall crossing cost:** 7,700 to 9,750 icalls, thousands of calls per frame.
  Each call is an indirect call plus the ABI placement. If it shows in a
  profile, specialised per-signature thunks are generated at AOT time.

## 9. P1 as built (2026-10-04)

`tools/unity-dotnet/mono/build.sh OUT` clones the fork at 6812ea26, applies
`patches/`, and builds:
- the runtime (`libmonosgen-2.0.a`, `libmono-ee-interp.a`, the icall table,
  the components) with emscripten for wasm64;
- `libSystem.Native.a` (the browser's build of the BCL's native shim) and
  its DllImport table (`entrypoints.c`);
- CoreLib (`./build.sh -subset mono.corelib -os linux -arch x64`).

`tests/unity-dotnet/smoke/run.sh OUT` links a test program against it and
runs it under node (V8, memory64; node 24+ needs no flag). It covers:
- CoreLib boot on the interpreter;
- loading `t.dll`;
- class and method lookup;
- `mono_add_internal_call`;
- `mono_runtime_invoke`, running `List`, `Dictionary`, `int.ToString`,
  `GC.Collect`, the internal call, string concatenation and reflection
  (`Assembly.GetName`);
- a full collection.

`IntPtr.Size` is 8. It passes.

The patches (`patches/0001-wasm64.patch`):
- **CMake:** a `wasm64` processor (`TARGET_WASM`/`HOST_WASM` with 8-byte
  pointers, plus `TARGET_WASM64`/`HOST_WASM64`).
- **`mono-threads-wasm.S`:** the stack bounds as i64.
- **`jiterpreter.c`:** two `static_assert`s that compared an `int` with a
  pointer's size.
- **The interpreter's native calls.**
  - The fast icall path calls through a C type with all-`gpointer`
    arguments. On wasm64 an `int32` is i32 in the callee's wasm signature,
    so only i64-typed signatures take the fast path
    (`interp_type_as_ptr`).
  - The interp-to-native signature cookie writes pointer-sized values as
    `L` (`aot-runtime-wasm.c`).
  - An `I8` takes one argument slot.

`host/gexmono.c` stands in for the browser driver: it does what
`src/mono/browser/runtime` does, minus JS.
- Runtime properties: invariant globalization.
- The interp-to-native table, from `host/gen-m2n.py`:
  - every `I/L/F/D` cookie up to 4 arguments, plus `m2n-extra.txt` (found
    by running);
  - the browser generates this table per app with WasmAppBuilder.
- The DllImport fallback for `libSystem.Native`.
- Interpreter-only mode, with the jiterpreter and the fork's JIT off.

Not yet:
- The **module format.** This is an emscripten main module run by node. The
  provider is a PIC module with emscripten's PIC libc for wasm64, whose
  syscall and thread imports gex provides; that is P2, on libprov.
- **The rest of the BCL** for the same tree (`-subset libs.sfx`; the smoke
  test uses the installed .NET 10's facades).
- **ICU.**
- **SGen in Boehm mode.**
- **The `gex` thread platform.** With threads, `mono-threads-wasm` requires
  coop suspend, so this build is coop.
- **The fork's JIT on wasm64:**
  - its emitter's addresses;
  - pointer-to-int casts in `interp.c`'s JIT entry and in the jiterpreter,
    which the compiler flags.

## 10. P2 as built (2026-10-04)

**Stateful providers for x86-64 Linux guests with a 64-bit memory**
(shims/native libprov, additive; streams E and W share the file):
- `Extras::memory_base` is u64, and the provider stack global is i64 under
  M64.
- `register64`: the `gex.provider_*` imports with i64 pointers and table
  indices, plus `provider_thread_entry` and `provider_stack`.
- A module containing musl's `__gex_thread_start` gets a guest address of
  its own (low, read-only) bound to it, for `clone`.
- `libprov::on_linux_maps`: when a Linux shared object a provider serves
  maps executable, its `.dynsym` exports are bound before the loader can
  call into it.
- `GEX_WASM_DLLS=soname=module.wasm` applies to Linux programs too.

**The provider runtime:**
- `tools/unity-dotnet/sysroot` builds musl wasm64 and compiler-rt.
- `tools/unity-dotnet/provider/gexprov64.h` gives the SysV/Win64 register
  view, entry (which sets the thread context, and attaches musl's
  per-thread state on a thread's first visit) and return.
- musl's `__gex_provider_init` runs once per process:
  - libc's own state, from a fake auxv and `/proc/self/environ`;
  - the module's constructors.
- `__gex_provider_attach` runs once per guest thread: a `struct pthread`, a
  TLS block, and the C stack libprov gave the thread.
- Threads the provider creates are guest threads (`clone`), started at the
  bound entry.

**Mono as a provider** (`tools/unity-dotnet/mono/build.sh OUT`):
- The fork is configured `HOST_GEX` (patches/0001-wasm64.patch):
  - a `wasm64` processor;
  - POSIX threads, mmap and dl, with `mono-threads-gex.c` for cooperative
    suspend;
  - no jitdump, no mini-posix signals;
  - `eng/native` taught wasm64 Linux (64-bit, not 32).
- System.Native is built for the same target, with no ICU, Kerberos or
  OpenSSL.
- `host/gen-adapters.py` writes a SysV adapter for every public `mono_*`
  function the build defines (661). Callback-taking functions are skipped:
  they need guest calls, which is P3.
- `host/gexmono-provider.c` does the browser driver's pre-init before
  `mono_jit_init*`, and supplies Mono's two C++-exception entry points
  through `gex.provider_try`/`throw`.
- The result is one 17 MB PIC module (with names). It imports only the
  memory, the table, the three PIC globals, `gex.syscall` and five
  `gex.provider_*` functions.

**Tests** (native, `--mem64`):
- **tests/unity-dotnet/prov64.** An x86-64 glibc program dlopens `libgt.so`
  and calls it from three threads. The calls cover integers, 8 arguments,
  doubles and floats, TLS, malloc, write, getenv, and the library's own
  pthreads. The wasm64 provider runs instead of the library, and the output
  matches the native run.
- **tests/unity-dotnet/embed.** An x86-64 program does what UnityPlayer
  does: it dlopens `libmonobdwgc-2.0.so` (a stub whose every export
  aborts) and dlsyms the API. Under gex:
  - CoreLib boots on the interpreter;
  - `t.dll` loads, and class and method lookup work;
  - `mono_runtime_invoke` runs collections, `GC.Collect`, string
  concatenation and reflection;
  - a full collection completes.
- tests/x86-programs 69/69, tests/providers 3/3.

**Not yet (P3):**
- calls from managed code into guest code: icalls, P/Invokes, callbacks;
- reverse thunks;
- `mono_raise_exception` through guest frames;
- conservative scanning of guest stacks and contexts, and SGen in its
  Boehm-like mode;
- signals at a provider's syscall (musl's `__gex_signal_frames` stops
  loudly);
- the browser shim's side (`register64` and `on_linux_maps` are native
  only);
- the Win64 ABI and NT processes, where every syscall is an NT call, so the
  provider's libc needs an NT-side route.

## 11. P3 as built (2026-10-04)

- **Calls into guest code** (`provider/gexprov64.c`, `gexp_call_guest`):
  - arguments go by the guest ABI, below the current guest RSP;
  - the return address is in the provider's code region, so a return that
    no function body predicted is still caught by gex's call-out;
  - a dispatch loop runs until the guest returns there;
  - `gexp_call_guest_catching` wraps the call in `gex.provider_try`.
  `gexp_raise` unwinds back to that call and restores the guest's
  registers. `mono_raise_exception` from guest code becomes the guest
  call's pending exception.
- **Mono's side** (`patches/0001-wasm64.patch`, `HOST_GEX`):
  - a native function pointer outside the provider's own table slots is a
    guest address;
  - `ves_pinvoke_method` marshals guest calls by the signature (SysV
    eightbyte classification, Win64 rules) and `do_icall` by its
    pointer-sized form;
  - every `dlopen` goes to the provider's fallback: System.Native linked
    in, else the guest's own `dlopen`/`dlsym` (`gex.provider_guest_sym`),
    `"__Internal"` included.
- **Calls from guest code into managed code** (delegates,
  `[UnmanagedCallersOnly]`) go through reverse slots: 3,840 guest
  addresses in the code region, all bound to `gexp__reverse`, which runs
  the native-to-managed wrapper on the interpreter
  (`gex_interp_native_to_managed`).
- **The GC behaves as Boehm did:**
  - small objects go straight to the mark-sweep major heap, and
    evacuation is off, so nothing moves and every collection is a full
    mark;
  - each thread's guest frames (context RSP to the end of its stack
    mapping) and 16 guest registers are scanned conservatively;
  - an attached thread runs guest code in a GC-safe region;
  - stop-the-world also parks the guest's threads at their next safepoint
    or syscall (`libprov::GuestStop`).
- **libprov (mem64, additive)** gains these imports: `provider_abi`,
  `provider_table_size`, `provider_guest_sym`, `provider_code_page`
  (64 KiB: thread entry, return addresses, reverse slots),
  `provider_thread_register`, `provider_park`,
  `provider_stop_guest`/`provider_start_guest`, `provider_guest_stack_end`.
- **Gate (coordgate on 49a5773b):**
  - workspace tests pass (a64 452, x86 441);
  - tests/nt 73/73 with `--no-cgen` and with `--cgen` (after the fix
    passing `--mem64` to unit builds);
  - x86-programs and glibc pass;
  - lockstep agrees.

  The browser NT step was stopped before it ran, because E was timing in
  the browser. The browser shim now builds (the fjit `volatile` argument).

## 12. P4 state and resume (2026-10-04, at the move)

- `host/gexmono-unity.c` implements the rest of the API UnityPlayer looks
  up:
  - `mono_unity_*`;
  - the legacy names;
  - the `unity_mono_*` three;
  - the guest-callback functions;
  - Unity's find-plugin callback and dl fallback;
  - the assembly search path, with `GEXMONO_ASSEMBLIES` (CoreLib plus
    .NET 10's managed shared framework) ahead of Unity's Managed
    directory.

  MonoClass gains `unity_user_data`.
- libprov binds a replaced Linux library's constructors and destructors
  (DT_INIT/INIT_ARRAY/DT_FINI/FINI_ARRAY) to `gexp__noop`, so none of
  `libmonobdwgc`'s code runs.
- `GEXMONO_TRACE=1` logs every embedding API call with its first arguments.
  It goes to stderr, which is the player log.
- **Muck** (`tests/games/muck-dotnet.sh OUT RUNDIR`, batch mode, no
  graphics, natively):
  - it reaches "Begin MonoManager ReloadAssembly" and "Completed reload,
    in 15.156 seconds": every game assembly loads on CoreLib and the
    facades;
  - it then aborts in `PersistentManager::PostReadActivationQueue` →
    `MonoScript::Rebuild` → `AttributeScannerQuery::UpdateAttributeUsageInfo`;
  - the trace ends `mono_custom_attrs_has_attr` → `mono_custom_attrs_get_attr`
    → `mono_gc_wbarrier_set_field (0, …)` → `mono_field_get_offset (0)`;
  - so the attribute object or a field lookup came back NULL, followed by
    an assertion on a NULL class.
- **On gooner (2026-10-04, in progress):**
  - Coordgate (BROWSER_NT=1, JOBS=6) on d69c1cd6, which is everything up
    to the move: green. Workspace a64 452 and x86 441 pass; tests/nt 73/73
    with `--no-cgen` and with cgen prefix units; x86-programs 69/69; glibc
    passes; lockstep agrees; browser NT 49/0. Output is in
    `scratch/unity/gate/`.
  - Paths: project `~/gex-sandbox`; build output (`OUT`) is
    `~/gex-sandbox/scratch/unity/dnc` (re-cloned; `dnc/src` is the fork at
    6812ea26 with `patches/` applied); musl from `wt/musl-wasm64`.
  - Toolchain: .NET SDK 10.0.110 (runtime 10.0.10, the shared framework
    the test copies) installed with dotnet-install.sh in `~/.dotnet`; put it
    on `PATH`. CMake also needs `llvm-strings`/`llvm-readelf`/`llvm-link`,
    which on gooner live only in `/usr/lib/llvm-22/bin` (build.sh appends it).
  - No taskset on gooner: build.sh runs at nice 19 only; `muck-dotnet.sh`
    pins only if `CORES` is set. Use the Linux build:
    `MUCK=~/gex-sandbox/games-linux/Muck` (the Steam symlink points at the
    Windows one).
  - The NULL lookup: `AttributeScannerQuery::UpdateAttributeUsageInfo`
    (UnityPlayer.so 0xaf3820) reads AttributeUsageAttribute's fields by
    name, `m_attributeTarget`/`m_inherited`/`m_allowMultiple` (falling back
    to Mono's old `valid_on`/`inherited`/`allow_multiple`). .NET's CoreLib
    names them `_attributeTarget`/`_inherited`/`_allowMultiple`. Fix: a
    hand-written `mono_class_get_field_from_name` in gexmono-unity.c that
    retries `m_x` as `_x`, then an alias table, and logs
    `[gexmono] no field ...` on a final miss.
  - Next, `scripting_field_has_attribute` asserted in Mono's
    `find_field_index`: UnityPlayer's serializer passes the class it is
    serializing (`List`1`) with an element type's field. The hand-written
    `mono_custom_attrs_from_field` uses the field's own parent.
  - Next, `CANNOT HANDLE INTERP ICALL SIG LLLLLLLL`: the m2n wrappers now
    cover every function type in the provider (build.sh links, reads the
    type section with `gen-m2n.py --types-from`, regenerates, relinks;
    1,895 wrappers).
  - **Result (21dbdeab, run `scratch/unity/runs/g3`):** Muck loads its menu
    scene in batch mode and runs the game's scripts on .NET Mono: save file
    created, settings applied, "Initializing packets", then the main loop
    until the timeout. Steam is absent, so Facepunch's `SteamClient.Init`
    throws and `SteamPacketManager.Update` logs a NullReferenceException
    each frame, as it would on Unity's Mono without Steam.
  - **With graphics (run `scratch/unity/runs/x2`, screenshot
    `scratch/unity/screens/muck-dotnet-menu.png`):** `GFX=GLDIR
    tests/games/muck-dotnet.sh` runs Xwayland inside the guest on gex's
    compositor (`tests/games/xwayland-inguest.sh`; gooner's Xwayland 22.1
    has no `-geometry`) with gex-gl for x86-64 (`GEX_CC=gcc guest/gl/build.sh
    scratch/unity/gl64`; needs the opengl-registry submodule). Unity creates
    its OpenGL 4.5 device and **Muck's main menu renders completely** (scene,
    grass, UI) with all game code on .NET Mono. Native only, so no speed
    numbers (cold translation; the browser is the target).
  - **wasmtime 49 (2026-10-05):** master fd8be1d1 merged (3718d0ca);
    wasmtime 27 dropped. The 64-bit provider imports follow master's error
    port (`crate::host`), the memory64 reservation is
    `Config::memory_reservation`. Gate on 3718d0ca (BROWSER_NT=1, JOBS=6):
    a64 452, x86 441, tests/nt 73/73 both ways, x86-programs and glibc,
    lockstep, browser NT 49/0; output in `scratch/unity/gate2/`. Muck batch
    mode, embed and the mem64-* programs (with and without --fjit) pass.
  - **The --fjit hang (c330a519):** under --fjit Muck spun at "Begin
    MonoManager ReloadAssembly": the function JIT followed UnityPlayer's
    PLT into libmonobdwgc and published Boehm's own code over the provider's
    bindings (GC_exclude_static_roots looping on the provider's
    GC_next_exclusion). libprov records replaced text; `ftab::job` skips it.
    Reload now takes 6.9 s with --fjit. embed has a --fjit leg.
  - **Speed observations:** pass `--fjit` to `muck-dotnet.sh` (it forwards
    gexc flags) for any speed observation. Without it UnityPlayer runs on
    tier-1, and the provider is interpreter-only. x2 (no `--fjit`, llvmpipe,
    gooner at load ~40) presented 0.3-0.5 frames/s at the menu, against
    17-23 fps for Unity's own Mono with `--fjit --mem64` on worldmachine.
    Neither number is a measurement; the browser on the bench box is.
    Coordinator's correction (2026-10-05): on worldmachine (RX 6500 XT,
    idle) the same menu without --fjit, interpreter only, runs 31.5-32.1
    presents/s (GL drain 6.1 ms, readback 1.55 ms): gooner's numbers are
    llvmpipe under load and the menu is not managed-bound. JIT gains are
    measured on worldmachine (`BENCH_HOST=worldmachine scratch/bench/bench.sh`,
    staged layout in `~/src/noita-wasm/scratch/coord-wm` there) in gameplay
    or a managed-heavy scene.
  - **P5 started: the browser shim for a 64-bit memory (docs/mem64.md step
    6), 36e98cd5.** `GEX_MEM64=1 shims/browser/build.sh` (x86) builds the
    shim for wasm64-unknown-unknown (nightly, build-std): a 16 GiB memory64.
    Its JS-facing exports and imports have exactly the wasm32 shim's types
    (checked with `scratch/unity/w64/sigs.py`), so the glue changes are
    small: `GEX_MEM64` creates the memory64, grows it in BigInt pages and
    sets the i64 `__stack_pointer`. `gexc seed --mem64`; gex-emit's dense
    dispatch now addresses memory in its index type (wasm32 output
    identical). The global kernel for these processes
    (`GEX_MEM64=1 shims/kernel/build.sh`, `gex_kernel.x86.m64.wasm`) reaches
    the process memory with i64 (`gex-kmem/process64`). Under node,
    `GEX_MEM64=1 GEX_ARCH=x86 GEX_SHARED=1 run-node.mjs
    web/public/gex/seed.x86.m64.shared.wasm PROG` runs gx64 ack/bits/branchy
    with the same output as the wasm32 shim.
  - **P5 plan, in order:**
    1. **Done (06e6b95b):** `gex_kernel::PAddr`, u64 only in the wasm64
       shim and the process64 kernel; the high arena is backed without
       moving the heap's cursor; `tests/x86-programs/mem64-io.c` passes on
       the wasm64 shim under node. Was: guest addresses to the global kernel were `u32` (the `k_*`
       imports, gex-kernel's remote pipe/unix/pty ops, `rf_*`). In M64
       a buffer above 4 GiB (the high arena) would be truncated. Widen them
       to a `PAddr` that is u64 in the wasm64 shim and the process64 kernel
       and u32 otherwise (M32 unchanged). Test: an x86-64 program that
       reads a file and a pipe into a buffer above 4 GiB.
    2. The x86-programs corpus under node (`scratch/unity/w64/x86p-node.sh`,
       `M64=1` for the wasm64 shim; its exit-status check is broken, output
       only): the wasm64 shim passes 22 of 34, the wasm32 shim 21; every
       wasm64 failure also fails on wasm32 (signals, threads, faults: the
       browser shim's own gaps). Dynamic glibc under node not tried yet.
       **Parked 2026-10-05: the user chose the wasm JIT first.**
    3. The Linux stateful provider in the browser: `libprov`'s mem64 side
       (SysV adapters' provider_abi/table_size/guest_sym/code_page, thread
       register/park/stop/start, guest stack end, reverse slots,
       `on_linux_maps` for a replaced `.so`) next to the existing NT
       stateful providers in cgen.rs and gex.mjs. Test:
       tests/unity-dotnet/embed in node.
    4. Muck in the page: the provider plus the BCL served as files,
       graphics through Mesa (wink) and the page's Xwayland (`hostx=1`).
    5. **In progress (the user: JIT first):** the fork's wasm JIT on
       wasm64. Done (0651b922): `patches/0002-wasm-jit-gex.patch` makes
       the runtime half build for gex (`WJ_RUNTIME`), with the JS parts as
       host imports (`mini-wasm-gex.h`; `shims/native/src/wasmjit.rs`);
       `GEXMONO_WASM_JIT=1` runs the whole pipeline (hotness, emit, host
       compile and per-store instantiate, interpreter fallback) and embed
       stays correct. The emitter now emits wasm64 (2df82f93): memory64
       and table64 imports, `WASM_PTR`/`P_*` ops for pointer-width values,
       pointer-typed locals, consts, returns and call types, the scratch
       block laid out for 8-byte pointers (`WJ_SCR`), long immediates
       (`WJ_LIMM`), no negative memarg offsets (memory64 does not wrap).
       The interpreter's shared vcall/AOT-vcall caches pack 32-bit
       pointers into i64 words and stay off on 64-bit; the worker-local
       vcall PIC has a 64-bit entry ({vtable, f-slot, hits, target}, 24
       bytes, four ways unrolled in the emitter; 368d3d66), and the C-side
       vcall memo is on. Methods with exception clauses are JITted
       (eccc327e): under a wasm64 provider `gex.provider_throw` is a wasm
       exception with a per-store tag (`(i32)` code) that
       `gex.provider_try` takes as the pending exception, JIT modules import
       it as `x.e`, and an EH method's dispatch sits in a `try_table` in
       place of the fork's legacy try/catch (wasmtime and current browsers
       have only the new form). Still refused: localloc, LMF wrappers,
       ldaddr of an argument, reflection residuals (as on wasm32). embed
       (`scratch/unity/wj.sh [build] [THRESH]`, `wjrun.sh`): every result
       right at threshold 1 (513 methods JITted, `T.Guest` catching a throw
       from a guest icall among them), 0 invalid modules.
       Speed (worldmachine, embed `T.Work(20000)` steady state, third call;
       `EMBED_WORK`/`EMBED_REPS`, `scratch/unity/wm/work.sh`): interpreter
       0.795 s, JIT 0.475 s. By part: virtual calls 0.342 -> 0.086,
       interface/generic 0.244 -> 0.050, arithmetic 0.166 -> 0.016, enum
       0.068 -> 0.127, reflection 0.081 -> 0.123, dictionary and exceptions
       even. The first call also pays the compiles (Cranelift, one module
       per method): 1.39 s. Muck runs natively with --fjit and the JIT
       (batch mode, 894 methods JITted, no crash; the NREs from
       SteamNetworking happen interpreted too). For speed observations use
       `muck-dotnet.sh ... --fjit` (and `--env GEXMONO_WASM_JIT=1`).
       Muck's menu on worldmachine (RX 6500 XT, --fjit, 800x600, mean fps
       of the second half of 6 min; `scratch/unity/wm/muck.sh`):
       interpreter 45.9, JIT 44.4, i.e. no change; the menu is not
       managed-bound. Gameplay is out of reach for now: Host Lobby goes
       through Steam's lobby API, and synthetic clicks (xdotool inside the
       guest, XTest) do not reach the game through rootless Xwayland (a
       Settings click opened nothing); input would have to come through
       gex's compositor. Next: the JIT's remaining residual kinds
       (reflection, enum), the delegate PIC on 64-bit, compile cost, the
       page. Assessment (2026-10-05): about
       14,400 lines (`mini-wasm*.inc`, `mini-wasm.c`, `wasm-encoder.c`) on
       Mono's mini IR, with no wasm64 awareness. It is emscripten-bound:
       `EM_ASM` instantiation with `wasmTable`, a per-Worker publish
       protocol, and `setInterval` tier-up ticks. Port: (a) mini's wasm
       arch with 8-byte pointers (`cpu-wasm.mdesc`, `mini-wasm.h`); (b) the
       emitter's addresses as i64 and memory64/table64 imports; (c) publish
       and instantiate through a gex host import (gex already compiles and
       instantiates modules per thread natively and in the page), with
       slots in the provider's table64; (d) EH and GC-root scanning
       re-checked against provider frames. Estimate 5-8 agent-days. It runs
       natively under gexc as well (wasmtime compiles the published
       modules), so native runs get the same tier.
  - Unity's remaining internal calls as they show up; then Stardew/Celeste.
  - **Scripted input (2026-10-05):** `GEX_WL_INPUT=FILE` feeds the native
    compositor timed `move`/`click`/`button`/`key`/`tap` events, timed from
    its `frames N`th frame (`frames 0`: from its start), held until a client
    has a seat and a surface. Checked end to end on worldmachine with
    `tests/x11/xinput-echo.c` on the in-guest (rootful) Xwayland: motion, a
    click and keys arrive. Muck's menu still ignores the clicks (unknown
    why; Muck is set aside, see the coordinator's direction).
  - **Hollow Knight (Unity 6000.0.61, linux-x64 Mono; 2026-10-05):** batch
    mode loads the first scene natively with --fjit
    (`muck-dotnet.sh` with `MUCK=<HK dir> EXE=hollow_knight.x86_64
    UNITY_RELAUNCH=1 STEAM_APPID=367520`). Needed: a PIE rebuild of the
    player's small main (`tests/games/unity-launcher.c`; Unity 6 links it at
    0x200000, inside gex's reserved low region); `mono_dllmap_insert`
    ignored; GC handles as the GC's raw values in runtime, CoreLib and
    provider (Unity 6 tests a handle's bit 0 and its managed code calls
    `GCHandle.FromIntPtr` on raw values); dead-handle reads tolerated; the
    framework's x86-64 crypto and compression natives beside the BCL (HK's
    localisation decrypts with AES). Also: guest signal handlers run at a
    provider's syscalls (`__gex_signal_frames` in gexprov64.c).
  - **Hollow Knight, parked 2026-10-05 (shutdown):** with graphics on
    worldmachine (`scratch/unity/wm/muck.sh` with `GAMEDIR=<HK>
    EXE=hollow_knight.x86_64 UNITY_RELAUNCH=1 STEAM_APPID=367520`; HK is
    copied to `~/src/noita-wasm/games-linux/Hollow Knight` there) two
    problems, after GL 4.6 comes up and physics initialises:
    1. **With --fjit only:** `free(): invalid pointer` (deterministic), from
       Unity's allocator's fallback `free` (UnityPlayer vaddr ~0xf8c0af, called
       via 0xf8a0c0/0xf9559d). Not a `free` the program calls through the PLT
       (an LD_PRELOAD watcher saw none), so glibc-internal or a corrupted
       Unity block. Without --fjit it does not happen: a function-JIT
       translation bug is the first suspect. Next: bisect with the fjit off
       for UnityPlayer ranges, or `GEX_DUMP_ABORT=1` (now works for x86-64)
       and map the frames.
    2. **Without --fjit (and in batch mode with it):** a hang after
       "Unloading 5 Unused Serialized files". `GEX_DUMP_AT=640` shows the
       main thread inside the provider's `mono_gc_collect` (Unity's
       UnloadUnusedAssets), sleeping in a loop (strace: only
       clock_nanosleep); threads 47/48 in libc. Next: the same dump with the
       wasm stacks (`GEX_DUMP_AT` now prints `WasmBacktrace` for each thread;
       run with `GEX_WASM_BACKTRACE=1`) to see what the collection waits for
       -- likely sgen's suspend of a thread attached to Mono (Unity has ~70)
       that never acknowledges, or the provider stop-the-world.
    Then the keyboard script to gameplay (`GEX_WL_INPUT`, works on X
    clients) and interpreter vs JIT on worldmachine.
  - **Hollow Knight after the restart (gooner, 2026-10-05; runs
    `scratch/unity/runs/hk20`-`hk30`, batch mode, no --fjit):**
    1. **The hang is fixed.** The `GEX_WASM_BACKTRACE` dump put the main
       thread in sgen's `unified_suspend_stop_world` restart loop (`g_usleep`):
       one thread stayed "in a critical location" because its saved SP lay
       outside its registered stack. `GEXMONO_TRACE_THREADS=1` (patch 0003)
       and `GEX_PROVIDER_STACKS=1` showed that SP was on thread 70's provider
       stack while the MonoThreadInfo was thread 28's: musl's
       `__gex_provider_attach` gave attached threads no thread-specific data,
       so every attached thread's `pthread_getspecific` read the words at
       address 0 and Mono's thread-info key handed thread 70 thread 28's info
       (musl-wasm64 3f1c1f08; that commit also makes a provider-created
       thread report gex's stack). The stop-the-world now names such a thread
       once (`gex STW: thread ... still critical`).
    2. **Next crash fixed (6407a1ed):** "Cannot transition thread from
       STATE_BLOCKING with DO_BLOCKING": Unity calls the embedding API
       (`mono_gc_get_used_size`, `mono_runtime_invoke`) from guest code a
       P/Invoke called, i.e. inside the interpreter's GC-safe region, and the
       old one-stack crossing hooks left the thread Blocking. Now calls into
       guest code and entries from it are separate pairs (gexprov64.c
       `gexp_hook_call_guest`/`_call_return`, `gexp_hook_from_guest`/
       `_to_guest`, `_mark`/`_unwind` for a raise through guest frames).
    3. HK then unloads 153 assets (the GC that hung) and runs its first
       scene's scripts (no UIManager in batch mode, TextMeshPro font
       warnings). `provider_stack` now reports the stack's real base (it was
       derived from the current SP).
    4. The --fjit `free(): invalid pointer` did not reproduce on gooner
       (hk21: --fjit with graphics, llvmpipe, reached the same unload hang);
       it was seen on worldmachine only. `GEX_FJIT_RANGE`/`GEX_FJIT_SKIP`
       (hex `LO-HI,...`) restrict the function JIT by entry address for the
       bisection.
    5. **Black frames with graphics (gooner, llvmpipe; hk31-hk32):** with
       --fjit HK creates its GL 4.5 device, loads a second scene and the
       compositor gets ~30 presents/s, but every captured frame
       (`f-*.ppm`, 185 of them over 25 min) is all black. Not the Wayland
       backend: `xwayland-inguest.sh` now unsets `WAYLAND_DISPLAY` for the
       program (Unity 6 and SDL could otherwise pick gex's compositor), still
       black. Unknown whether Unity draws nothing (shaders?) or the blit to
       the window is lost. hk33 (`GEXGL_TRACE_GENERATED=1
       GEXGL_SUBMIT_TRACE=1`, periodic tallies every 5000 submits in
       out.txt) was stopped at the reboot before it got far: rerun it, map
       opcodes with `guest/gl/gexgl.h`, and check for draws vs clears/blits.
    6. **The --fjit `free(): invalid pointer` bisection (worldmachine only;
       its glibc is Arch 2.44, gooner's Debian 2.36):** script
       `scratch/unity/wm/fjtry.sh NAME SKIP [RANGE]` (run through
       `BENCH_HOST=worldmachine scratch/bench/bench.sh -- bash
       scratch/unity/wm/fjtry.sh ...`, give each a distinct `XNUM=`; prints
       BAD/GOOD/NONE/STALL; it installs `wm/gexc.new` if present). Addresses
       on worldmachine: libc 0x7d748000-0x7d948000, UnityPlayer
       0x7e041000-0x7fe00000. Results: full --fjit BAD (hkw1, hkw2);
       RANGE=libc only GOOD (b3, reached "Unloading 5"); RANGE=UnityPlayer
       only, libc+UnityPlayer, SKIP=UnityPlayer, SKIP=libc: all stall
       before the GL device (b1, b2, b4-b8; b7/b8 after the VMA-scan fix in
       `job`). So something besides libc and UnityPlayer is involved, and
       jitting a subset while other code stays on tier-1 stalls (itself
       worth a look: a tier-1/fjit interplay bug). b9-b11 (libc + the small
       libs 0x7d948000-0x7e041000; libc + above UnityPlayer; everything below
       0x7d948000) were killed by the reboot: rerun them first.
    7. Next after these: GEX_WL_INPUT keyboard script into gameplay, then
       interpreter vs JIT on worldmachine (stage with `scratch/unity/wm`:
       copy target/dbg-x86/release/gexc, dnc/provider/mono.wasm,
       dnc/lib/System.Private.CoreLib.dll, tests/games/*.sh; HK lives in
       `~/src/noita-wasm/games-linux/Hollow Knight` there), then the next
       GAMES-CORPUS.md games.
    **Build and run recipe (gooner):** `. scratch/env.sh`; gexc:
    `cargo build --release -p gexc --features gexc/x86 --target-dir
    target/dbg-x86`; provider: `tools/unity-dotnet/mono/build.sh
    ~/gex-sandbox/scratch/unity/dnc sysroot runtime provider` (sysroot when
    musl-wasm64 changed; it builds wt/musl-wasm64, branch gex-wasm64, pushed
    to mw at 3f1c1f08); HK batch: `MUCK="$HOME/gex-sandbox/games-linux/Hollow
    Knight" EXE=hollow_knight.x86_64 UNITY_RELAUNCH=1 STEAM_APPID=367520
    GEXC=$PWD/target/dbg-x86/release/gexc sh tests/games/muck-dotnet.sh
    ~/gex-sandbox/scratch/unity/dnc RUNDIR SECS [--fjit]`; add
    `GFX=$HOME/gex-sandbox/scratch/unity/gl64 XNUM=N EVERY=300` for
    graphics. Regression checks used: `tests/unity-dotnet/embed/run.sh
    ~/gex-sandbox/scratch/unity/dnc target/dbg-x86/release/gexc` (PASS) and
    Muck batch with --fjit (main loop runs).
    Working-tree note: `dnc/src` has every patch staged (`git add -A`) so
    `git diff` shows only new edits (how 0003 was made). Run `git -C
    scratch/unity/dnc/src reset -q` before build.sh's `src` step.
- **Resume (after the 2026-10-05 evening reboot; HEAD 8bfbae51, master
  e3917bac merged; parked 2026-10-05 ~20:55 at the commit that adds this note (after 8c98f89b); state in items 4-5 of the HK list below):**
  0. Gate head 8bfbae51 offered to the coordinator (the branch gate has
     never run on anything after d69c1cd6/3718d0ca). Extra legs that prove
     this stream's work, all checked green on 8bfbae51 on gooner:
     `tests/unity-dotnet/embed/run.sh ~/gex-sandbox/scratch/unity/dnc
     target/dbg-x86/release/gexc` (interpreter, --fjit, and since 8bfbae51
     the wasm JIT at threshold 1, plus the GC-roots control);
     `tests/unity-dotnet/prov64/run.sh
     ~/gex-sandbox/scratch/unity/dnc/sysroot/sysroot GEXC` (note the doubled
     `sysroot`); Muck batch with --fjit (`muck-dotnet.sh ... 600 --fjit`,
     player.log reaches "Initializing packets"); HK batch (reaches its first
     scene's scripts: "Couldn't find a UIManager"). Logs in
     `scratch/unity/logs/`, runs `scratch/unity/runs/mk2`, `hk34`.
  1. The fork clone `dnc/src` has a local commit `2a05cbbc5` (patch 0001)
     on top of 6812ea26, so build.sh's `src` step fails ("local changes
     would be overwritten"). The working tree was verified identical to
     6812ea26 + patches/0001-0003 (tree hashes equal), so build with the
     steps `sysroot runtime provider` (skip `src`).
  2. **HK black frames (gooner, runs hk35-hk37, hk40):** not a GL
     translation problem: Unity never presents. `GEX_GL_CAPTURE=FILE` (host
     env) recordings (`scratch/unity/gltr.py FILE hist|frame N|draws`) of
     hk36 and hk37 are byte-identical in size (70,337,112 bytes) and hold
     no PRESENT, no draw, one CLEAR, no shader: the stream stops after a
     TEX_STORAGE_2D during the first scene's load. `--strace` (hk37): the
     render thread (guest tid 68, same ring as the main thread) made 4
     submits and then waits in futex/clock_gettime, idle, while the main
     thread keeps running scripts. So the player loop stops sending
     frames. Suspects: (a) the window never gets focus or visibility in the
     rootful in-guest Xwayland with no window manager, and HK (unlike
     Muck) does not run in the background; hk40 (`GEX_WL_INPUT` pointer
     moves into the window every 7 s) was to test this but died first, see
     3; (b) the game logic: "Couldn't find a Game Manager" in both batch
     and graphics runs. `GEXMONO_TRACE=1` (hk38) shows Unity's
     FindObjectOfType loop testing every object against GameManager's class
     with `mono_class_is_subclass_of` and finding none. A control on
     Unity's own Mono (run `hkst`, no provider) is running to tell whether
     that message is normal for this scene. Also seen: SDL "window backend
     is (null)" and "Video subsystem has not been initialized" early on,
     then a window at 800x600 anyway (same in hk31/hk32).
  3. **--fjit memory corruption is intermittent, and not only on
     worldmachine:** hk40 on gooner died at startup with `*** stack
     smashing detected ***` (same flags as hk36, which ran). On worldmachine:
     b9 (RANGE libc + small libs) GOOD; b10 (libc + above UnityPlayer) and
     b11 (everything below 0x7d948000) stalled before player.log existed;
     b12 (full --fjit, `GEX_DUMP_ABORT=1`) BAD, dump in
     `scratch/unity/wm/res/b12-out.txt`: main thread during physics init,
     return addresses UnityPlayer+0x8920af/+0x8900c0 (vaddr 0xf8c0af, Unity's
     allocator after `pthread_getspecific`) -> libc free; b13 (SKIP the
     allocator 0x7e8d0000-0x7e8de000) failed differently: thread 49
     SIGSEGV at pc 0 after 4 min. On gooner, batch HK with RANGE=UnityPlayer
     only (hkr1) and full --fjit (hkr0) both reach the first scene, so the
     worldmachine "stalls" with partial ranges are not a deterministic
     tier-1/fjit interplay bug. Conclusion so far: a nondeterministic
     corruption under --fjit (canary, malloc chunk, a null call target),
     more likely a race or a per-thread state slip (TLS/fs base?) than one
     mistranslated function; range bisection cannot pin it. Next: repeat
     identical runs to measure the failure rate, then look at thread
     creation/fs-base and publication races in the function JIT.
  4. **Parked for the reboot (2026-10-05 ~20:55; head below).** Results since 3:
     - **Repeat runs (`scratch/unity/rep.sh PREFIX N SECS`, batch HK --fjit,
       gooner glibc): r1 8/8 OK.** Batch mode does not show the corruption.
     - **Arch glibc on gooner reproduces a crash deterministically, in
       graphics mode only:** `GUEST_LIBS=scratch/unity/archlibs` (worldmachine's
       glibc 2.44 + libstdc++/libgcc_s; muck-dotnet.sh relinks the launcher with
       that ld.so as interpreter, 8c98f89b). Batch hkA1 OK; graphics hkA2, gs3,
       gs4, gs5 all exit 139 during startup (before player.log, while ld.so
       loads SDL's X11 libs). gs5 with `--verbose`: `guest terminated by
       signal 11 at pc=0x21c6: branch to 0x21c6 ... last indirect target 0x21c6
       from block 0x4a0bbfe` (block 0x4a0bbfe is in ld.so, low region:
       `mov rax,[rbp-0x60]; mov [r15],rax; ... ret`: a return to 0x21c6, i.e. a
       corrupted return address/stack, stack 0x7cc94224 (libmd), 0x4a113ef).
       gs4 has a full `--strace` (`runs/gs4/out.txt`). **Next: this is the
       repro to chase** (same run without --fjit to confirm it is the function
       JIT; then GEX_FJIT_RANGE on ld.so 0x4a00000-0x4a40000 vs libmd/libbsd).
       Pass `--verbose` always: guest-signal deaths are otherwise silent.
     - Native gooner glibc graphics: hk40 stack smashing at startup (1 of ~8
       graphics runs); gs1/gs2 (GEX_TRACE_SEGV) were still loading when stopped.
  5. **HK black frames, findings:**
     - **Focus:** without pointer focus Unity never presents (hk36/hk37: 0
       PRESENT). hk41 (`GEX_WL_INPUT` moves into the window every 7 s,
       `runs/hk40/input.txt`): 11,148 PRESENTs, but every frame black, no draw,
       no shader, one CLEAR in the whole trace. Fix for the harness: a default
       pointer-in-window input script in GFX mode (to do).
     - **No camera renders because the game never leaves Pre_Menu_Intro:**
       `GEXMONO_TRACE_SUBCLASS=NAME` (provider built into `scratch/unity/dnc2`,
       uncommitted-to-dnc copy with symlinks) shows FindObjectOfType<UIManager>
       testing only Pre_Menu_Intro objects (StartManager, LogoLanguage, ...)
       long after "Unloading 153": Menu_Title (level1, holds _GameManager,
       _GameCameras, _UIManager per UnityPy) is never activated. Decompiled
       StartManager.Start (ilspycmd in the session scratchpad; reinstall with
       `dotnet tool install --tool-path X ilspycmd`) waits for the Animator to
       reach "LoadingIcon", then for PlayerPrefs; neither "Waiting for
       PlayerPrefs" nor "Didn't need to wait" is logged, so it is stuck at the
       animator wait (probably culled/never updated since no camera renders,
       and in batch mode never). "Couldn't find a Game Manager" in
       Pre_Menu_Intro is expected (no GameManager there).
     - Controls: stock Unity Mono under gex (hkst3) stalled after physics init
       (47 log lines, 1 h). Native HK on gooner segfaults in batch mode
       (strcasecmp(NULL,"x11") in PlayerMain: no video driver; gex survives it
       because address 0 is readable).
     - Next: why the intro Animator/camera does nothing (check the Camera's
       enabled state and Animator culling via the trace; try `-force-...` or
       pointer focus from frame 0), then the default focus input.
  6. **After the Claude Code restart (2026-10-05 ~21:00; gooner load 90-180,
     so every graphics run takes 25+ min to reach Unity's log):**
     - **The Arch-glibc graphics crash is the function JIT's:** gs6 (same as
       gs5, no --fjit) gets past the point where every --fjit run died (78 log
       lines, physics init, killed by its 1500 s timeout). gs7 (`GEX_FJIT_RANGE`
       = Arch ld.so 0x4a00000-0x4a40000 only) neither crashed nor reached
       Unity's log in 1500 s (the partial-range stall, now seen on gooner
       too). gs8 (`GEX_FJIT_SKIP` = that ld.so range, everything else jitted)
       is running, 2400 s. The faulting block (gs5) is the epilogue of Arch
       ld.so's `_dl_lookup_symbol_x` (file offset 0xbbfe: `*ref = sym; lea
       rsp,[rbp-0x28]; pop x6; ret`), which returned to 0x21c6: its saved
       return address or rbp was overwritten. Next: if gs8 passes, dump that
       function's translation (`GEX_FJIT_KEEP`/`GEX_FJIT_DUMP` with
       `GEX_FJIT_RANGE` around 0x4a0b000-0x4a0c000) and give F the GIR.
     - Pre_Menu_Intro's settings (UnityPy): cameras `Camera` and
       `AspectBorders` enabled (clear flags 1, full viewport, display 0);
       StartManager's Animator culling 0 (always animate), update mode 0
       (scaled time). So no culling explains it: either the player loop runs
       very few frames (11 FindObjectOfType<UIManager> in ~20 min) or time
       does not advance. hk46 (pointer in the window from the first frame,
       `runs/hk46/input.txt`, GL capture, 3600 s) is running.
  7. **Root cause found for the --fjit corruption (763fc74c):** the function
     JIT's learned state (builtins by address, function starts, noreturn,
     translated entries) was global to the host process, and the native
     runner hosts the whole guest process tree (sh, Xwayland, xkbcomp, the
     game) with one translator. Builtins were replaced on every execve, so
     another process's memcpy/memset addresses (by symbol, from its ld.so and
     exe) were served as host builtins in the game's code. Fix: per-process
     state keyed by a `Process` id in `ftab::Shared` (shared with a forked
     child until execve), `Translator::image`/`forget`. Regression test
     `tests/fjit-procs/run.sh GEXC` (fails on the old gexc: "after probe: 6";
     passes now). Explains: graphics-only (batch has one process),
     worldmachine-only BAD (different glibc layout from Debian's), intermittent
     on gooner (hk40), and the partial-range "stalls" are separate (not yet
     looked at). **Confirmed on worldmachine:** b14 (fjtry, full --fjit,
     graphics, the fixed gexc) GOOD (reached "Unloading 5"), where hkw1, hkw2
     and b12 were BAD with the old one. gs13/gs14 on gooner (worldmachine
     glibc) were still loading under load ~160 when this was written.
     hk46 (focus from frame 0): 5,797 PRESENTs, still no draw/shader, same log
     as hk41: the intro does not advance; next on that.
  8. **Checks on the fixed gexc (gooner, 2026-10-05 late):** x86-programs
     72/72, glibc 2/2, tests/nt 81/81 with `--no-cgen` and 81/81 with cgen
     prefix units; gs13/gs14 (worldmachine glibc, full --fjit, graphics) ran to
     their 3000 s timeout in the first scene with no crash (4 of 4 crashed
     before the fix).
  9. **HK intro, why it does not advance: it is waiting for the language
     choice.** `tools/unity-dotnet/probe/build.sh DATA_DIR OVERLAY` builds a
     diagnostic assembly that Unity runs through RuntimeInitializeOnLoads.json,
     and `muck-dotnet.sh OVERLAY=` (with UNITY_RELAUNCH) puts it in the run's
     copy of the game. Run hk48 (batch): game time advances normally (time
     scale 1, ~30 frames/s of game time, `Time.time` tracks real time),
     first-chance exceptions are reported (the probe's self-test shows) and
     none other is thrown, so nothing is swallowed. StartManager:
     `CheckIsLanguageSet() = False`, `Platform.Current.ShowLanguageSelect =
     True`, `confirmedLanguage = False`, Animator parameters all false: a fresh
     HOME has no language saved, so the first-run language select waits for a
     confirmation. Not a gex bug. Next: seed the language (Unity PlayerPrefs in
     the run's HOME, or scripted input on the language screen), then the
     graphics question that remains: with focus, Unity presents but draws
     nothing, although both intro cameras are enabled and active (probe:
     `Camera`, `AspectBorders`, 640x480).
  10. **After the language screen (2026-10-06, runs hk49-hk65; dnc2 provider):**
     - **Seed:** `HOME_SEED=tests/games/seeds/hollow-knight` (muck-dotnet.sh)
       puts Unity PlayerPrefs with GameLangSet=1 in the run's HOME, in both
       `unity3d/Team Cherry/Hollow Knight/prefs` and `unity3d/unknown/unknown/
       prefs` (graphics start-up opens the latter before it knows the
       product). The language select is skipped; the intro animator reaches
       LoadingIcon.
     - **Liveness was stubbed:** `mono_unity_liveness_*` reported nothing, so
       Unity's UnloadUnusedAssets unloaded every asset only managed code held:
       StartManager's `loadSpinnerPrefab` came back destroyed and
       `Instantiate` threw ArgumentNullException, ending the intro coroutine.
       Implemented (gexmono-unity.c): a walk over reference fields, arrays and
       value-type fields from each root and from all loaded classes' statics,
       reporting objects of Unity's filter class in batches, each object once
       per pass across Unity's 24 states; `mono_unity_stop_gc_world` holds the
       sgen GC lock. HK now unloads 82 assets instead of 153 and keeps the
       prefab, **but then faults** in `UnityEngine.Object::Internal_CloneSingle_
       Injected` (Instantiate of the spinner): a Unity native function reads a
       wild pointer (GEX_TRACE_SEGV dump, named frames with GEX_CGEN_NAMES=1:
       f_7e69d820 icall -> f_7ea6bd90 -> f_7ea6a590). Just before it, Unity
       iterates one class's methods 182k times (mono_class_get_methods +
       mono_method_signature, no other API calls): the serializer creating a
       [Serializable] field's objects. Not the GC lock (GEXMONO_LV_NOLOCK=1
       crashes too); with GEXMONO_LV_OFF=1 (walk, no reports) no crash. Next:
       which class, and whether Unity reads MonoMethod/MonoMethodSignature
       fields directly at Unity-Mono offsets there.
     - **Focus:** `tests/games/xfocus.c` (muck-dotnet.sh builds it and
       xwayland-inguest.sh runs it unless NOFOCUS=1) gives the largest top-level
       window the X input focus: Application.isFocused becomes True.
     - **Cameras never render on their own:** probe `prerender 0` for the
       whole run while Unity presents ~30/s; `GEXPROBE_RENDER=1` (Camera.Render
       by hand) works (prerender 1). The probe shows `Display.main` with
       rendering and system size 0x0 (Screen is 800x600): Unity's display list
       was built while SDL video was not initialized ("Error getting num native
       displays: Video subsystem has not been initialized", "Desktop is 0 x 0").
       The in-guest Xwayland has RandR (1 output, 1 monitor 800x600) and
       Xinerama (scratchpad xinfo test), and SDL's X libraries load and connect.
       SDL_VIDEODRIVER=x11 / XDG_SESSION_TYPE=x11 change nothing. Next: why
       SDL video is not up when Unity first asks (strace in runs/hk65).
  11. **SDL video fixed (e50a8cb5):** the strace (hk65) shows SDL_Init
     (timer|video|joystick|haptic|gamecontroller) failing on libudev's monitor:
     `socket(AF_NETLINK, ..., NETLINK_KOBJECT_UEVENT)` = EAFNOSUPPORT, after
     which SDL undoes video too. gex-kernel now gives an inert netlink socket
     (an empty eventfd). Unity then logs "The selected window backend is x11",
     `Display.main` is 800x600, and the cameras render (hk69: 18,518
     pre-renders, glDrawElementsBaseVertex with textures every frame). Frames
     still look black because, with GEXMONO_LV_OFF=1 (needed to avoid the crash
     below), the spinner is gone and the intro stays at LoadingIcon, where
     every element is faded out (vertex alpha 0 in the trace).
  12. **The Instantiate crash (liveness on), what is known:** it is not the
     function JIT (hk68, tier-1 only, crashes too). It is not the GC lock. It
     moves: Internal_CloneSingle (hk60/hk61), then after FsmState creation and
     its method scan (hk70), then during field deserialization (hk71), or at
     the first frame (hk72): memory corruption, not one bad read. With
     `MONO_GC_DEBUG=disable-major` (hk73) it fails differently: "Unable to find
     method BeginRenderExtraOverlays in [UnityEngine.UIModule.dll]
     UnityEngine.Canvas", then an assertion in class.c:2272 (`type` NULL).
     The serializer creates PlayMaker objects (Fsm, FsmState, FsmString x4096,
     List<T> x8192, TMP_Glyph x8192: GEXMONO_TRACE_METHODS=1 lists classes it
     scans, =2 every method with its signature) and stores managed references
     into its own native buffers with mono_gc_wbarrier_set_field(NULL, ...).
     Leads: references held only in Unity's native heap (invisible to sgen),
     and Unity writing managed objects through Unity-Mono assumptions. Next:
     MONO_GC_DEBUG=verify-before-collections / check-at-minor style heap
     checks, and the class.c:2272 caller.
  13. **Gate head 55c3197f (2026-10-06):** liveness walk opt-in
     (`GEXMONO_LIVENESS=1` then; on by default since item 18), netlink socket, F's notes,
     HOME_SEED/OVERLAY/xfocus/probe tooling. Legs on it with `scratch/unity/dnc`
     rebuilt from it: embed PASS, prov64 PASS, fjit-procs ok/ok, Muck batch
     --fjit reaches "Initializing packets", HK batch reaches "Unloading 153" and
     its first scene's scripts.
     **The crash with liveness on, more:** tier-1 block names (GEX_PERFMAP=1 +
     GEX_WASM_BACKTRACE=1) put the fault in `UnityEngine.Object::GetName_Injected`
     -> UnityPlayer 0x14ed6e0 (`Object::GetName`): `[this+0x20]` is taken as an
     object with a vtable. `this` (0x1e009f510) has the vtable of
     `13MonoBehaviour` (typeinfo via the R_X86_64_RELATIVE at vtable-8); +0x20 of
     a Component is its GameObject pointer, and here it holds 0x1103ed2e0, an
     address in the provider's/Mono's region, while other objects of the class
     have 0 there and their name inline at +0x38 (GEX_TRACE_SEGV=2 dumps the
     words at each register). Next: what 0x1103ed2e0 is (MonoDomain?) and who
     writes it into a native MonoBehaviour during Instantiate (watchpoint on
     that word: GEX_WATCH).
  14. **The dangling pointer is a destroyed GameObject (runs hk79-hk84):**
     0x1103ed2e0 is not a Mono address: `GEX_WATCH=0x1103ed2e0+0x40` shows a
     `10GameObject` (instance ID 48962) created there by the loading thread
     and later destroyed, and the memory reused for path strings and vertex
     data. The MonoBehaviour (ID 48966, the loading spinner's
     StandaloneLoadingSpinner) keeps pointing at it. `GEXMONO_LV_WATCH`
     (provider: words at given addresses at each liveness world stop/start)
     shows the GameObject alive through the liveness pass and destroyed by
     the "Unloading 82 unused Assets" that follows: the walk reports the
     component (reached from StartManager.loadSpinnerPrefab), Unity keeps it,
     but not its GameObject. Unity's mark function (UnityPlayer 0x12f1ef0,
     called per reported object by instance ID) sets the mark bit and queues
     the object for dependency traversal only when flag 0x8 is clear, so a
     reported asset may not pull its GameObject in by itself.
     Changes made to match Unity's own unity-liveness.c (fetched from
     Unity-Technologies/mono unity-main): from_root walks the root's references
     without reporting the root; statics come from the ambient memory
     manager's class_vtable_array (generic instances included, corlib
     skipped). Neither changed the outcome (still 82 unloaded, same crash).
     Next: what flag 0x8 means and how real Unity keeps a component's
     GameObject (does the managed walk reach the GameObject's wrapper there,
     e.g. through a GC handle root?); compare the reported set against the
     handle roots.
  15. **Reported set vs roots (hk85-hk88, GEXMONO_LV_TRACE=1: each root and
     each reported object as class + Unity instance ID, read from m_CachedPtr
     at +0x10 without metadata calls, since the walk runs on Unity job threads
     Mono does not know):** Unity passes 126 roots, all scene UI components
     (Image, EventTrigger, MenuButton, Text, ...). The walk reports 223 objects,
     among them `StandaloneLoadingSpinner` id 48966 (the spinner component);
     its GameObject 48962 is not reported (no managed reference reaches a
     GameObject wrapper), which is what Unity's own walk would do too. Per-state
     visited sets (GEXMONO_LV_PERSTATE=1) change nothing (82 unloaded, same
     crash). The reference run on the game's own libmonobdwgc (hkst4, tier-1,
     batch) stalls after physics init for 2 h, as hkst3 did: no comparison.
     **Leading hypothesis now:** Unity keeps a prefab referenced from a scene
     script through its native dependency walk over the script's serialized
     data (StartManager's `loadSpinnerPrefab` field), not through the managed
     liveness walk. Without liveness the whole prefab is unloaded (the
     original ArgumentNullException), so that native walk does not see the
     field under the provider; with liveness only the component is marked and
     Unity's mark function (0x12f1ef0) does not queue it for dependencies when
     flag 0x8 is set. Next: watch whether Unity's GC reads StartManager's
     managed fields (GEX_WATCH on the field inside the StartManager wrapper)
     during MarkObjects, and what its serialization command list for
     StartManager contains (the field's offset as Unity cached it).
  16. **The serialized-field hypothesis is dead; the question is Unity's flag
     0x2 (2026-10-06):** Unity's GC does not read a MonoBehaviour's managed
     fields itself. Its per-object dependency step (UnityPlayer 0x12f3130,
     a switch on the object's type) does, for a MonoBehaviour (case at
     0x12f33b0): if the object's mapping entry has flag 0x2, mark its
     GameObject (`[this+0x20]`, by instance ID); mark its script; then call
     `mono_unity_liveness_calculation_from_root` (slot 0x263c8d0) on the
     MonoBehaviour's managed object. So references held in script data are
     found only by the provider's walk, which does report the spinner
     component; and a component pulls its GameObject in only when flag 0x2 is
     set on its entry, which for the spinner it is not. Where flag 0x2 comes
     from is not found yet (the entries are not written by an `or $0x2`; the
     candidate is the object's own bits word at +8, high half 0x0850002c for
     this component, 0x0f70002c for its GameObject).
     Also killed: GC-handle encoding. Unity's Mono gives a strong handle as a
     pointer to a slot (bit 0 clear) and a weak one with bit 0 set
     (Unity-Technologies/mono boehm-gc.c `handle_tag_weak`; UnityPlayer reads
     the slot directly when bit 0 is clear), while the runtime's raw strong
     handles end in 3. A build with Unity-style slots for the handles Unity
     asks for (runtime patch to sgen-mono.c's free/get/set to accept slots,
     provider giving slots; tree `scratch/unity/dnc3`, not committed) runs
     Hollow Knight the same: 153 unloaded without the walk, 82 and the same
     crash with it.
  17. **Workaround: HK reaches Menu_Title (6b119e15, 2026-10-06):** Unity's
     dependency step gates both "component -> its GameObject" (MonoBehaviour
     case) and "GameObject -> its components" (case at 0x12f31f2: the
     {type, component} array at [go+0x20], count [go+0x30]) on flag 0x2, so
     keeping only the GameObject moved the crash to a component of it (a
     virtual call through a freed component, run hk95-hk97). The walk now
     reports, for each reported UnityEngine.Component, its GameObject and that
     GameObject's components, through stand-ins Unity's callback reads like
     wrappers (it reads only m_CachedPtr at +0x10 and the native instance ID
     at +8). With `GEXMONO_LIVENESS=1` (walk opt-in; `GEXMONO_LV_NOGO=1` drops
     the workaround) HK batch (hk98): 69 assets unloaded at the intro, the
     spinner instantiated, "Didn't need to wait for PlayerPrefs load",
     Menu_Title becomes the active scene and loads (10,779 assets unloaded
     after it, 36,129 objects). Next problem seen: HK's shared-data save uses
     BinaryFormatter, which .NET 10 removed (PlatformNotSupportedException in
     JsonSharedData.Save); saves will need the formatter (the unsupported
     System.Runtime.Serialization.Formatters package, or a shim). Flag 0x2
     itself is still unexplained.
     **With graphics (hk99, gooner llvmpipe, netlink fix + xfocus + seed +
     walk):** the main menu renders (`scratch/unity/screens/hk-menu.png`:
     title, particles, menu arrows); the menu item labels draw as black
     boxes (text: TextMeshPro font warnings earlier in the log; next to look
     at), cameras 5, ~50k renders.
  18. **Root cause of 12-17 and of the black labels: the walk reported too
     late (2026-10-06, runs hk102-hk107).** Unity's GC driver (UnityPlayer
     0x130d26b on) runs, per GC: the mark jobs, each draining its state's
     queue (0x12f2e60; the queue is what the mark function 0x12f1ef0 pushes
     an object's entry onto, unless the entry has flag 0x8), with
     `from_root` called from the MonoBehaviour case and `from_statics` on
     the main state; then, after every queue is drained,
     `mono_unity_liveness_finalize` on each state (0x12f30a0); then
     `free_struct`. The provider reported objects to Unity's callback only
     when its buffer filled or at finalize, so most objects found by the walk
     were marked after the last drain: marked, kept, but none of their native
     dependencies followed. Unity's Mono reports at the end of each
     `from_root`/`from_statics` call (unity-liveness.c), which the provider
     now does too (`lv_flush_now`; `GEXMONO_LV_LATE=1` restores the old
     behaviour for comparison).
     This explains all of it: the spinner component kept without its
     GameObject (items 14-17: its entry was never dequeued, so the
     dependency step and its flag-0x2 test never ran for it; flag 0x2 is set
     on nearly every entry, it was a red herring), and the black labels: a
     dynamic Font kept by script data, its atlas texture (the Font's PPtrs at
     native +0x30 material, +0x34 texture) unloaded and reloaded from disk as
     the serialized 0x0 texture while the Font's glyph cache stayed, so glyph
     updates went to a GL texture with no storage (the GL capture: texture
     459 generated then only `TexSubImage2D`'d, `GetNativeTexturePtr` 458 on a
     0x0 "Font Texture"; GexProbe `GEXPROBE_FONTS=1` shows each font's atlas).
     Not a liveness victim of TextMeshPro (its font assets were alive, hk100)
     nor a GL-path problem.
     With the fix and **no workaround** (the stand-in GameObject reporting of
     item 17 is removed) Hollow Knight passes the spinner Instantiate, reaches
     Menu_Title with graphics, and the menu labels render
     (`scratch/unity/screens/hk-menu-labels.png`, hk107). The walk is now on by
     default (`GEXMONO_LIVENESS=0` turns it off).
     How found: a diagnostic that read Unity's GC table from the callback's
     userdata (Unity 6000.0 linux-x64: `[data+8]` = table, entries of 24
     bytes {object, RTTI, flags} at table+0, buckets of 12 bytes {hash,
     instance ID, entry index} at table+0x48, mask x4 at table+0x50; RTTI
     +16 = class name) showed the marked Font with its texture and material
     unmarked; reading the driver showed when finalize runs.
     **BinaryFormatter:** `build.sh`'s `formatters` step (63c7f5da) restores
     the out-of-band System.Runtime.Serialization.Formatters NuGet package
     (10.0.*, its net8.0+ assembly) into `OUT/lib-extra`, which
     `muck-dotnet.sh` copies over the framework's stub; HK's
     `JsonSharedData.Save` no longer throws (hk102).
  19. **Scripted input to New Game (2026-10-06, runs hk109-hk118):** in
     progress. Found and fixed so far:
     - gex's compositor gave the keyboard to whichever surface last drew,
       and Xwayland's cursor surface draws after its window, so every key
       went to the cursor (`[gex] input: ... focus Some(12)` with 12 the
       Cursor). Only toplevels and popups take it now (88b99006, with a
       test).
     - `tests/games/xfocus.c` focuses the top-level under the pointer (else
       the largest) and with `XFOCUS_TREE=1` dumps the window tree with
       event masks when it changes: Unity 6 maps its SDL window 0x800004
       ('Hollow Knight', core masks incl. KeyPress) inside an
       override-redirect 800x600 parent first, then reparents it to the
       root; the focus ends on 0x800004 and Application.isFocused is true.
     - Keys do reach X clients: `tests/x11/xinput-echo.c` gets KeyPress 36
       for a scripted Enter, and a plain SDL2 2.26 program (dlsym'd libX11,
       same in-guest Xwayland) gets SDL_KEYDOWN scancode 40 / sym 13.
     Still open: Hollow Knight's legacy Input sees pointer motion (GexProbe
     `GEXPROBE_INPUT=1`: mouse 400,257 after the scripted move) but no key
     and no button. Without xfocus (NOFOCUS=1) it is not focused at all.
     An LD_PRELOAD spy on the Xlib functions Unity's SDL dlsym's
     (`scratch/unity/xspy/xspy.c`; Unity sends its stderr to the player log)
     shows the events do reach Unity: every scripted key arrives as a
     KeyPress through XFilterEvent (-> 0, not swallowed) and
     Xutf8LookupString (Return, z, Left, Right), Unity selects XI2
     RawButtonPress/RawButtonRelease/RawMotion (mask 0x38000) on the root
     for all master devices, and a scripted click arrives as XI2 raw
     button 1 press and release (hk120). Yet GexProbe's legacy Input reports
     no key and no button and the menu does not react; only the pointer
     position (likely polled) works. So the loss is inside Unity, between
     its X event handling and its input queue. One candidate: event
     timestamps (Unity's input queue orders events by time; position by
     polling would not care). Next: the X event `time` values against the
     guest's CLOCK_MONOTONIC as Unity reads it.
  19b. **The dropped input was the monotonic clock (02b4f7f7, hk123-hk124):**
     gex answered CLOCK_MONOTONIC with the wall clock (~1.8e12 ms). Xwayland
     stamps events with `(CARD32)` monotonic milliseconds (the spy: event time
     338241355 against Unity's monotonic 1791339769010), and Unity's input
     layer compares event times with its own monotonic clock, so every key
     and button was dropped while polled pointer positions worked. The
     monotonic clocks now count from one boot instant per host process (the
     first reading less 1000 s), and the absolute waits on them follow
     (clock_nanosleep TIMER_ABSTIME, monotonic timerfd, FUTEX_LOCK_PI2,
     FUTEX_WAIT_BITSET without FUTEX_CLOCK_REALTIME, which had been read as a
     relative length capped at an hour). With it (hk124) the scripted Enter
     opens Select Profile, the next one starts a new game and Hollow Knight
     shows its first-run screen-scale page. The now-real futex deadlines
     exposed a host race (check the deadline, then subtract a later
     reading: "overflow when subtracting durations"), fixed in 4a4510fb.
     GexProbe's legacy-Input log still shows no keys (Hollow Knight reads
     input through InControl; the probe's view is not evidence either way).
     Also checked: Unity's SDL keymap at init is complete (XDisplayKeycodes
     8..255, XkbKeycodeToKeysym 9 -> Escape...), its window gets a normal
     FocusIn, and it selects XI2 raw button/motion on the root and XI2
     motion/touch on its window.
  20. **Head c3b6288a (master 132f73ed merged, 2026-10-06):** gexc rebuilt
     (`target/dbg-x86`); `scratch/unity/dnc` rebuilt from e61b5a4b (provider
     unchanged since; `lib-extra` holds the Formatters assembly). Legs, walk
     on by default: embed PASS; Muck batch --fjit "Initializing packets"
     (mk10); Hollow Knight with graphics --fjit, no probe: the main menu with
     its labels (hk122 f-300); gex-wayland tests 36/36.
     **Muck with the walk on by default:** batch, current gexc, --fjit:
     "Initializing packets" (mk8); without --fjit too (mk4). (mk1-mk3, mk6
     hung at ReloadAssembly: they ran the script's default
     `target/x86/release/gexc`, an Oct 5 build from before the fjit
     per-process fix, not a regression.) HK reaches Menu_Title with the
     walk on by default in every run since hk109.

  21. **Speed on a bench box (2026-10-07, restarted stream U; head after
     ba7d2dc7 = master 5a066f67 merged).** The user's concern: Hollow Knight's
     start-up (~4.5 min to the first frame, ~12.7 min to the menu) and 5-15 fps,
     all from gooner (loaded, llvmpipe). No game runs on gooner any more: games
     run on edger (`scratch/bench/bench.sh`).
     - **Harness:** `tests/games/unity-bench.sh hk|muck RUN SECS [gexc flags]`
       wraps muck-dotnet.sh with graphics and writes `meta.txt`, `plog-t.txt`
       (the player log with ms since start), out.txt with `[gex-gl] first
       present` and `[gex-gl] presents/s` (`GEX_GL_FPS=1`, gex-gl host: the
       game's own swaps, not compositor commits) and with `PROF=1` the
       `[gex-prof] AOT x% of guest` line every 10 s (`GEX_PROF_SHARE=1`; new
       kind `provider-jit` for the .NET wasm JIT's modules) and prof.txt.
       `tests/games/unity-bench-sum.py RUN` prints the milestones and the menu
       fps window. Run as `scratch/bench/bench.sh -s
       /home/claude/gex-sandbox/wt/unity/tests/games -- env PROF=1 bash
       wt/unity/tests/games/unity-bench.sh hk scratch/unity/eruns/NAME 1200
       --fjit`.
     - **edger staging** (same paths as gooner): `games-linux/{Hollow
       Knight,Muck}`, `scratch/unity/dnc/{provider/mono.wasm,lib,lib-extra}`,
       `scratch/unity/gl64`, `~/.dotnet/shared/Microsoft.NETCore.App/10.0.10`,
       `scratch/unity/edger/gexc` (copy of `target/dbg-x86/release/gexc`),
       `scratch/unity/edger/bin/Xwayland` (gooner's 22.1; edger has none; passed
       as `--env XWAYLAND=`), `scratch/unity/edger/llvm-bin/{clang,wasm-ld}`
       wrapping gooner's LLVM 22 copied to edger `~/toolchains/llvm-22` (+
       `edger/llvm-libs`): gexc links provider modules with wasm-ld at run
       time (`GEX_CLANG`/`GEX_WASM_LD`, set by unity-bench.sh).
     - **edger fixes:** Xwayland aborted in NVIDIA's EGL (its swrast GLX opens
       EGL through glvnd): xwayland-inguest.sh sets `XWAYLAND_NO_GLAMOR=1` and
       `__EGL_VENDOR_LIBRARY_FILENAMES` to Mesa's.
     - **Start-up is translation:** the first edger profile (first 2 min) is
       Cranelift + gex_xlate recovery on the thread that missed (the fjit
       translates synchronously), ~1-1.5 cores busy of 8.
     - **ELF units (new, this commit):** `GEX_ELF_UNITS=UnityPlayer.so[,..|all]`
       installs a function-tier unit for a Linux shared object at its mapped
       bias (`nt_cgen::on_elf_maps`, after `elfprov` in the Linux map hook;
       `CodeMap` now carries the file offset); a missing one is built in the
       background for the next run (`GEX_ELF_UNITS_BUILD=0` stops that;
       `gexc cgen-image --elf SO --base HEX --mem64 --jobs N` builds one by
       hand). The object must land at the same base every run: `--pin FILE`
       with a line `BASE SPAN UnityPlayer.so`.
     - **Liveness walk (provider, this commit):** class shapes (reference
       offsets per class, value types flattened, filter test cached) and a
       lock-free generation-tagged visited set replace the per-object field
       iteration and the mutex-guarded hash; `GEXMONO_LV_NOSHAPE=1` walks as
       before for A/B.
     - **First edger numbers (native, `--fjit`, 800x600, cores
       2,4,6,8,14,16,18,20, PROF=1; UnityPlayer and every guest library on
       the function JIT, the .NET side on the interpreter, no wasm JIT; runs
       `scratch/unity/eruns/*`, summaries in `*.out`):**

       | run | what | first present | menu ready | menu unload MarkObjects | menu fps (20-80 s after) |
       |---|---|---|---|---|---|
       | hk-e1 | cold | 472.5 s | 539.5 s | 17.8 s | 24.5 |
       | hk-e2 | cold, `GEX_FJIT_CACHE=1` writing | 449.7 s | 518.4 s | 19.2 s | 23.5 |
       | hk-e3 | warm `GEX_FJIT_CACHE=1` | 243.2 s | 301.4 s | 18.7 s | 24.3 |
       | hk-e4 | warm, shape walk provider (`dnc-shape`) | 249.7 s | 294.2 s | 0.75 s | 24.0 |
       | mk-e1 | Muck cold, cache on | 230.8 s | packets 316.4 s | - | 33.6 (last quarter 30.8) |

       "Menu ready" is the post-Menu_Title unload. The player log's first line
       comes 262 s (cold) / 117 s (warm) after the start: ld.so and
       UnityPlayer's init are translated before Unity logs anything. In the
       menu about half the CPU samples are the host (gex-gl, the NVIDIA
       driver, the readback) and the guest half is almost all the fjit
       (`[gex-prof]` lines). Same unload counts with the shape walk (49 and 66
       assets), so it changes cost only: Menu_Title start to menu ready went
       from 37.0 s (hk-e3) to 20.6 s (hk-e4).
     - **Frames are wrong on edger's NVIDIA driver:** every frame HK presents
       is black (GL-only PPMs, `GEX_PPM_GL_ONLY=1`), and Muck shows its
       blurred sky with no UI, while the same build rendered both on llvmpipe
       (gooner) and RX 6500 XT (worldmachine). gex-gl's own host tests pass on
       edger. `GEX_GL_ERRORS=1` (gex-gl host: glGetError after every command,
       per opcode) is the next look. The fps numbers above are for that
       rendering.
     - **Fixed (273b4635):** `GEX_GL_ERRORS=1` showed BIND_FRAMEBUFFER (and
       FRAMEBUFFER_TEXTURE_2D) failing with INVALID_OPERATION every frame:
       the host ran the guest's framebuffer names as they were, and
       ARB_framebuffer_object (NVIDIA, unlike Mesa) refuses a name
       GenFramebuffers did not return. Framebuffer and renderbuffer names are
       now the host's (`gexgl_native_object`, binding queries mapped back).
       hk-g2 (cold, new gexc, shape provider): no GL errors, the main menu
       renders (`scratch/unity/screens/hk-menu-edger.png`), first present
       448.3 s, menu ready 499.4 s, menu 24.4 presents/s. (The e1-e4/mk-e1
       fps above were measured with the broken rendering.)
     - **ELF unit for UnityPlayer.so:** built on gooner at base 0x7e041000
       (`GEX_CGEN_CACHE=scratch/unity/units/cgen`, `gexc cgen-image --elf
       ... --mem64 --jobs 12`, log `scratch/unity/units/build-up*.log`):
       recovery 55,439 functions in 5,424 s (single-threaded, ~20 GB), then
       the C compile; recovery and objects are cached, so a killed build
       resumes at the compile. Pin for runs: `scratch/unity/edger/hk-pin.txt`
       (`0x7e041000 0x2674000 UnityPlayer.so`); copy the cgen cache to edger
       and run with `GEX_CGEN_CACHE=... GEX_ELF_UNITS=UnityPlayer.so
       GEX_ELF_UNITS_BUILD=0` and gexc flag `--pin FILE`.
     - After a Claude Code restart: background runs and builds die with it;
       check `ps` on gooner and edger, rerun what was cut.
     - **Baseline with correct rendering (edger, native, `--fjit`, shape
       provider):**

       | run | state | first present | menu ready | menu fps (20-80 s after) |
       |---|---|---|---|---|
       | hk-g2 | cold, readback every frame | 448.3 s | 499.4 s | 24.4 |
       | hk-p1 | warm fjit cache, readback every frame | 244.6 s | 289.1 s | 24.4 |
       | hk-r1 | warm, readback every 1800th (`GEX_GL_READBACK_EVERY`) | 239.6 s | 282.4 s | 26.4 (last quarter 25.8) |

       Tiers in the menu (hk-p1 `[gex-prof]`): guest code is 99.5% fjit
       (UnityPlayer and libraries), providers (the .NET runtime and
       interpreter) 0.2-0.5%, no units, no .NET JIT; about half of all
       samples are host. Steady-state profile (hk-p1, `GEX_PROF_START=340`):
       the render thread (guest tid 68) spends 38% of its samples in the
       host's sched_yield inside NVIDIA's driver (the per-frame readback's
       GPU wait, hence `GEX_GL_READBACK_EVERY`) and ~15% in the driver.
       Per-thread CPU over 20 s of the menu (hk-r1 `threads.txt`): main
       thread 69% of a core, render thread 25%, tid 69 25%, the job workers
       2-10% each; 2.15 cores in all. No thread is saturated, so the menu
       frame is bound by waits between threads (the main thread waiting on
       the render thread or job fences), not by one thread's code speed.
     - **Muck with the NVIDIA fix (mk-g1, cold, readback every 1800th):** the
       menu renders fully (`scratch/unity/screens/muck-menu-edger.png`),
       first present 228.2 s, "Initializing packets" 314.2 s, menu 42.7
       presents/s (last quarter 44.4). Threads: main 73% of a core, one
       worker (tid 44) 49%, 2.06 cores in all.
     - **The main thread's top cost was mono_gc_get_used_size:** the
       steady-state profile of hk-p1's main thread (perf-map names) put
       49.9% of its samples in SGen's `major_get_used_size`, which walks every
       object of the major heap with a full sweep; Unity asks every frame and
       under gex every object is in the major heap. The provider now answers
       from a cache refreshed after a collection or every 500 ms
       (`GEXMONO_USED_SIZE_EXACT=1` for the old behaviour; b0cf74ae). Note:
       the `[gex-prof]` share line classified those samples as fjit ("jit"),
       so its provider share is not trustworthy; use the per-thread
       profiles with names.
     - **Gate legs added:** gex-gl test
       `a_desktop_context_takes_framebuffer_names_it_never_generated` (fails
       before 273b4635 on edger's NVIDIA, passes after; passes on gooner
       either way); embed's liveness check (301 leaves, each once, batched;
       also under `GEXMONO_LV_NOSHAPE=1`).
     - **Pitfall:** a wait loop like `while kill -0 $(pgrep -f "PATTERN")`
       matches its own command line; wait on a known PID.
     - **Pin lesson:** UnityPlayer.so (span 0x2674000) cannot be pinned at
       0x7e041000 on edger: the range would cross 0x80000000 and the pin was
       never taken (`[pin] ... never reserved`); unpinned it lands at
       0x7d98a000 there (hk-a1). The unit is being rebuilt for 0x7d98a000
       (`build-up4.log`; recovery is keyed by the placed image, so a new base
       is a full rebuild, ~2.5 h on gooner) and the pin file now says
       `0x7d98a000 0x2674000 UnityPlayer.so`.
     - **mono_gc_get_used_size cache, measured:** hk-u1 (warm, readback
       every 1800th) 28.4 presents/s against hk-r1's 26.4 (same gexc, same
       state): 7% faster. Main thread 59.5% of a core (was 69%). Main-thread
       profile after (hk-p2): `major_get_used_size` 5.6% (the 500 ms
       refresh), `mono_interp_exec_method` 12.1% (managed code on the
       interpreter), `wasm[0]::function[10]` 14.6%.
     - **The .NET wasm JIT on Hollow Knight (hk-j1, `--env
       GEXMONO_WASM_JIT=1`):** the host faults at 267 s, after the first
       scene ("gexc: guest memory access out of bounds (SIGSEGV)
       (pc≈0x0)"), before the menu. Not looked at yet; the interpreter
       stays the default.
     - **UnityPlayer.so unit, measured (2026-10-07):** built for 0x7d98a000
       (key 0b00cd4b..., 55,438 entries, 165 MiB wasm; recovery 1,461 s on an
       idle gooner, whole build 4,791 s). It lands there unpinned on edger
       (the pin still reports "never reserved"; the unit is placed by the
       natural base). hk-a3 is its first run (wasmtime compiles it: placed
       in 83.2 s), hk-a4 the warm one (1.8 s). Paired with hk-u1 (warm fjit
       cache, same provider and gexc, no unit):

       | run | first present | menu ready | menu fps |
       |---|---|---|---|
       | hk-u1, no unit | 236.7 s | 280.4 s | 28.4 |
       | hk-a4, unit | 197.6 s | 227.7 s | 29.7 (last quarter 30.7) |

       So the unit brings the menu 52.7 s sooner (19%) and the menu 5% faster;
       the frame is not bound by UnityPlayer's code speed. The first log line
       still comes at ~106 s.
  22. **The .NET wasm JIT is the default (user, 2026-10-07).** Normal runs use
     the fork's wasm JIT; ejected builds will use .NET AOT. `GEXMONO_WASM_JIT=0`
     runs the interpreter only (d4a9389d; embed keeps an interpreter-only leg;
     `unity-edger-gate.sh` runs the game legs with the JIT, `JIT=0` for the
     interpreter).
     - **The Hollow Knight fault under the JIT (hk-j1/j2) was a sign
       extension:** the JIT'd `SpanHelpers.ClearWithReferences` (from
       `Array.Clear`) computed `Unsafe.Add(ref p, -1)` with the `-1` an
       OP_ICONST in an i32 local, widened with `i64.extend_i32_u`: 0xffffffff
       pointers, 32 GiB past the span. On a 64-bit mono target a 32-bit
       constant's register holds the sign-extended 64-bit value
       (`inst_c0`; mini-amd64 moves it as one) and mono folds `conv.i` of a
       constant away relying on that. A negative OP_ICONST now gets an i64
       local and `i64.const` (patch 0004, a1b2f22f). Repro and test: embed's
       `Clears(300)` (Array.Clear/Span.Clear/List.Clear over reference, value
       and mixed arrays) gives "clears 643296" on the interpreter and the
       JIT; before the fix the JIT leg died there.
     - **Hollow Knight with the JIT (hk-j3, warm fjit cache, no unit, same
       gexc and provider as hk-u1 but the JIT fix):** reaches the menu and
       renders it; first present 250.5 s (hk-u1, interpreter: 236.7 s), menu
       ready 300.8 s (hk-u1: 280.4 s), menu 28.8 presents/s, last quarter 30.2
       (hk-u1: 28.4 and 28.6). The JIT's compiles cost ~20 s of start-up;
       the menu is not managed-bound (managed code was 12% of the main
       thread on the interpreter).
  23. **Plan: .NET AOT for ejected builds (not started; estimate 10-15 agent
     days).** The browser's own path is mono's LLVM AOT in llvm-only mode
     (`mono-aot-cross` with Mono's LLVM, bitcode linked by emcc, the
     interpreter as fallback for what AOT cannot see). For gex the pieces:
     1. **A wasm64 cross compiler** (3-4 days): build `mono-aot-cross` from
        the fork for host linux-x64 targeting our `wasm64` processor
        (`TARGET_WASM`+`TARGET_WASM64`, 8-byte pointers), with the
        cross offsets header for the target (offsets-tool against the gex
        wasm64 sysroot and `HOST_GEX` config) and Mono's LLVM (the
        Microsoft.NETCore.Runtime.Mono.LLVM.Sdk package) emitting
        `wasm64-unknown-unknown` bitcode; mini-llvm's TARGET_WASM paths
        checked for 32-bit pointer assumptions (`mtriple`, intptr types,
        the llvm-only EH model).
     2. **Linking into the provider** (2-3 days): compile the bitcode with
        clang 22 `--target=wasm64` and our musl, link it into `mono.wasm`
        (one stateful provider), register the AOT images at start-up (the
        browser driver's `mono_aot_register_module` table), and run
        `MONO_AOT_MODE_LLVMONLY_INTERP` so anything not AOT'd falls back to
        the interpreter (generic instances AOT could not see, dynamic
        methods).
     3. **gex calling conventions** (2-4 days): AOT'd managed-to-native
        wrappers must call guest addresses through the guest native call
        (`ves_pinvoke_method`'s marshalling, icalls registered by UnityPlayer
        at run time) rather than as wasm function pointers; native-to-managed
        (reverse) thunks into AOT code; exceptions through guest frames
        (`gexp_raise`) meeting llvm-only EH.
     4. **Coverage and the session** (2-3 days): AOT CoreLib, the facades,
        `UnityEngine.*` and the game's `Managed/*.dll` (the set recorded by
        the session), with a coverage report of methods left to the
        interpreter; measure Muck and Hollow Knight against the JIT.
     Risks: Mono's LLVM wasm64 output (untested upstream), code size (tens of
     MB for Hollow Knight), and AOT of the Unity-patched BCL behaviour we
     depend on (GC handles, liveness) being in C, so unaffected.
  24. **Muck with and without the JIT (edger, warm fjit cache, provider
     `dnc-jit` = d4a9389d's runtime with patch 0004, same gexc, no unit;
     runs mk-i1 `GEXMONO_WASM_JIT=0`, mk-j1 `=1`):**

     | run | first present | "Initializing packets" | menu fps (20-80 s after) | last quarter |
     |---|---|---|---|---|
     | mk-i1, interpreter | 151.6 s | 224.1 s | 43.0 | 43.5 |
     | mk-j1, wasm JIT | 154.6 s | 231.1 s | 38.6 | 42.6 |

     Both render the full menu, no faults. The JIT's window starts while it
     is still compiling (per-second minimum 7.9), and settles 2% below the
     interpreter: the menu is not managed-bound, and the JIT's compiles cost
     7 s of start-up. Hollow Knight (item 22): 250.5 s vs 236.7 s first
     present, 300.8 s vs 280.4 s menu, 30.2 vs 28.6 presents/s in the last
     quarter. A managed-heavy scene (gameplay) is where the JIT has to pay;
     the menus cannot show it.
     **Next:** the JIT's start-up cost (compile threshold, caching compiled
     JIT modules across runs like `GEX_FJIT_CACHE`), the main/render thread
     waits, then .NET AOT (item 23).
  25. **JIT module cache and pins (2026-10-07, gexc 691acaa9):**
     - Compiled .NET JIT modules are now kept on disk by their bytes
       (`~/.cache/gex/modules/wasmjit`, `GEX_WASMJIT_CACHE=0` off). Hollow
       Knight's start-up to the menu compiles 2,331 (46 MB) and a second run
       finds all of them. Measured, warm fjit cache, JIT on: hk-c2 (cache)
       first present 255.1 s, menu 312.5 s, 29.1 presents/s; hk-c3 (cache
       off) 255.2 s, 306.2 s, 29.5. No difference: the compiles are not what
       makes the JIT start-up slower than the interpreter's (hk-j3 250.5 s vs
       hk-u1 236.7 s); the hotness counting and tier-up are the next suspect.
       The cache stays (harmless, 46 MB).
     - **`--pin` works when the range is valid:** a pin at 0x7a000000 placed
       UnityPlayer.so there (hk-pt: "no function-tier unit ... at 0x7a000000",
       as expected with no unit for that base). The first pin (0x7e041000)
       failed because the object's span would cross 0x80000000, the top of
       the region shared objects are placed in; the kernel then placed it
       elsewhere without a word until the process exited. A pin that lands
       elsewhere is now said when it happens (`[pin] PATH WANTED X GOT Y`).
       Unit builds should pick a base with base+span below 0x80000000.
  26. **Thread waits, first look (hk-s1, `GEXGL_SUBMIT_TRACE=1`):** the GL
     transport is not it. Hollow Knight's menu makes about one submit per
     frame (the first 5,000 submits came over ~5,600 presents; 81% of them
     follow PRESENT, the rest are glGet*-style replies), so the render thread
     does not wait on GL round trips. Main thread 60% of a core plus render
     thread 30% at 29 presents/s is a frame of ~34 ms of which the two
     threads' work adds up to ~31 ms: Unity runs them close to serially
     (the main thread waits for the previous frame's render before the next).
     Faster code on either thread is what moves the frame; on the native
     host that is the units (5%) and the provider. Since the browser is the
     target and its costs differ (V8 instead of Cranelift, the page's GL),
     the next frame-time work belongs in the page (P5).
  27. **P5 plan: Unity in the page (2026-10-07; estimate 11-14 agent days).**
     The browser is the only performance target. Native edger numbers stay
     next to each page number (same game state, same provider). What exists:
     the wasm64 browser shim and kernel (`GEX_MEM64=1 shims/browser/build.sh`,
     node runs of small programs), the page's Linux game path for wasm32
     (`tests/games/browser-linux.mjs`: hostx Xwayland as wasm, Mesa's gex_wgpu
     over WebGPU), stateful NT providers in the page (wasm32), ELF export
     providers in the page, the function JIT in the page. Steps:
     - **B0, a 64-bit memory in the page (1-2 days):** `mem64=1` in the page
       (wasm64 shim, seed and kernel; the glue's BigInt paths), checked with
       `tests/x86-programs/mem64-io` and a dynamic glibc program, then a GL
       game through browser-linux.mjs under mem64.
     - **B1, stateful Linux providers in the page (3-4 days):** the page's
       `on_linux_maps` (a replaced `.so` maps: its exports bound to the
       provider, its constructors to `gexp__noop`), the wasm64 `provider_*`
       imports in gex.mjs (try/throw on a per-store wasm EH tag, ctx, alloc,
       log, stack, abi, table_size, guest_sym, thread register/park/stop/start
       on shared-memory words, guest_stack_end, code page and thread entry,
       reverse slots), musl's per-thread attach in each Worker. Test:
       `tests/unity-dotnet/embed` in the page, interpreter.
     - **B2, the .NET wasm JIT in the page (2 days):** the `wasmjit` host
       imports in the page: slots reserved in the function table, each module
       compiled once and instantiated in every Worker through the table log
       (as the function JIT's are), its throw tag. Test: embed with the JIT at
       threshold 1.
     - **B3, Muck in the page (2-3 days):** the game, the BCL and the
       provider served to the page, Xwayland and Mesa as for the Linux games;
       time to first frame and menu fps on edger's Chromium, against native.
     - **B4, ELF units in the page (2 days):** the page's cgen hook for a
       Linux map (`GEX_ELF_UNITS`), units from the unit cache by the same key
       (built natively, `c.wasm` for the page); Muck's UnityPlayer unit.
     - **B5, Hollow Knight in the page:** its 4.9 GB of data served lazily.
     Shared shim code (gex.mjs, cgen.rs, the kernel) goes to the coordinator
     as gate heads early, step by step.
  28. **P5 progress (2026-10-07):**
     - **B0 done (d442cc57, 1e4ae9d7):** `?mem64=1` in the page (wasm64 shim,
       seed and kernel; the host X server keeps the wasm32 kernel build);
       `MAP_32BIT` in the kernel and the guest GL ring mapped with it (the
       page's `gl_submit` takes 32-bit addresses); the function JIT in a
       64-bit process (table64, i64 PIC globals; no std::time on wasm64);
       `?fjit=1`. `tests/games/src/glx-context` passes in Chromium with
       `?mem64=1&fjit=1`.
     - **B1 done (182e7cfb, 171b7772):** stateful Linux providers in the page
       (`shims/browser/src/cgen/libprov.rs`, `?wasmdlls=soname=path`): the
       replaced library's exports bound to the module, installed as Wine's
       PE module is; the wasm64 `provider_*` imports in gex.mjs (try/throw on
       a per-Worker `WebAssembly.Tag`); its syscalls marked (bit 61) and
       served like a wasm32 program's (threads staged, waits in place);
       FUTEX_LOCK_PI in the page (retried outside the host lock); the
       provider's stop-the-world (GuestStop) at safepoints and syscall
       returns. `tests/unity-dotnet/embed` passes in Chromium.
     - **B2 done (6fb2dcc4):** Mono's wasm JIT in the page (table range with
       the placeholder, per-Worker instantiation, slot signatures parsed from
       the provider module since JS has no type reflection). embed with the
       JIT at threshold 1 passes under node (`run-node.mjs`, wasm64 shim) and
       in Chromium.
     - **How to run:** `scratch/unity/page/embed.sh NAME` (Chromium, gooner
       is fine: not a game) and `scratch/unity/page/node-embed.sh LOG
       [--env ...]` (node, faster, full wasm stacks with GEX_TRAP_STACK). Own
       harness state and ports: `TMPDIR=scratch/unity/page/tmp
       GEX_NT_CHROME_PORT=9531 GEX_NT_VITE_PORT=5391` (the default state dir
       and ports are shared with other streams). Page assets copied into the
       worktree's untracked `web/public/gex`: `xwayland/` (from wt/games2),
       `gex-wgpu-replay.*` (from a gate tree); `web/node_modules` from `npm
       install`. Guest GL: `scratch/unity/gl64-new` (with the MAP_32BIT ring).
     - **Next: B3, Muck in the page on edger** (its opened-files list from a
       native `--strace` run on edger, `scratch/unity/eruns/mk-st/files.txt`).
  29. **What the ~200 s of start-up with the UnityPlayer unit is (hk-a5,
     edger, warm, `PROF=1` over the first 240 s):** guest code is only ~7%
     of the samples. The rest is the host: hash-map inserts and hashing
     (~12%: the function map's publish/replay), `Table::set` and wasmtime's
     funcref type checks (~7%: every thread's store installing the unit's
     55,438 slots as it catches up; Hollow Knight has ~70 threads), the
     function JIT still translating the other libraries (gex_xlate
     summaries, regalloc, Cranelift; libc, libstdc++, the game's other .so),
     and malloc. Levers: install a unit's slots once per process rather than
     per store (a shared table, or lazy per-slot installs), units for the
     remaining libraries (libc/libstdc++/libGL-side), and the per-thread
     replay cost. The page has the same shape (each Worker instantiates
     every unit), which matters more there.
  30. **B3, Muck in the page, first run (2026-10-08, edger Chromium 153,
     WebGPU on the P620; `scratch/unity/page/muck-page.sh NAME MINUTES`, run
     through bench.sh; no function JIT, the .NET wasm JIT (on by default in
     mono.wasm; this item first said interpreter, wrongly); aa4b35e1):** Muck
     boots on the .NET provider in the page, first frame 79 s after the page
     opened, 11.5 presents/s from ~300 s to ~700 s (native edger, warm
     caches, .NET JIT, mk-j1: first present 154.6 s, menu 38.6 presents/s;
     so the page is about 3.4x slower in presents/s than native).
     Then a WebGPU validation error in Mesa's gex_wgpu stops drawing: "Bind
     group layout [draw VS:GLSL19/FS:GLSL19 g0] ... does not match layout
     [draw VS:GLSL4/FS:GLSL4 g0] of bind group [draw VS:GLSL20/FS:GLSL20 g0]"
     (a bind group made against one program's layout, used with another's):
     for the GL stream. The screenshots show only the harness's terminal
     (the canvas is not in the shot); the compositor reported 118,580 lit
     pixels while it drew.
     What it took (this commit): guest code a provider called waits in place
     and stages threads (a trap to the run loop had abandoned the provider's
     frames, and the guest then returned into the provider's code page:
     SIGSEGV); tier-1's syscall import starts staged threads in a provider's
     process; provider call-outs nest up to 400; gex-fjit lays a function of
     over 1,500 blocks out flat in the browser (its structured emission
     overflowed a Worker's stack on UnityPlayer under `?fjit=1`, not retried
     yet).
     Staging on edger: `scratch/unity/page/muck-bcl` (the run's BCL + 
     mono.wasm), `muck-files.txt` (host files from a native `--strace`), the
     worktree synced to edger without `target/`.
  31. **Visual check, fjit retry, B4 (2026-10-08, ff0456a2):**
     - The compositor reads its output back (`Compositor.readout()`,
       `readoutBase64()`); `browser-linux.mjs` writes it as `cNNN.png`
       next to each 30 s screenshot. mp9's show the empty desktop with the
       cursor (it never presented: below).
     - mp9 (`--fjit`) overflowed a Worker's stack in gex-fjit's emitter
       again: the 1,500-block cut misses deeply nested functions. Now the
       structured layout is also refused in the browser when `nest_bound`
       (an upper bound on the `tree`/`within`/`node` recursion over the
       dominator tree) exceeds 200; such functions are laid out flat.
     - B4: `?elfunits=SO,...` (with `?cgen=1`) places the natively built
       unit of a listed ELF object at the bias it maps at
       (`libprov::elf_unit_maps`, keyed as natively; served from
       `GEX_CGEN_CACHE` at `/gex-cgen/`). Muck's UnityPlayer unit at
       0x7000000: 76,412 functions, 3,805 C units, 142 MiB wasm, 1,687 s to
       build on gooner; staged on edger in
       `scratch/unity/units/cgen-page`.
     - mp11 (`--fjit`) and mp13 (`--fjit`, unit) then died at ~160 s:
       thread 0 returned into the provider's code page from libc after a
       clock_nanosleep with no call-out open. A provider's guest call that
       lands on a function JIT or unit body directly has no fallback
       call-out, so the blocking syscall went back to the run loop and
       abandoned the provider's frames. Fixed in 9bbb85ee:
       `gex.provider_guest_call(+1/-1)` around `gexp_call_guest` (the
       shim's export bound straight in; a no-op natively), counted in
       `provider_callout_open`, and restored by `provider_try` on a raise.
       The provider build is `scratch/unity/dnc-gc/provider/mono.wasm`
       (PROVIDER_DIR; dnc is left alone for the gate). It is staged on
       edger as `muck-bcl/mono.wasm`, with the old one at `mono.wasm.prev`.
       embed passes natively and under node.
     - **mp14 (edger Chromium, `--fjit` + the UnityPlayer unit via
       `?cgen=1&elfunits=UnityPlayer.so`, .NET JIT):**
       - The unit is placed in the page (76,412 entries, 1.7 s).
       - First frame at 72 s.
       - The menu holds at 20.0-20.4 presents/s from 180 s to 480 s. That
         is 1.8x faster than mp8 (no fjit, no unit: 11.5/s) and 1.9x slower
         than native mk-j1 (38.6/s).
       - The readbacks (`shots/mp14/c005-c016.png`) show Muck's real main
         menu: the scene, the buttons and the version text. The ground has
         dotted-line artefacts.
       - At about 500 s the same bind-group validation error (GL's) blanks
         the window to black.
       - The glibc_math and glibc_mem provider modules have to be in the
         page's unit cache as well (copied from ~/.cache/gex/cgen/providers).
     - Not run yet: the .NET interpreter in the page (`PAGE_ENV=GEXMONO_WASM_JIT=0`)
       and `--fjit` without the unit. They were dropped to keep edger's lock
       free for the integ/2026-10-08b gate.
     - Per-Worker unit install time is now logged (`[unit] installed at
       BASE in N ms`, when 20 ms or more): the measure for item 4, the
       once-per-process slot install.
  32. **Where the page's 20 fps goes (mp16, edger, 2026-10-08):**
     - Setup: `--profile 240,20` (new in browser-linux.mjs) profiles the page
       and all 27 Workers for 20 s at the menu, with fjit plus the unit.
       The profiles are in `scratch/unity/page/shots/mp16/*.cpuprofile`.
     - The page's main thread is 100% idle, and the compositor Worker 53%
       idle (its busy part is createRenderPipeline, createBindGroup and
       gex_wgpu draws).
     - Guest Worker 3 (Muck's main thread) spends 39% of its time in
       `KLock::take`: 18% from `gex_wake` → `schedule` after a park, the
       rest from syscalls. Another 17% goes to `k_park_futex`. Guest code is
       only a few percent.
     - Worker 24 (the render thread) spends 45% in `gl_submit` (the page's
       synchronous submit), and that call ran under KLOCK, inside the
       ioctl in `gex_syscall`'s locked dispatch. 33% goes to parks.
     - The ~22 idle Workers each spend 65% in `k_park_futex` and 34% in
       `KLock::take` (from `schedule` on wake-up): a convoy behind the lock
       held across GL submits.
     - Fix (8842f365): the GL ioctl records the submit, and `gex_syscall`
       runs it once the lock is released. The GLX page test passes.
     - **mp17 (the same setup with the fix):**
       - The menu runs at 21.2-21.6 presents/s, against 20.0-20.8 in mp16:
         about 5% faster.
       - Time in `KLock::take` drops to 1% on the main thread and 0% on the
         idle Workers.
       - The limit is now the GL round trip. The main thread parks 52% of
         its time waiting on the render thread. The render thread spends
         48% in the synchronous `gl_submit` (about 2.5 submits per frame)
         and 28% parked. The compositor Worker is 50% idle. This went to GL.
       - The first frame came at 110 s (72 s in mp16). Not explained yet.
     - **Unit install per Worker (item 4):** 36-45 ms for the 76k-slot unit,
       24 installs in mp17, so about 1 s in all. Installing it once per
       process is not worth it in the page.
     - No `[unit]` lines reached the log: the harness keeps only console
       errors from Workers, and those lines are `console.warn`. The
       per-Worker install times need console.error (to do).
     - The dotted grass is Muck's geometry-shader grass (GLSL 410,
       `layout(points)` to `triangle_strip`, `max_vertices = 5`, in
       sharedassets0.assets). The page draws only the GS's input points:
       gex_wgpu has no GS. Sent to GL. The sea at the horizon is missing
       too.
  33. **Start-up and GL in the page (2026-10-08, edger, fjit plus the unit):**
     - **The first frame is bimodal: about 71-75 s or about 110-119 s,
       whether or not the KLOCK fix is in.** The A/B pair: mpA1 (fix
       reverted) took 110 s; mpB1 (83548eac) took 71 s.
       - With timestamped console lines (91cc32d8), mp19 shows a silent
         37 s gap between 69.7 s and 106.7 s.
       - Profiles over that window (`--profile 75,25`, mp21) show the main
         thread parked. One Worker spends the time in UnityPlayer's FMOD
         Vorbis decoder (decode_packed_entry_number, decode_map,
         fmod_tremor_mdct_backward, FMOD::CodecVorbis::readInternal under
         CodecFSB5): audio clips decoded on load.
       - **Resolved (mp24, mp25): no run waits longer; the "first frame"
         metric is what is bimodal.** In the fast runs Unity presents 2
         early frames (a 481-word clear/blit shader) at ~71-84 s, during
         loading. In the slow runs it does not, and the first present is
         the first real frame.
       - The real frames start at the same time either way. Presents at
         ~122 s: 60-72 (fast), 42-93 (slow). At ~153 s: 533-650 in both.
         The ~37 s FMOD decode is loading time in both modes; profiling
         30-70 s in each mode shows only the main thread busy
         (translation), with no FMOD yet.
       - browser-linux.mjs now also reports `ok   100 presents N s`, the
         steadier start-up figure.
     - **GL's bind-group fix** (mesa-wink 9f1da0fa, mods/gpe): in mp18,
       presents keep going past 500 s with 0 validation errors.
     - **GL's guest GL gl64q2** (frame-query cache, and an async present
       at swap interval 0), in mp23 against mp19 (both with the gpv Mesa):
       - 46.8-47.6 presents/s at the menu, against 24.2-25.3: about 1.9x
         faster, and above native mk-j1's 38.6 (old guest GL).
       - Submits per frame: 2.3 against 2.6.
       - `muck-page.sh` takes `GL64=` for the guest GL.
     - **The grass is still dots with the gpv Mesa.** Tint rejects 7
       SPIR-V modules ("index 2147483648 out of bounds"). GL has the
       verbose log (mp22).
     - The guest GL from master (4f042637) segfaults Muck natively under
       edger's older gexc (GL's report). To check against the gate's legs.
  34. **Hollow Knight in the page (2026-10-08, edger):**
     - **Staging:**
       - `scratch/unity/page/hk-stage.py RUN OUT` builds `hk/game`: the
         launcher, steam_appid.txt and links to the 115 game files a native
         `--strace` run (hk-st) opened, 519 MiB in place of the whole
         4.9 GB install. It also writes `hk/files.txt` (125 host files).
       - `hk-page.sh NAME MINUTES` runs it, with `--home` set to
         tests/games/seeds/hollow-knight. It reuses muck-bcl and
         mono.wasm.
     - **hp2 and hp3 crashed** at ~103 s (with the .NET JIT and with the
       interpreter alike). A thread ran libmonobdwgc's own x86
       `mono_unity_liveness_finalize`: the miss path re-bridged only ELF
       export providers' addresses, not a replaced library's. Fixed in
       be33e8a4.
     - **hp4 (no fjit, .NET JIT, gl64q2):**
       - First frame at 108 s; 100 presents at 185 s.
       - The menu at 15.2-15.6 presents/s. The readback (`shots/hp4/c019.png`)
         is HK's real main menu.
       - Native hk-j3 was 28.8.
     - **hp5 (`--fjit`):** 59-60 presents/s from 300 s, but every readback
       after 60 s is black. hp1 (`--fjit` with elfunits) trapped earlier in
       a glibc_mem memcpy of -8 bytes from libxcb.
       - Suspect: the browser's function JIT. `GEX_CGEN_EXP=WASMFLAT`
         applies the browser's flat-layout cuts natively, to test that.
     - HK's UnityPlayer maps at 0x11752000 in the page. A unit for that
       base is building on gooner (`scratch/unity/units/build-hk-page.log`).
     - **Bisecting the HK fjit black screen** (`?fjitlibs=+name,-name`,
       731faf46; `scratch/unity/page/hk-bis.sh NAME LIBS` gives the last
       readback's mean brightness: the menu ~37-40, the failure 0):

       | run | fjitlibs | result |
       |---|---|---|
       | hb1 | -UnityPlayer.so | black |
       | hb2 | -UnityPlayer.so,-libc.so.6 | black |
       | hb3 | +none.so (JIT on, no object) | menu |
       | hb4 | +libxcb.so.1,+libX11.so.6 | menu (lit 59k from ~210 s; the last readback only black) |

       The compositor's lit-pixel count (every 30 s in the log; ~5179 is
       the title bar alone, ~59k the menu) separates two things the last
       readback's brightness mixed up: hb1, hb2 and hp5 are never lit,
       while hb4 draws the menu and only its final frame is dark. So the X
       libraries are not the cause; hp1's libxcb crash (`_xcb_in_read_block`
       copying `queue_len` = -8 bytes) stays open.
       - **These rules leaked:** `?fjitlibs=` was checked only on tier-1
         dispatch misses (`serve`), not on `gex.lookup` misses, so JIT code
         calling through a pointer or a PLT slot translated any object
         (hb1 and hb2 still JIT-compiled UnityPlayer and libc that way).
         841b380b checks the rules at every translation and takes
         `name@lo-hi` (hex offsets into the mapping, as traps print them).
       - Next with the strict rules: hb6 `+libGL.so.1` (the guest GL;
         rendering black while presents continue points at the GL stream),
         hb7 `-libGL.so.1`. `hk-bis.sh` now prints the lit timeline too.
       - Native xbig (`scratch/unity/xbn/run.sh`, big X replies across
         many socket reads) passes with `--fjit`, `--fjit --mem64` and
         `GEX_CGEN_EXP=WASMFLAT`; the page runs of it (xb1, xb2) never
         printed (the old xbig printed only at the end; it now reports
         progress).
     - **hp6 (no fjit, `elfunits=UnityPlayer.so`) did not use the unit:**
       UnityPlayer mapped at 0x10752000, not 0x11752000. The function
       JIT's heap spans come from the arena cursor the guest's mappings
       use, so its 16 MiB chunk moved every later mapping in hp1. hp6 is a
       no-unit run: first frame 108 s, 100 presents 183 s, menu 15.7-16.0
       presents/s (hp4: 108 s, 185 s, 15.2-15.6). 841b380b adds
       `?pins=path@base+span` (the closure pins, `gex_pin_object`).
     - **hp7 (no fjit, the UnityPlayer unit pinned at 0x11752000,
       `?cgen=1&elfunits=UnityPlayer.so&pins=UnityPlayer.so@11752000+2674000`):**
       first frame 55 s (hp6 108 s), 100 presents at 100 s (hp6 183 s),
       the menu at 24.5-27.6 presents/s from ~180 s (hp6 15.7-16.0), lit
       from ~150 s; readback `shots/hp7/c017.png` is the real menu. Native
       gexc on edger (integ/2026-10-08d gate leg): 30.59 presents/s, menu
       ready at 406.5 s. So the page with the unit is about 14% slower
       than native at the menu (26.3 vs 30.59) and reaches it sooner.
       `hk-page.sh` runs want the pin whenever `elfunits` is given.
     - hb6 (`+libGL.so.1`, strict) died at ~133 s: thread 24 in
       UnityPlayer (+0x1dcbe60, tier-1) branched to 0x13450100, a mono.wasm
       export with no bridge (SIGSEGV, status 139). Not seen in any other
       run; hb10 repeats it.
     - The page keeps the guest's output (`shots/NAME/out.txt`), and
       hk-page.sh passes `-logFile -`, so HK's player log is in it.
     - The HK UnityPlayer unit for 0x11752000 (55,439 functions, 165 MiB
       wasm, key 81cfd177) is what hp7 ran with.
     - **The fjit crash (hb6-hb21, strict `?fjitlibs=`).** Every run that
       JIT-compiles libGL dies at 120-250 s, branching to the provider's
       return address 0x13450100 (`code_page + 0x100`, depth 0). It shows up
       on thread 24 (hb6/13/16/20) or thread 0 (hb21). hb10/17 instead had
       thread 0's poll return to a heap address. `+libxcb,+libX11` (hb4) and
       `+none.so` (hb3) reach the menu.
       - Block 0x12c17e60 is UnityPlayer's `syscall@plt`. The diagnostic's
         `.so +X` is relative to the text mapping, so UnityPlayer's base
         there is 0x10752000. The caller is Unity's futex lock slow path at
         +0xdf1ecd (FUTEX_WAIT_PRIVATE through `syscall()`).
       - At the fault rax is still 0xca (futex, never answered), rcx is
         after libc syscall()'s `syscall`, and rsp is syscall()'s entry rsp.
         So the futex trapped out to wait, and the thread resumed at the
         provider's ra instead of the saved pc (0x12ee34f9).
       - Every provider guest call in the ring is 0x10d90 deeper than that
         sp, every call-out is closed, and "provider guest call open" is
         false.
       - `provider_try` resets GUEST_CALLS on any exception, not only its
         raise tag, and rethrows. So a wasm trap that unwound a provider's
         frames would look exactly like this.
       - hb21 (`+libGL.so.1` only; since 7e23c82a the rules also keep other
         objects' callees out of a module) crashed the same way at 248 s. So
         JIT'd libc is not needed for it.
       - Diagnostics, f823644f: `[gex] a trap left through N provider guest
         call(s)` (from provider_try, with the JS stack) and `... reached the
         run loop with N open`. The wild-branch report now also lists the
         Worker's last 16 set_resume calls (lib.rs line, pc).
       - hb23 and hb24 (`+libGL.so.1,provcheck=1`) crashed the same way.
         provcheck said every provider guest call ended at its slot + 8, and
         the dump showed the "slot" at 0x57f4830 holding 0x12d7a280: no
         `ret` read 0x13450100. hb22 (`+libc.so.6`) instead ran a libc
         continuation (+0x8ea87, after `call *0x378(%r12)` into ld.so's
         `_dl_catch_error`) with Unity's registers and called a heap address.
       - **Cause: a stale resume (fixed in e2f06c87).** The dispatch miss
         that recognises a call-out's return (`cgen::returned`) sets the
         resume and traps when it cannot leave by returning, which is the
         case with JIT bodies on the stack. `#callOut`'s catch returned on
         `gex_callout_hit()` without reading the resume. The next time the
         thread blocked with nothing else to run (`schedule()` returning 2;
         here Unity's futex), the run loop took the stale resume. That was
         the provider's ra 0x13450100 (hb21/23/24, the last resume in the
         ring) or 0x12e63a87 (hb22). The catch now drops the resume when it
         ends its own call-out.
       - hb25 (`+libGL.so.1`, the fix): no crash; lit 59-60k from ~150 s;
         the last readback (`shots/hb25/c013.png`) is the menu; 15.0
         presents/s at 392 s (no unit). hb26 (the full function JIT) and hp8
         (`--fjit` with the pinned unit, the target configuration) are
         queued (`q16.sh`, `q17.sh`).
     - **HK's start-up in the page (hp7, the unit):** first frame 55 s;
       "Performing automatic level start" by 122 s; 18 presents/s by 152 s;
       the menu is up at about 2.5 min (native gate: menu ready 406.5 s).
     - **After the fix (2026-10-08 evening; `hk-bis.sh`, `hk-bisu.sh` for
       the unit runs; lit = the compositor's lit pixels, ~59k the menu):**

       | run | config | result |
       |---|---|---|
       | hb26 | full fjit, no unit | never lit, 59.5 presents/s; player log: "The selected window backend is (null)", "Video subsystem has not been initialized", "Desktop is 0 x 0" |
       | hb27 | +UnityPlayer.so | menu (lit from ~270 s) |
       | hb28 | +libc.so.6 | menu (lit from ~210 s) |
       | hb29 | -UnityPlayer.so,-libc.so.6 | never lit; SDL's X11 video init fails as in hb26 |
       | hp8 | full fjit + the pinned unit | died at 32 s: p_memcpy OOB from libxcb `_xcb_in_read_block` (+0xf16f), `queue_len` negative (as hp1) |
       | hu1 | +libxcb.so.1 + the unit | menu at 393 s, 8.9 presents/s |
       | hu2 | -libxcb.so.1 + the unit | **menu, 41.6 presents/s** (first frame 128 s, 100 presents 166 s, 41.1-41.7 from 302 s) |

       - hu2 against hp7 (the unit, no fjit): 41.6 vs 26.3 presents/s at
         the menu (58% more), and against native gexc's 30.59 (36% more);
         but the first frame is later (128 vs 55 s), as the JIT compiles.
       - libxcb crashes only with other objects JIT-compiled too, and alone
         it is fine (hu1). `queue_len` can only go negative if two threads
         run `read_packet` at once, so the suspect is the iolock: libxcb
         drops it around `poll()` in `_xcb_conn_wait` and takes it again
         after. A wrong resume after the blocking poll (the class of the
         stale-resume bug) would skip the re-lock.
       - The black screen is in SDL's X11 video init and needs neither
         UnityPlayer nor libc JIT-compiled (hb29). 3-minute strict runs,
         judged by the player log's "window backend is x11|(null)":

         | run | fjitlibs | backend |
         |---|---|---|
         | hb30 | +libX11,+libxcb | x11 |
         | hb31 | +the 12 X extension/aux libraries (Xext, Xrandr, Xi, Xinerama, Xcursor, Xss, Xxf86vm, Xfixes, Xrender, X11-xcb, Xau, Xdmcp) | x11 |
         | hb32 | +ld.so, libdl, libpthread, libm, libstdc++, libgcc_s, librt | x11 |
         | hb33 | everything but UnityPlayer, libc and all 14 X libraries | x11 |
         | hb34 | everything but UnityPlayer, libc, ld.so, libdl, libpthread, libm, libgcc_s, librt | (null) |

         So it takes the X libraries together with something outside them.
         (A `+` in `?fjitlibs=` must be `%2B` inside a name: hb34 meant to
         drop libstdc++ too and did not.) `q24.sh`: X plus libstdc++
         (hb35), plus the GL libraries (hb36), plus audio/system (hb37).
     - **xbig hung in the page (xb3 without fjit, xb4 with; fixed in
       895a3b8c).** xb5 (per-step times): round 0 (300 KB property, 480 KB
       image, 50 atoms) done at 1.7 s, then the second property read never
       returned. The X server is the host-side wasm Xwayland, and its
       `gex_poll` (web/xwayland/gexos.c) set POLLOUT on every descriptor
       but returned only for POLLIN: a reply that did not fit the guest's
       kernel socket (`SOCK_BUF`, 256 KiB) waited for input that never came,
       since the client was waiting for the reply. POLLOUT now comes from
       the socket's room (`k_sock_writable`, `hostx.js` `writable`) and
       counts. Any X client reading more than 256 KiB at once was exposed
       (Unity's XGetImage/property reads, Wine). Rebuilding Xwayland.wasm on
       gooner needed xorgproto 2024.1, util-macros, libxshmfence's header,
       wayland-protocols and W's host wayland-scanner staged by hand
       (`scratch/unity/xwl/`, `PKG_CONFIG_PATH`/`ACLOCAL_PATH`).
     - lockpoll (libxcb's unlock/poll/lock on 4 threads, 80,000 entries,
       `scratch/unity/page/lockpoll`) passes in the page with and without
       fjit (lp1/lp2) and natively, so the hp8 libxcb race is not that
       pattern on its own.
     - **Muck's grass is fixed by `?gllimits=41` with GL's rebased Mesa
       (mods/gpu, mesa-wink gl/perf 60202246):** mpg3 (fjit + the unit):
       the context is GL 4.2, no "Shader Unsupported" for the grass, and
       `shots/mpg3/c013.png` shows the grass blades and the full 3D menu
       scene; 36.6-38.1 presents/s. mpg1/mpg2 (mods/gpu at GL 4.0) were
       dots. With the site Mesa (mpg4) the 3D scene is never drawn (lit
       5179; "WebGPU validation: Bind group layout ..."), so it needs both.
       mp26's black scene was at GL 4.6 (the compute limits too); 4.1's
       texture limits alone do not cause it.
     - **The black screen bisect, continued (3-minute runs, X = the 14 X
       libraries, judged by the player log's window backend):**

       | run | fjitlibs | backend |
       |---|---|---|
       | hb35 | X + libstdc++ | x11 |
       | hb36 | X + the GL libraries | x11 |
       | hb37 | X + audio/system | (null) |
       | hb39 | X + libbsd, libz, liblzma and the other small ones | x11 |
       | hb40 | X + audio, dbus, systemd, udev, steam | (null) |
       | hb43 | X + dbus, systemd, udev | (null) |
       | hb44 | X + libsteam_api | x11 |
       | hb45 | X + libdbus-1 | **(null)** |
       | hb46 | X + libsystemd | x11 |
       | hb47 | X + libudev | x11 |

       So the X libraries JIT-compiled with libdbus. `LD_DEBUG` (hb41 fails,
       hb42 passes): after `SDL_X11_LoadSymbols` the failing run unloads
       the X libraries at once (`X11_CreateDevice` failed) and has opened
       one X connection, the passing run two. libdbus does not touch X: it
       only changes which functions land in which batch, so this looks like
       the same libxcb miscompile as hp8's crash. hs1/hs2 (`?strace=1`, X +
       libdbus and X + steam) record the syscalls around the connect.
     - **hp8's libxcb crash, bisected inside libxcb** (the unit + fjit;
       `name@lo-hi` excludes an offset range of the object's text):

       | run | fjitlibs | result |
       |---|---|---|
       | hu3 | +xcb,+X11 | pass |
       | hu4 | +xcb,+libc | pass |
       | hu11 | +xcb,+ld.so | pass |
       | hu12 | +xcb,+libc,+ld.so | pass |
       | hu6 | everything, xcb 0-9f00 excluded | pass |
       | hu7 | everything, xcb 9f00-14000 excluded | crash |
       | hu8 | everything, only xcb 3120-32a0 (`_xcb_in_read_block`) excluded | pass |
       | hu9 | everything, xcb 0-3120 (the PLT, `xcb_connect_to_fd`'s callers) excluded | pass |
       | hu13 | everything, `watch=` on `queue_len` (c+0x1074) | queue_len stays 0 to the crash |

       It is deterministic (~30-34 s, libxcb +0x316f). At the crash the
       registers hold `memmove(queue, queue+8, -8)` with done = 8 while
       `queue_len` is 0: the `min(queue_len, len)` select came out wrong in
       the page's code, not a race (the iolock theory above is wrong;
       lockpoll agrees). The offline harness (`scratch/unity/fjitrepro`:
       libxcb alone, and with libc and the glibc_mem providers, eager and
       lazy binding) emits a correct select, so it depends on the page's
       batch: PLT stubs inlined into it (`gex-fjit` inline.rs) or a callee
       summary. `?fjitlibs=dumpmod=ADDR` (fjit.rs) dumps the module whose
       batch root is ADDR as base64 `[fjitdump]` lines; hu14
       (`dumpmod=1a79d120`) captures the crashing module.
     - **Start-up in the page (unit + fjit, `-libxcb.so.1`):** ht1
       (`?attachtrace=1`): thread Workers attach ~0.1 s after their post, and
       posts are ~0.25 s apart, so spawning threads costs little. ht2 (a
       Chrome profile of the main Worker, 70-120 s): 64% (33 of 52 s) in
       `fjit::translate`: recovery 27.6 s (discover/recover_one 16.8, lift
       10), analyze 6.4, jump tables 4.4, emit 5.5, build_runtime 5.7, and
       7 s in `lib_wanted`/`object_file_off` (only with `?fjitlibs=` rules).
       ht3 (`byobj=1`, counts by object up to the menu,
       translations/functions/wasm): UnityPlayer.so 750 / 3,667 / 36 MB
       although the unit serves it, then libasound 7.9 MB, libc 4.7 MB,
       libcrypto 3.5 MB, libX11 2.1 MB; first frame 125 s. A miss in
       UnityPlayer (a function the unit lacks) pulled up to 63 of the unit's
       own functions into its batch. 1cf4e76f keeps callees the unit serves
       out of the batch (`cgen::index_of`).
     - **Root cause of both: the JIT kept a dlclose'd library's function
       starts (fixed in 763a1538).** `?fjitlibs=dumpmod=` lines must be
       under ~4,000 characters (the page console cuts them; ae44c162 uses
       3,000; `dumpmod=OBJ@OFF` names a function by object and text-mapping
       offset). hu16's module has `_xcb_in_read_block` ending after its `cmp`
       at +0x2d with a tail call to +0x30, the `cmovle`, and no flags stored
       before it: +0x30 was a strong start, so recovery ended the function
       there as at a tail call, with the flags dead, and the `cmovle` read
       stale flags: `min(queue_len, len)` gave `len` (hp8's memmove of -8
       bytes). hs1/hs3 (X + libdbus) show the first X connection shut right
       after the setup `writev`, with no read: `read_setup`'s
       `_xcb_in_read_block` took the reply from an empty queue (the black
       screen). hs4 (`dumpmod=libxcb.so.1@3150`) found no batch holding
       +0xf150 as a function, and hs5 (`scanconst=0x1310d150`, which reports
       every module mentioning an address) found it in libdbus's modules:
       SDL dlopens libdbus, dlcloses it, and libxcb is mapped over the same
       range, while the JIT's learned starts (and its set of translated
       functions) are keyed by address and outlived the unmap. That is why
       hb45 needed libdbus JIT-compiled. 763a1538 keys the learned state by
       an epoch that `retire_unmapped_code` moves on (`fjit::on_retire`,
       logged as "code unmapped ... learning afresh"; hb50 had 10 epochs by
       151 s, six from one libasound plugin range). hb50 (full fjit, no
       unit): "window backend is x11", where hb26/hb48/hb49 had "(null)".
     - Also (a12da54a, 6bc61569): a miss strictly inside an `.eh_frame` FDE
       (vfprintf's computed-goto labels, the code after `clone`'s syscall,
       return addresses reached through dispatch) is left to the fallback,
       since `recover_one` records every root as a start for good and would
       split the enclosing function the same way; `.plt`'s single FDE is
       exempt (its stubs are entries). Natively the same holes exist (the
       learned state is per process, kept across dlclose; mid-function
       roots become starts), masked by the images' seeded starts.
     - 1cf4e76f's first form used `cgen::index_of`, which also answers for
       the JIT's own entries, and `executable` is what instruction fetch
       asks of every byte: hu15 decoded the `cmovle` at 0x1a79d150 (a JIT
       entry by then) as illegal (SIGILL, status 132). 5ec43e82 counts unit
       entries only (`cgen::unit_serves`).
     - **Results after the fixes (2026-10-09, HK in the page on edger;
       presents/s at the menu, from the last minutes of 7-minute runs):**

       | run | config | shim | first frame | 100 presents | menu presents/s |
       |---|---|---|---|---|---|
       | hp7 | unit, no fjit | before | 55 s | - | 26.3 |
       | hu2 | unit + fjit `-libxcb` | before | 128 s | 166 s | 41.6 |
       | ht3 | unit + fjit `-libxcb` | before (byobj) | 125 s | - | - |
       | ht5 | unit + fjit `-libxcb` | 5ec43e82 (unit callees out of batches) | 113 s | - | 40.2 |
       | ht6 | unit + fjit `-libxcb` | a12da54a (+ mid-function misses to the fallback) | 78 s | 96 s | 39.6-39.7 |
       | hp9/hp10 | full fjit + unit | a12da54a/6bc61569 | - | - | crash at 23.6 s (memmove after writev: the stale-start split) |
       | hp11 | full fjit + unit | 763a1538 (epochs) | 131 s | 161 s | 40.7-42.0 |
       | hp12 | full fjit + unit | 58e95157 (epochs only where translated) | 128 s | 158 s | 42.4-42.7 |
       | **hp13** | **full fjit + unit** | **977a2236 (current JIT callees out of batches too)** | **87 s** | **119 s** | **39.6-39.9** |
       | hb51 | full fjit, no unit | 58e95157 | 222 s | 270 s | 33.9 at 392 s, rising (x11 backend; was black) |
       | hb52 | full fjit, no unit | merge/unity-rekey d146c248 | - | - | SIGILL at start-up (status 132) |
       | hb53 | full fjit, no unit | merge/unity-rekey 64efd78e | 178 s | 225 s | 35.7 at 392 s (x11 backend) |

       Native gexc's gate (unit, no fjit): menu at 406.5 s, 30.59
       presents/s. hp13 against hp7: 52% more presents/s (39.9 vs 26.3),
       first frame 32 s later (87 vs 55 s). hp13's menu rate is 6% below
       hp12's (39.9 vs 42.4): callees served by older modules are called
       through the table instead of directly, which may cost that much, but
       the menu rate ranges 39.7-42.7 across these runs. By the menu hp13's
       JIT has made libasound 20 MiB of wasm (947 functions), libc 4.3 MiB,
       libudev 3.2 MiB, libcrypto 2.9 MiB; UnityPlayer is out of the top 12.
- **merge/unity-rekey (64efd78e, one commit on 4ed5cad3, for its own
  round):** the native JIT's two holes fixed in gex-xlate
  (`Jit::retire(process, ranges)` from both hosts' on_retire; a miss's
  root stays a start only if it was a strong one already) and fjit.rs out
  of GEX_CGEN_RECOVERY_INPUTS (key c6014ffc... with and without an
  fjit.rs edit, 855acf6a... after a func.rs edit). hb52 (d146c248) died
  with SIGILL: with misses no longer starts, recovery walked over a JIT
  entry inside the function, and the page's "served" test was part of
  `executable`, which fetch asks of every byte, so the bytes decoded as
  illegal. 64efd78e passes it as `skip` (`Jit::translate_skipping`), the
  batch bound only. hb53: menu, x11, first frame 178 s, 35.7 presents/s
  (hb51 on the epoch shim: 222 s, 33.9). Native: tests/fjit-remap (the
  Oct 8 gexc gets 99,800 of 200,000 wrong under --fjit), fjit-procs,
  x86-programs (82 ok), embed PASS, gex-xlate tests. The page's unit cache
  (scratch/unity/units/cgen-page) must be rebuilt after it lands.
- **Mesa pin (1d5b26d9): mesa-wink 60202246 and `gllimits=41` by
  default.** Replays gpu vs this build: Noita 92f095c5, HK 9eece5c5, 0
  rejects and 0 validation errors each; GL's 4-slot repro (tinyu) 2,777
  frames with 0 validation errors; browser-gui --gl x86 and x64 12 ok
  each; HK hp14 (merge/unity shim, this Mesa) menu at 41.8 presents/s.
  Muck with the default and no `?gllimits` (mk2, the seed shim, no unit):
  "OpenGL 4.2" context, no "Shader Unsupported", grass blades in
  `shots/mk2/c013.png`, 36.9 presents/s at 393 s.
  (Superseded 2026-10-19: the default is now `gllimits=43`, GL 4.6 with the
  adapter's compute limits. Muck renders correctly there with Mesa
  g2/msvo-unformatted, and its MultiScaleVO AO costs about a quarter of the
  frame rate: 22.7-24.1 presents/s against 32.6 forced to 4.2, edger, G2;
  see docs/gl-path.md.)
- **Muck dies with SIGILL at ~25 s on merge/unity's shim (977a2236,
  4ed5cad3)**, with the unit (mpg5, mk1) and without (mk4). It does not
  with 64efd78e (mk3, 4 minutes, no unit) or 1bf5ba09 (mk2). The page asked
  "served by the unit or the JIT" of instruction fetch (part of
  `executable`); with epochs a JIT entry the current epoch has no start
  for is read as non-code inside the function being recovered and decoded
  as illegal. 64efd78e passes it as `skip` (batch bound only).
- **Start-up (decision 3).** hp14's main-Worker profile, 10-90 s (unit +
  fjit): 61% (49.8 of 81 s) in `fjit::translate`: recover_one 27.2 s
  (jump tables 16.9, discover 9.2, lift 6.9), summaries 17.7, build_runtime
  8.8, emit 8.4, prepare 5.5; the per-byte `executable` 5.9 (2.3 in
  elfprov::provided's locks). Audio: the page has no audio device; FMOD's
  output init fails ("Unknown PCM default", tried twice), after pulse
  fails to connect. libasound's 20 MiB was mostly copies: the page's JIT
  had no function starts but those calls name, so a tail jump absorbed the
  function it reached, and libasound's exported wrappers each carried one.
  Offline (fjitrepro, all 1,698 exports): 57.2 MiB / 16.4 s unseeded, 9.7
  MiB / 7.1 s seeded. ALSA with no config (hp15, ALSA_CONFIG_PATH=/dev/null)
  cut libasound to 11.2 MiB but not the first frame (112 s against hp14's
  110 s, both profiled), so it is not the path to cut.
  merge/unity-seed 1bf5ba09 (on 64efd78e) seeds every mapped object's
  starts (`Jit::add_image`) before a translation and takes the fetch
  checks once per translation: no unit, hb53 (64efd78e) 178 s first
  frame, 225 s 100 presents, 35.7 presents/s; hb54 (seeding) 121 s, 171 s,
  39.1; hb55 (both) 121 s, 166 s, 40.1; libasound 26.0 -> 3.7 MiB.
  The HK unit re-keyed for 64efd78e (key 7bd87731, byte-identical c.wasm,
  1,941 s on gooner) is on edger in `units/cgen-page-rk`; Muck's is
  rebuilt too (4e58f35b, 884 s), also in `cgen-page-rk`.
  With the unit (target configuration, seed shim 1bf5ba09):
  - hp16 (HK): first frame 56 s (hp13 87 s), 100 presents 85 s (119 s),
    41.5 presents/s at 392 s (39.9).
  - hp17 (HK, profiled 10-90 s): `fjit::translate` 20.7 s of 81 s (25.5%;
    hp14 49.8 s, 61.5%); the main Worker now waits on its threads 31.6 s.
    Profiled runs' first frame (110 s both) is held by the profile.
  - mk5 (Muck): first frame 82 s, 100 presents 95 s, 34.0 presents/s at
    392 s, OpenGL 4.2, no "Shader Unsupported", grass blades
    (`shots/mk5/c013.png`).
  Branches: merge/unity-rekey 64efd78e (own round), merge/unity-seed
  1bf5ba09 on it (worktree wt/unity-rk); merge/unity 1d5b26d9 (pin).
  Natively the JIT seeds only the main image too (gexc's
  FjitTranslator::image); a Translator hook per mapped object would do
  the same there.
- **Single U head for the gate: merge/unity-seed 0ddc9390** (merge/unity
  a0f6a8c9 merged into merge/unity-seed; has 64efd78e, 1bf5ba09, 4ed5cad3,
  1d5b26d9). Gated as integ/2026-10-09b. Master 9e5ac0ff merged in at
  114b6b69. Master's gex-abi change moves GEX_CGEN_RECOVERY_INPUTS, so the
  HK/Muck units are rebuilding on gooner for 114b6b69
  (`scratch/unity/units/build-m1.sh`, gexc copy `units/gexc-114b6b69`,
  logs `build-{hk,muck}-m1.log`). Runs until then use the pre-merge shim
  `scratch/unity/shim-lazy` (= 1e9154ad) with `units/cgen-page-rk`.
- **B5: Hollow Knight in the page past the menu into gameplay (1e9154ad,
  2ea7e6f2).**
  - Lazy files: `TmpFs::add_lazy(path, size, url)`, which reads 1 MiB chunks
    through `env.lazy_fetch`: a synchronous XHR with Range and
    `Cache-Control: no-store`.
    - nt-page takes `?lazy=MANIFEST`.
    - browser-linux.mjs takes `--lazy DIR` (the 4.9 GiB install, 1,754
      files) and `move`/`click` in `--input`.
    - The script is `tests/games/hollow-knight-page-input.txt`, the native
      script timed from present 600.
  - hg1 failed every read past a file's first chunk. Without no-store,
    Chromium's cache answered later ranges badly; curl against vite gets
    206 for every range.
  - hg2 (fixed fetch, .NET wasm JIT on): no failed reads. It went through
    the menu, profile, scale and brightness pages and the intro skip into
    the opening sequence. Then Mono asserted at loader.c:1826 (token table
    != METHOD) at ~340 s.
  - hg4 (diagnostic provider) shows the method is NULL: the signature of a
    NULL MonoMethod under `mono_runtime_invoke`.
  - **hg3 (`PAGE_ENV=GEXMONO_WASM_JIT=0`, the interpreter): King's Pass,
    the knight walks and the "X Attack" prompt is shown
    (`shots/hg3/c029.png`). Gameplay runs at 14.4-14.9 presents/s; the
    menu ran at ~41.**
  - Native hk130 (the King's Pass reference) ran before the JIT became the
    default (d4a9389d, Oct 7 18:45), so it used the interpreter too. The
    JIT bug is not page-specific.
  - Open: which JIT path asks for the NULL method's signature. hg5 traps
    there for a named wasm stack (provider `dnc-gc/provider/mono.wasm.diag2`,
    staged as edger `page/muck-bcl-diag2`). The staged `muck-bcl/mono.wasm`
    is unchanged (`mono.wasm.good`). The diagnostic edits in
    `dnc/src/.../loader.c` and in host/gexmono-adapters.h and
    gexmono-provider.c (`gexmono_last_api`) are uncommitted.
- **The .NET JIT's NULL-method assert, located (hg7, provider with a trap
  at the top of `mono_method_signature_checked_slow`):** JITted
  `tk2dSpriteAnimator:Start ()` (wasmjit-batch[3]) -> `wj_call_interp_inner`
  -> `interp_entry` -> interp -> icall `ves_icall_mono_delegate_ctor_interp`
  -> `interp_delegate_ctor`. There `ftnptr_to_imethod(addr)` gives an
  InterpMethod whose `method` is NULL, so the ldftn value reaching the
  delegate ctor is not a live InterpMethod. The diagnostic patches are
  `scratch/unity/gexdiag-{host,loader}.patch`, and the trap build is
  `dnc-gc/provider/mono.wasm.diag3`. Both sources are reverted.
  - hg6 repeats hg3 (interpreter) with no other Chromium on edger: King's
    Pass, 14.5 presents/s.
- **Start-up (item 2).**
  - hp18 profiled every Worker from 5 s to 65 s. The browser-harness now
    picks up Workers as they start.
  - The main Worker is active 52.8 of 60.6 s. Of that, `fjit::translate`
    is 23.6 s: recover_one 8.0, analyze_fn_full 6.6, build_runtime 5.5,
    emit 5.2 and jump-table resolve_all 5.0.
  - Mono takes 13 s: the interpreter's transform 7.0 s and class init
    6.0 s.
  - `sys_mmap` takes 5.1 s, 4.3 s of it in `zero_dirty`, which scanned
    bytewise because the shim has no SIMD. The scan is now word-wise
    (38739efc).
  - Unity's 20+ other thread Workers are parked; their ~5.8 s each is
    mostly atomics.wait inside KLock::take.
  - Translation is almost all system libraries: libc, libasound, libX11,
    libcrypto, ld.so, libdbus, libudev and libpulse*.
  - System-library units: edger's libraries (`units/edger-libs`, which
    differ from gooner's) are built at the page's bases
    (`build-libs-m1.sh`, `build-libs-m3.sh`) into `units/cgen-page-m1` on
    edger. That directory also holds the re-keyed HK unit 70db2b08, which
    is byte-identical to 7bd87731, and Muck's unit. The key is unchanged at
    7bcb4fbe (master ff69f7e4).
  - With these units, libdbus-1 unmapped and libxcb mapped at its base
    exited with status 255 at ~16 s (hp19, hp22). Fixed in 705dc708: a
    unit on code that is no longer mapped executable is retired whatever
    its bytes.
  - Five libraries load at new bases since master 9e5ac0ff, and their
    units are rebuilt for those bases (build-libs-m3).

    | run | shim | units | first frame | 100 presents | presents/s (212 s) |
    |---|---|---|---|---|---|
    | hp16 | 1bf5ba09 | UnityPlayer | 56 s | 85 s | 41.5 (392 s) |
    | hp20 | 114b6b69 + zero scan | UnityPlayer | 52 s | 70 s | 41.4 |
    | hp24 | 705dc708 | UnityPlayer | 53 s | 83 s | 41.1 |
    | hp21 | 114b6b69 + zero scan | + libc, libm | 45 s | 77 s | - |
    | hp23 | 705dc708 | + 15 libraries (5 at stale bases) | 43 s | 73 s | 40.1 |
    | hp25, hp26 | 705dc708 | + 15 libraries at current bases | 40 s, 40 s | 71 s, 70 s | 39.8, 41.2 |
    | hp27 | 87176416 | same | 37 s | 69 s | 40.3 |

  - hp25/hp26 still JIT-translated libasound (3.1 MiB): ALSA dlopens and
    dlcloses it six times, and the retired unit was never placed again.
    That is fixed in the commit after 705dc708.
  - Left to the JIT by the first frame: ld.so 1.8 MiB (the kernel maps it,
    so `elf_unit_maps` never sees it), Mesa's `/gex/gl/libGL.so.1`
    0.2 MiB, and UnityPlayer misses 0.1 MiB. The edger system libGL unit
    is unused.
  - Library bases are deterministic for a given shim. 5 of 16 moved when
    master 9e5ac0ff merged in, and a mismatched unit falls back to the JIT
    (no fault). Pinning them (`?pins=`) would make the cache independent of
    layout.
  - Native has the same hash-only retire test (`nt_cgen::on_retire`) and
    the same placed-once rule.
- **The .NET wasm JIT's delegate bug, fixed (patch 0005, 3302db70).**
  - Cause: gex runs Mono interpreter-only, so the wasm JIT's `mono_ldftn`
    and `ldvirtfn_internal` returned `create_method_pointer`'s native entry.
    The delegate .ctor it calls runs on the interpreter
    (`ves_icall_mono_delegate_ctor_interp`), which takes function pointers
    as `InterpMethod*`. Under `HOST_GEX` both now return
    `get_interp_method` (bit 0 set for an unboxing receiver, as
    `MINT_LDVIRTFTN` does), except for native-to-managed wrappers.
  - The embed test gains `T.Delegates`: ldftn, ldvirtftn, a boxed struct's
    override and a closure, all made in JIT-compiled code at threshold 1.
    The provider without the patch dies there (exit 139); with it, every
    pattern holds.
  - The provider is `dnc-gc/provider/mono.wasm.jitfix1`, staged on edger as
    `page/muck-bcl-jit1`.
  - hg8 (JIT on) and hg9 (`GEXMONO_WASM_JIT=0`), otherwise the same: shim-m4
    with library units, lazy install, the New Game script. Both reach King's
    Pass, and their screenshots match (`shots/hg8/c023.png`, `hg9/c023.png`).
    The JIT is slower in both phases:

    | phase | JIT (hg8) | interpreter (hg9) |
    |---|---|---|
    | menu, best minute | 39.9 presents/s | 44.6 presents/s |
    | King's Pass, 602-692 s | 11.7-12.7, mean 12.0 presents/s | 14.8-15.3, mean 15.1 presents/s |

    In King's Pass the JIT is 21% slower than the interpreter (12.0 vs 15.1
    presents/s). hg10 profiles the JIT run with its counters on.
- **Native unit retirement matches the browser (0308e2e0).**
  - `nt_cgen::on_retire` and `ftab::on_retire` take the gone pages. A unit
    page that is no longer mapped executable is patched, its non-provider
    entries leave the table, and its ELF object leaves `ELF_SEEN`, so it is
    placed again when mapped again.
  - `ftab::on_retire` clears every entry's probe-cache slot before it
    retires any. Retiring first left a cached unit entry behind: libb,
    loaded where liba's re-placed unit had been, ran liba's unit, which
    bailed to tier-1 and was sent straight back (a spin under `--fjit`).
  - `tests/elf-units-remap`: liba and libb share one pinned base and each
    has a unit. Each is loaded and unloaded twice, with and without
    `--fjit`. The output matches native and the units are placed 4 times.
    The old rule places them twice, so the second loads ran on tier-1.
- **Start-up, item 3.**
  - Pins (1fc1a8dd): `browser-linux.mjs --pins FILE` takes
    `BASE SPAN PATH` lines. `tests/games/hollow-knight-pins.txt` pins the
    16 objects that have units at their 6158cded bases. libdbus goes at its
    second base (libxcb keeps the first). Run: hp28.
  - ld.so (29cbcc4d): gex's loader places the interpreter, so no guest
    mmap ever announced it. Both hosts now push `CodeMap::of_placed` for
    it at load, and its unit (`elfunits=ld-linux-x86-64.so.2`, built at
    0x4a00000 with `--mem64` from edger's ld.so, key 59660fd8) is placed
    after the first syscall.
    - Natively, 18 of 20 ld.so lookups in the remap test hit the unit.
    - Page run: hp29 (shim-m5).
  - The interpreter's "transform 7.0 s" (hp18) was mostly not transform.
    5.07 s of it is static constructors run from inside
    `mono_interp_transform_method` (`mono_runtime_class_init_full`), and
    `generate` itself is 1.64 s. The JIT does not remove it: methods are
    still transformed at tier 0 first.
- **Where the JIT run's time goes (hg10: hg8 with `MONO_WASM_JIT_STATS=1`, counters every 60 s, and a
  20 s profile at 520 s).**
  - Delegates are not the cause, and neither is patch 0005.
    - A delegate built by JIT code now goes through `interp_delegate_ctor` with an `InterpMethod*`,
      exactly like one the interpreter builds.
    - Emitted code has 93 delegate sites, and `call_delegate` exits to the interpreter 601 times in the
      whole run.
  - The JIT calls back into the interpreter far more often than the interpreter calls into it. In King's
    Pass, `residual` (JIT to interpreter) grows by ~67k/s and `invoked` (interpreter to JIT) by ~18k/s.
    - 2,954 methods are registered.
    - Of 58,493 callee sites in emitted code, 30,488 are "untried": the callee was never compiled.
  - The threshold is 500, tuned upstream on Minecraft/IKVM. Re-emission is off; the counters say it
    could reach a further 52.1% of callee sites.
  - The profile is not representative: the game made 0 presents during the window.
  - hg11 (threshold 100) and hg12 (threshold 100 plus `MONO_WASM_JIT_REEMIT=1`) test both knobs.
- **Upstream review (r58Playz wasm-jspi-jit-10.0.3, 6812ea26..2f86e2c9: 21 commits, +8.4k lines).**
  - The new work is Minecraft tuning:
    - B4 hot-set modules
    - S2 profile-guided inlining
    - C1 compact virtual sites
    - tier-2 caps
    - J1 eager list
  - Unchanged: the tier-1 threshold, re-emission and the call_interp handoff.
  - Applying 0001-0005 to the tip gives 15 small conflict hunks in 5 files.
  - The real rebase cost is auditing the new emitters for wasm64.
  - Upstream's CLAUDE.md records an open module-corruption bug.
  - Patch 0006 takes only a0f2c7b9c (compute_reachable as a worklist), which applies cleanly. The
    rebase decision is with the user.
- **Pins in the page.**
  - Pinned at their old arena bases, the pins did not hold:
    - hp28: 8 of 16 landed.
      - 4 slid because the arena had already given part of their span away.
      - 4 loaded unpinned; all 4 were dlopened while other threads run.
    - hp29: 9 were misplaced. The shim's own heap takes from the same arena cursor and grew earlier.
  - 62b38fc2: `MemoryMap::reserve`, and pins armed per thread.
  - 046bec02: the system libraries are pinned in the placement window (0x7000000-0xb000000), with
    their units rebuilt there (`units/build-libs-m6.sh`).
  - hp30 (shim-m6, window pins, ld.so unit):
    - Every pin landed, and every unit was found.
    - The game then hung before its first present. GL was set up: 361 submits, then nothing.
    - The Mesa libGL unit (`/gex/gl/libGL.so.1`) was placed for the first time in this run; earlier
      runs' libGL unit was built from the system libGL and never matched. That makes it the suspect.
    - hp31 repeats hp30 without it.
  - ld.so's unit works in the page: hp29 placed it at 7.7 s with 357 entries. First frame was at 43 s,
    and the menu ran at 42.1 presents/s.
  - hp31 hung exactly as hp30 did, so libGL is not the cause; 62b38fc2 is. It is reverted (42f09b39).
    Which half (the arena reserve or per-thread arming) hangs is not known.
  - hp32 (shim-m5, i.e. 29cbcc4d without 62b38fc2, plus the window pins of 046bec02): all 16 units
    found, no pin misplaced. First frame at 34 s (hp29: 43 s), 100 presents at 66 s (hp29: 75 s), menu
    41.4 presents/s at 212 s.
- **Transition cost (ce395feb: `EMBED_BENCH=N` in `tests/unity-dotnet/embed`, browser, 20M calls a
  leg, ns per call; "refused" is a callee the JIT will not compile).**

  | leg | interpreter only (cb1) | JIT, threshold 1 (cb2) |
  |---|---|---|
  | same tier | 64.8 / 65.5 | 14.2 / 14.2 (JIT to JIT) |
  | to refused | 67.6 / 64.5 | 508.4 / 472.9 (JIT to interpreter) |
  | from refused | 51.1 / 50.0 | 207.9 / 187.1 (interpreter to JIT) |
  | refused to refused | 56.9 / 74.3 | 55.5 / 55.2 |

  A JIT-to-interpreter call costs 7-10 times an interpreter call, and an interpreter-to-JIT call
  about 3 times.
- **Steady-gameplay profile (hg13 JIT, hg14 interpreter: King's Pass, the main Worker only, 1 ms
  sampling, 20 s at 600 s).**
  - Presents did happen in the window. The "0 presents" the harness printed was a counting bug
    (it read the count from before the profile); fixed.
  - About 6 s of each 20 s is `gl_submit` waiting for the page (the minified `Atomics.wait` is not
    counted as wait). Managed code (under `gexp_mono_runtime_invoke`) is 6.60 s in hg13 and 5.27 s in
    hg14.
  - Normalised to the same number of frames (UnityPlayer's own time outside managed code, 3.55 s vs
    4.94 s, as the frame count), managed code costs 6.60 s under the JIT against 3.79 s on the
    interpreter: 74% more per frame.

    | bucket (s, same frames) | JIT | interpreter |
    |---|---|---|
    | JIT code + interpreter execution | 1.16 + 0.33 = 1.49 | 1.07 |
    | JIT/interpreter glue (`interp_entry`, `wj_*`, lazy binding, `get_imethod`) | 1.01 | 0.06 |
    | other mono (call info, hash lookups, mutexes) | 1.02 | 0.50 |
    | guest-call glue (`gexp_call_guest`, `provider_try`, wasm-to-js) | 0.75 | 0.39 |
    | GC-mode transitions (`mono_threads_*`) | 0.49 | 0.24 |
    | UnityPlayer code called from managed | 0.88 | 0.70 |
    | `major_get_used_size` | 0.73 | 0.64 |

  - The JIT does not make the method bodies faster: emitted code plus what is left on the
    interpreter takes 0.42 s more than the interpreter alone. The emitted time is flat (the hottest
    method, `Fsm:UpdateStateChanges`, is 0.06 s self), so no single body is to blame. HK's managed
    code is thin: PlayMaker FSM steps and Unity binding wrappers.
  - Most JIT-to-interpreter time is calls into UnityPlayer. Of the 4.22 s under
    `wj_call_interp_inner`, 3.03 s ends in an icall or P/Invoke. `wj_lazy_refusal` refuses
    INTERNAL_CALL and PINVOKE_IMPL callees ("shape"), so every binding call from JIT code takes the
    ~490 ns crossing, then the interpreter's own icall path. On the interpreter that call is an
    ordinary interpreter call.
  - Lazy call sites to never-compiled callees resolve on every call:
    `mono_wasm_jit_lazy_interp` (0.27 s) runs `mono_wasm_get_call_info` each time (0.13 s), and
    `mono_wasm_jit_pretransform` looks up the InterpMethod by hash each time (0.30 s).
  - Both tiers spend 0.6-0.9 s per 20 s (3-4.5% of the main Worker) in `major_get_used_size`. Team
    Cherry's `GCManager:Update` calls `GetMonoHeapUsage` every frame, and sgen answers by walking every
    object in the major heap.
  - What it would take for the JIT to pay off: direct icall/P/Invoke calls from emitted code (no
    interpreter in between), caching what the lazy interpreter path resolves per site, and then better
    bodies. Even then, the bodies alone are not faster than the interpreter, so HK stays on
    `GEXMONO_WASM_JIT=0`.
- **Making the hot residual callees compile (patches 0007-0020, 2026-10-10).**
  The rule: rank callees by how many interpreter legs they cost, then make the emitter handle each
  bail reason, with a test for each. Leg counts are over a whole 12-minute King's Pass run, from the
  `iroute top` / `iroute kinds` tables. hg10 is the baseline (0001-0006). hg17 is p9 = 0005-0008
  plus LDADDR_ARG. hg22 (p25, through 0019) and hg25 (p29, every knob at its default) are queued as
  q77/q78.

  | reason | example callee (hg10 legs) | fix | after | test |
  |---|---|---|---|---|
  | ldarga of a scalar argument | Unmarshal.FromIntPtrUnsafe (3,005,829) | LDADDR_ARG on in the provider | gone in hg17 | T.ArgAddr |
  | ldaddr (same gate) | KeyCombo.GetInclude (432,402) | same | gone in hg17 | T.HotShapes (Combo) |
  | OP_IADDCC in shared generic code | List\<DelayedEvent\>.AddRange (707,514) | 0008 checked arithmetic | gone in hg17 | T.Checked, T.HotShapes |
  | int_conv_to_r_un | InputStateBlock.ReadFloat (571,200) | 0008 | the bail is gone; still 624,382 legs in hg17, now as retries of an invalid module (next row) | T.HotShapes (ReadFloatLike) |
  | invalid module (a reused 32-bit load in an i64 register) | hg17: invalid=1322; embed: AssemblyName.FillName re-emitted 106 times | 0019 | embed invalid 106 -> 0 | T.LcseWide |
  | guest icall / P/Invoke | Component.get_gameObject_Injected (1,617,846) and 438 others (13.2M legs in all) | 0009 direct guest calls | hg22 | T.NativeDirect, CallBench |
  | the runtime's own icalls, handles included | GCHandle.InternalGet (3.84M in hg17), RuntimeType.GetName, Array.GetLowerBoundInternal, FastAllocateString (3.8M "icall-handles" in all) | 0018 | embed residual 70,359 -> 236 | T.MonoIcalls |
  | string ctors | string(char,int) via the wrapper | 0018 (managed String.Ctor) | embed: 0 | T.MonoIcalls |
  | nested finally / call_handler continuations | tk2dSprite.UpdateMaterial (99,952), GUILayoutGroup.CalcHeight (42,961) | 0012, 0014 | hg22 | T.NestedFinally |
  | ldarga of a reference argument | ObjectEqualityComparer\<Type\>.GetHashCode (69,029) | 0011 | hg22 | T.ArgAddr (RefArg) |
  | "residual reflection" gate | ActionData.LoadActionField (144,905) | 0013: only once a Reflection.Emit image exists | hg22 | T.Reflect |
  | ldarga in EH methods | - | 0015 | - | T.FinArg |
  | localloc, 64-bit checked multiply | Number.FormatFloat, BinaryReader.ReadInt32 | 0016 | - | T.Localloc |
  | vtype call arg lost its variable | BinaryReader.ReadString, NumberToString\<char\>, ResourceManager..ctor | 0017 | embed: compile | T.Localloc (ReadString) |

  Bails left in the embed suite: managed-to-native and runtime-invoke wrappers (marshalling
  wrappers, deliberately refused), REQSECOBJ methods and their callers (correct: they need a stack
  walk), and one EH filter clause.
- **Knob re-enable pass (p25 -> p29).** The wasm64 provider shipped with every JIT fast path that does
  its own pointer arithmetic set to 0. Each one alone on the embed suite at threshold 1:
  - Clean as-is: GUARDED_INLINE, DELEGATE_DEVIRT, IC_MID, PRED_MID, LAZY_COLD, INLINE_COLD_THROW,
    VCALL_AOT_WAYS, LAZY_NOGC, PROF_ORIGIN, PROF_INLINEE.
  - Emitted modules that do not validate ("expected i64, found i32"): INLINE_ALLOC (55), T2 (688),
    EH_REC (43), FAST_TLS (7). Patch 0020 ports the inline TLAB bump, the fast thread read, the EH
    record and the tier-2 poll to pointer width; all four now give invalid=0.
  - DIRECT_DEPS in the off-list was a no-op: "0" is outside its range [1, 512], so the runtime's 512
    was kept.
  - All of them together, at the runtime's own defaults (DELEGATE_DEVIRT=15), pass the suite on both
    legs: invalid=0, 3-4 tier-2 compiles, 10 guarded inlines. The provider's off-list is removed.
- **The worker-thread NULL (fixed, 1846e66a).** The embed test's worker thread sometimes saw NULL from `Make`
  or from `mono_string_to_utf8` ("worker thread: N bad of 640", about 1 run in 20 on either tier; every run
  under `EMBED_WORKER_STRESS`). The object was intact and a second conversion of it succeeded; the provider
  never produced a NULL. The cause was the provider ABI:
  - An adapter puts its result in the guest context's rax, then `gexp_return` runs `gexp_to_guest`.
  - Mono's hook there (the GC-safe transition) can wait out another thread's collection on a futex.
  - musl-wasm64's syscalls go through the same context and leave their result (0) in rax.

  The fix:
  - `gexp_return` keeps rax and rdx across the hook.
  - `gexp_call_guest` runs its hook before loading the argument registers and reads the callee's results
    before its return hook.
  - `gexp_call_guest_catching` restores the guest's registers after the unwind hook.

  The same lost result could hit any provider call that returns while another thread collects, in the page
  as well as natively.

  Keeping the value in a wasm local was not enough. The repeat runs (299 of 300 clean) and
  `EMBED_WORKER_STRESS=20` (about 1 run in 8, on the interpreter and on the JIT, and 4 in 16 with
  `MONO_GC_PARAMS=no-lazy-sweep,no-concurrent-sweep`) still showed held strings turning into other
  strings or into "". Allocation in the provider goes straight to the major heap (no nursery). While the
  hook waits out the collection, the result is referenced only from that local, so the collector
  freed it, and the next allocation reused its slot.
  - Diagnostics ruled out three other causes:
    - the guest-stack scan (it covered `held[]` on every collection);
    - the pin phase (no allocated held object was left unmarked);
    - the sweep of marked objects.
  - The fix: `gexp_return`, and `gexp_enter` for the argument registers, keep their copies in a
    volatile array on the C stack, which the collector scans conservatively.
  - Result: 24 of 24 clean under stress with synchronous sweep, against 4 of 16 failing before.
  - Validation is listed in section 13.

## 13. Final state (stream U, 2026-10-10)

Stream U stops here. Branch `merge/unity-seed`; the head is in the commit that adds this section.

### Default

**Hollow Knight runs on the interpreter (`GEXMONO_WASM_JIT=0`). Muck and the provider in general keep
the JIT on.** The rule was: JIT on only if it beats the interpreter in the browser. q81 (edger,
Chromium, provider p31, King's Pass with scripted input, 12 minutes each; hg26 JIT, hg24 interpreter)
gave these numbers:

| | JIT (hg26) | interpreter (hg24) |
|---|---|---|
| presents/s, 602-692 s | 13.78 | 15.62 |
| presents/s, 482-692 s | 13.85 | 14.64 |
| presents in the 20 s main-Worker profile at 700 s | 276 | 324 |
| managed time in that profile (under `gexp_mono_runtime_invoke`) | 3.94 s | 5.04 s |
| managed time per present | 14.3 ms | 15.6 ms |
| main-Worker time per present | 71.5 ms | 61.2 ms |

- **Frame rate:** the interpreter gives 13% more presents/s over 602-692 s (15.62 against 13.78), and
  6% more over 482-692 s.
- **Managed code:** the JIT's managed code is now cheaper per frame than the interpreter's: 8% less
  per present (14.3 ms against 15.6 ms). In hg13/hg14 (patches 0001-0006) it cost 74% more.
- **Outside managed code:** the JIT run spends 10 ms more per present (71.5 ms against 61.2 ms).
  That is the "other" bucket (1.24 s against 1.08 s) plus UnityPlayer code outside managed code
  (4.98 s against 5.08 s, over fewer frames). It is not attributed yet; JIT compiles and module
  instantiation still running at 700 s are the first suspect.
- So the JIT does not beat the interpreter, and HK defaults to the interpreter.
  - `tools/unity-dotnet/page/hk-page.sh` passes `GEXMONO_WASM_JIT=${JIT:-0}`.
  - `tests/games/unity-edger-gate.sh` runs its hk leg with `HK_JIT` (default 0).
- **Caveat:** p31 predates both provider fixes (1846e66a, 352a2737). In p31, a provider call that
  returns while another thread collects can lose its result or have its object freed. Both runs ended
  normally (rc 0, no fatal errors in the logs), so the numbers stand. HK was not rerun on p33.

### Mono patches (`tools/unity-dotnet/mono/patches`, on r58Playz `wasm-jspi-jit-10.0.3` at 6812ea26)

| patch | what it fixes |
|---|---|
| 0001 wasm64 | Mono builds for wasm64 under gex: the provider's target, threads (`mono-threads-gex.c`), mmap and dl on gex, guest-root scanning in sgen, the stop of guest threads after Mono's own stop-the-world |
| 0002 wasm-jit-gex | the fork's wasm JIT inside the gex provider: modules compiled by the host, the JIT_CALL table, GCHandle in CoreLib |
| 0003 gex-thread-diagnostics | stop-the-world diagnostics for threads that never reach a safepoint |
| 0004 negative-iconst | a negative OP_ICONST is an i64 local with `i64.const` (Hollow Knight's `Array.Clear` fault: 0xffffffff pointers) |
| 0005 interp-function-pointers | `ldftn`/`ldvirtftn` from JIT code give the interpreter's InterpMethod (tk2dSpriteAnimator.Start NULL method) |
| 0006 compute-reachable-worklist | upstream a0f2c7b9c: `compute_reachable` as a worklist (a Worker's stack overflowed) |
| 0007 sgen-cache-major-used-size | `mono_gc_get_used_size` cached between collections (HK asks every frame: 3-4.5% of the main Worker) |
| 0008 checked-arithmetic | ADDCC/SUBCC/MUL_OVF and unsigned-to-double (List.AddRange, InputStateBlock.ReadFloat) |
| 0009 direct-guest-native-calls | JIT code calls UnityPlayer's InternalCalls and P/Invokes directly (13.2M interpreter legs in hg10) |
| 0010 lazy-interp-leg-cost | a lazy stub caches its call shape and InterpMethod instead of deriving them per call |
| 0011 ldarga-reference-args | ldarga of a reference or byref argument (constrained calls on generic T) |
| 0012 split-nested-finally-leave | one continuation block per finally on a leave out of several |
| 0013 reflection-callers-and-stack-walks | callers of System.Reflection compile unless a Reflection.Emit image exists (PlayMaker LoadActionField) |
| 0014 call-handler-continuation-blocks | a continuation block for every CALL_HANDLER; bail reasons per gate |
| 0015 ldarga-in-eh-methods | ldarga in methods with EH clauses |
| 0016 localloc-and-checked-long-multiply | stackalloc and 64-bit checked multiply |
| 0017 vtype-call-args-keep-their-variable | a by-address vtype argument keeps INDIRECT through alias analysis |
| 0018 direct-runtime-icalls-and-string-ctors | direct calls to the runtime's own InternalCalls (handles included) and the string ctors (3.8M "icall-handles" legs in hg10) |
| 0019 lcse-reuse-widens, invalid-is-final | a reused 32-bit load widened into an i64 register; a module that fails validation is never retried |
| 0020 wasm64-fast-paths | INLINE_ALLOC, T2, FAST_TLS, EH_REC at pointer width; the provider's knob off-list removed |
| 0021 call-results-take-the-register-width | call results stored at the destination register's width (UTF8Encoding.GetChars at threshold 10) |
| 0022 integer-ops-at-register-width | INEG/INOT/LNOT/LNEG/LDIV, conversions, switch index and SAT_CONV at the register's width (InputStateBlock.ReadFloat, 597k legs in hg25); V8's "Compiling function" text counts as final |

Outside the Mono patches, the provider ABI fixes above (1846e66a, 352a2737) are in
`tools/unity-dotnet/provider`. Provider p33 is 0001-0022 plus both of them. The q81 runs used p31,
which predates them.

### Interpreter legs by reason, hg10 -> hg25

Legs are calls from JIT code that ran on the interpreter instead, over a 12-minute King's Pass run
(`MONO_WASM_JIT_STATS=1`, `iroute kinds`).

| kind | hg10 (0001-0006) | hg17 (p9: 0005-0008) | hg25 (p29: 0001-0020) |
|---|---|---|---|
| managed callee not compiled | 5,615,739 | 1,637,578 | 896,090 |
| guest icall / P/Invoke | 13,229,779 | 15,036,093 | 86,073 |
| runtime icall with handles | 3,820,554 | 4,679,486 | 0 |
| total | 22.67M | 21.36M | 1.02M |

There is no q81 column. q81's counter run (hg27, p31, JIT with `MONO_WASM_JIT_STATS=1` and
`VERBOSE=1`) died at 23 s in start-up: SIGSEGV, a wild branch from libc +0x67042 into the provider's
code page +0x2. The rerun on p33 (hg28) ran the full 12 minutes, rc 0, about 14 presents/s at
632-693 s with the counters on, so the crash did not reproduce. The page log, however, does not keep
the `iroute` tables (printf to stdout), so the counts after 0021/0022 are not measured.

hg25's managed legs include InputStateBlock.ReadFloat (597k; fixed by 0022). What is left after it
(hg25's top callees):

- InputManager.UpdateWithAxes (63k) and UpdateMouse (13k): the "vcall2 delegate" gate.
- Canvas.WillRenderCanvases.Invoke (25k).
- Reflection paths that throw (24k).
- ContactFilter2D.CheckConsistency (85k): an instance InternalCall on a struct. 0009's gate
  (`sig->hasthis && (pinvoke || valuetype)`) refuses it, because the `this` of a struct
  icall is passed by address.

### Building

On gooner, from the worktree, after `source scratch/env.sh`:

```
# the fork at 6812ea26 with patches/ applied, into OUT/src (once, or after a new patch)
tools/unity-dotnet/mono/build.sh OUT src
# runtime and provider (the provider step relinks mono.wasm; "runtime" alone does not)
nice -n 10 systemd-run --user --scope -q -p TasksMax=4096 env PATH=$PATH \
  tools/unity-dotnet/mono/build.sh OUT runtime provider
# -> OUT/provider/mono.wasm, OUT/lib/System.Private.CoreLib.dll
cargo build --release -p gexc --features gexc/x86 --target-dir target/x86
```

### Tests

```
nice -n 10 systemd-run --user --scope -q -p TasksMax=256 env PATH=$PATH tests/unity-dotnet/embed/run.sh OUT
rm -rf tests/unity-dotnet/smoke/obj
# one JIT leg with extra knobs (PROV=, THR= threshold): tests/unity-dotnet/embed/jitknob.sh NAME K=V...
# 100 JIT legs at thresholds 1, 2 and 10, 4 at a time: tests/unity-dotnet/embed/repeat.sh
```

### Hollow Knight and Muck in the page

Edger only, through the bench lock (`scratch/bench/bench.sh`, `BENCH_TIMEOUT` bounds the run inside
the lock). The scripts are in `tools/unity-dotnet/page`. They run the checkout they are in (or
`TREE=`), and the staging is in `~/gex-sandbox/scratch/unity/page`.

- **Guest GL:** the x86-64 guest GL comes from the tree's own sources (`guest-gl.sh TREE`, into
  `TREE/target/guest-gl-x64`, rebuilt when `guest/gl` or `crates/gex-gl/src/proto.rs` changes; `GL64=`
  overrides). Run `guest-gl.sh` on the build host before syncing the tree to a bench box. The old default,
  the prebuilt `scratch/unity/gl64-new` (2026-10-07), lacked the frame-query cache and the async present,
  and halved page Muck: 13.5 against 23.5 presents/s on the same tree (edger, 2026-10-11).
- **Function JIT:** `muck-page.sh` passes `--fjit` itself (`FJIT=0` for without; 10.1 against 13.2 presents/s
  without it).

- **BCL directory** (`page/muck-bcl-NAME`): copy `muck-bcl`, which holds .NET 10's shared framework
  assemblies and the native `libSystem.*` libraries. Then put this build's `OUT/lib/System.Private.CoreLib.dll`
  and `OUT/provider/mono.wasm` into the copy. CoreLib must match the build, because 0002 patches GCHandle.
- **Muck:** `bash tools/unity-dotnet/page/muck-page.sh NAME MINUTES`. This needs
  `muck-files.txt`, the host files from a native `--strace` run.
- **Hollow Knight:**
  - Stage it once with `hk-stage.py STRACE_RUN page/hk`. This writes `hk/game` (the 115 files HK opens)
    and `hk/files.txt`.
  - Then run, as `hk-jit-vs-interp.sh` does:

```
U=~/gex-sandbox/scratch/unity/units/cgen-page-m1
LIBS=libc.so.6,libX11.so.6,libdbus-1.so.3,libudev.so.1,libasound.so.2,libcrypto.so.3,libpulsecommon-17.0.so,libpulse.so.0,libm.so.6,libxcb.so.1,libpulse-simple.so.0,libXrandr.so.2,libXi.so.6,libsystemd.so.0
Q="cgen=1&elfunits=UnityPlayer.so,$LIBS&pins=UnityPlayer.so@11752000%2B2674000&fjitlibs=byobj=1"
BENCH_TIMEOUT=50m scratch/bench/bench.sh -- env GEX_CGEN_CACHE=$U GL64=~/gex-sandbox/scratch/unity/gl64q2 \
  BCL=~/gex-sandbox/scratch/unity/page/muck-bcl-NAME \
  bash tools/unity-dotnet/page/hk-page.sh NAME 12 --fjit --query "$Q" \
  --lazy "$HOME/gex-sandbox/games-linux/Hollow Knight" --input tests/games/hollow-knight-page-input.txt
```

  - This runs the interpreter, HK's default. `JIT=1` runs the JIT, and `PAGE_ENV=K=V,...` adds
    guest environment. Add `--profile 700,20,/worker-` for a main-Worker profile.
  - `hkpres.sh LOG` gives presents/s over 602-692 s. `hkbuckets.py SHOTDIR` gives managed time at
    equal frames.

### Open items

- **The JIT's non-managed overhead in Hollow Knight (top item).** q81, provider p31:
  - presents/s: JIT 13.78, interpreter 15.62;
  - managed time per present: JIT 14.3 ms, interpreter 15.6 ms;
  - main-Worker time per present: JIT 71.5 ms, interpreter 61.2 ms.

  The JIT already wins on managed code. It loses about 10 ms per frame outside it, in the "other"
  bucket and in UnityPlayer time. The suspect is compiles still running on the main Worker (module
  compilation and instantiation, tier-2 recompiles) at 700 s. A profile split by those frames
  (`mono_wasm_jit_*` compile and instantiate under the host's wasm compile) would settle it. Once that
  is fixed, the JIT should beat the interpreter on HK, and the default can flip back.
- About 300k interpreter legs a run are left: the vcall2-delegate gate, WillRenderCanvases, throwing
  reflection paths, and ContactFilter2D (the struct-instance icall, above).
- **62b38fc2** (`MemoryMap::reserve`, pins armed per thread) hung HK in the page (hp30, hp31). It is
  reverted (42f09b39). The hang itself is not understood.
- Tier-1's safepoint slow path writes cached guest registers back only inside fused loops. A thread
  parked there for a provider's collection can leave a stale RSP and stale registers in its context.
  The worker bug did not need this fix, so it is not applied. Writing back at every slow path
  (`flush_conditional` unconditionally) is correct, but it adds code to every safepoint. It is left
  for a decision on code size.
- **Rebase onto r58Playz's tip** (6812ea26..2f86e2c9, see "Upstream review" above): the decision is
  still with the user.
