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
       The interpreter's vcall/PIC/AOT-vcall caches pack 32-bit pointers
       into i64 words, so on 64-bit they are off and the inline PICs are
       not emitted (each vcall goes through the resolve helper); a 64-bit
       form of those caches is later work. embed with
       `MONO_WASM_JIT_THRESHOLD=1`: 431 methods JITted, 0 invalid modules,
       all results right (`scratch/unity/wj.sh [build] [THRESH]`). Methods
       with EH regions are refused until wasmtime's exceptions are wired
       (the emitter uses legacy try/catch with the C++ tag); localloc and
       LMF methods are refused as on wasm32. Next: Muck natively with
       --fjit and the JIT; speed measured on worldmachine (bench.sh) in a
       managed-heavy scene against interpreter-only. Assessment (2026-10-05): about
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
- **Resume (after the move; HEAD 1db5c816, `master` merged in):**
  0. The coordgate on this branch still lacks its browser NT step (it was
     stopped while E was timing), and nothing after 49a5773b has been
     gated. Run it before the libprov changes go to master.
  1. Build: `tools/unity-dotnet/mono/build.sh OUT`, then
     `tools/unity-dotnet/sysroot` via its `sysroot` step. musl comes from
     `MUSL_SRC`, which defaults to `../musl-wasm64` beside the worktree
     (remote `mw`, branch `gex-wasm64`).
  2. Find which lookup returns NULL. Candidates: `get_attr` on an
     attribute in UnityEngine.CoreModule, a field name Unity's Mono had
     that .NET's types lack, or a type whose lookup resolves through a
     facade's forwarders.
  3. Then on to the menu with graphics (the old `muck.sh` path: in-guest
     Xwayland on gex's compositor), and to Unity's own internal calls.
