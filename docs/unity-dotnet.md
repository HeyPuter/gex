# Unity's managed code on upstream .NET's wasm runtime

Stream U, 2026-10-04. Design and feasibility; nothing is built yet.

Direction (user, 2026-10-04): Unity games run their `Managed/*.dll` on
upstream .NET's wasm runtime, not by translating Mono's x86 JIT output.
UnityPlayer's `mono_*` calls are bound to that runtime through export
providers (merge-plan §9.9, eject-pe §5.1). The fjit writable-and-executable
work (addac37e) is reverted: with no x86 Mono, there is no JIT output to
translate.

## 1. Decisions

1. **Runtime: dotnet/runtime's Mono** (`src/mono`), interpreter first, with
   Mono's LLVM AOT of every managed assembly as the performance tier. It is
   pinned to **v10.0.10**, which matches the installed shared framework.
   CoreCLR is not the choice: its wasm port is a preview in .NET 11 and
   ships in .NET 12 (2027). It has no `mono_*` embedding API, and its GC is
   precise, while Unity's native code holds object pointers in x86
   registers and stacks. CoreCLR's browser ReadyToRun is worth another look
   once it ships.
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
4. **Toolchain: emscripten's clang and its `wasm64-emscripten` libc** (already in
   `/usr/lib/emsdk`'s sysroot), with the output linked as a **PIC side module**
   (`-sSIDE_MODULE -sMEMORY64`, `dylink.0`), which is the stateful-provider
   format. Emscripten's JS runtime is not used. The few imports libc makes
   (`__syscall_*`, clock and futex wait) are gex host imports, backed by
   gex's Linux syscall layer, which NT processes have too (Wine's unix
   side uses it). Threads are guest threads (§5), not emscripten's pthread
   Workers.
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

The interpreter is the bring-up tier. The jiterpreter is not used: it
generates wasm32 modules from JS at run time, which neither fits wasm64 nor
the "eject: no JIT" rule.

The performance tier is **Mono LLVM AOT** of CoreLib, the BCL, `UnityEngine.*`
and the game's assemblies:
- compiled at eject or build time to wasm64 (LLVM's wasm64 backend), the
  way Blazor's AOT compiles to wasm32;
- linked into the provider;
- with interpreter fallback for generic instances it did not see.

The `Managed` set is fixed per game, so it is all known at build time.

Native gexc runs the same provider on wasmtime. It is for correctness only.

## 7. Prototype plan

| step | what | done when |
|---|---|---|
| P1 | Upstream Mono at v10.0.10, built as a wasm64 side module with the interpreter, SGen in Boehm mode and the `gex` thread platform. The managed parts are built for linux-x64 from the same tag. | `mono_jit_init` + CoreLib load under a native test host (wasmtime, memory64) |
| P2 | Stateful providers for x86-64: SysV and Win64 adapters, M64 (today they are i386-only and never M64; mem64.md §10), TLS, `provider_bind`, guest-thread creation. Built on integ's `libprov.rs`. | tests in tests/x86-programs |
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
