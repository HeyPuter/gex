# The NT layer: Windows programs on gex

Stream S12 of the pewasm → gex merge (`merge-plan.md` §1, §6 M6). Written
2026-09-30. This is the design; `nt-layer-scope.md` (S8) is the measurement it
builds on. Status of each part is at the end.

**The decision being implemented.** A Windows program runs on gex with Wine
11.15's own PE DLLs (`ntdll.dll`, `kernelbase.dll`, `kernel32.dll`, `msvcrt`,
`ucrtbase`, `user32`, `win32u.dll`, …) unchanged in guest memory, translated by
gex like any other guest code. Everything Wine normally does on the Unix side
of the syscall boundary (unix `ntdll.so`, `win32u.so`, `wineserver`,
`winewayland.drv`) is implemented natively in `gex-kernel/src/nt/`. There is no
Linux Wine anywhere in the process, and no Linux syscalls are issued by a
Windows process.

---

## 1. Process model

### 1.1 What unix ntdll normally does before the PE side runs

Wine's `start_main_thread` (`dlls/ntdll/unix/loader.c`) and what gex does
instead:

| Wine unix side | gex (`nt::boot`) |
| --- | --- |
| `virtual_alloc_first_teb`: reserves `KUSER_SHARED_DATA` at 0x7ffe0000, a 32-block TEB area near 2 GiB, the PEB in the last block | the same objects, at deterministic addresses from the NT allocator (§1.2) |
| `server_init_process`: connects to wineserver, gets pid/tid, session | pid/tid from gex's process table; no server |
| `virtual_map_user_shared_data`, `init_cpu_info` | fill `KUSER_SHARED_DATA` with Wine 11.15's layout (§4) |
| `init_startup_info` → `init_peb`: `RTL_USER_PROCESS_PARAMETERS` (image path, command line, cwd, `DllPath`, environment block, std handles), `peb->ImageBaseAddress`, OS version 10.0.19045, subsystem fields, `load_global_options` | the same fields, built in Rust from `gexc run`'s argv/env; the environment gets `WINEPREFIX`-free Windows variables (`SystemRoot`, `windir`, `PATH=C:\windows\system32;…`, `TEMP`, `USERPROFILE`, …) |
| map the main exe (`virtual_map_image`) | `nt::section::map_image` (gex-ld `pe::parse` + `PeImage::map`) |
| `init_thread_stack`: stack reserve/commit from the exe's headers, `Tib.StackBase/StackLimit/DeallocationStack` | the same |
| `load_ntdll`: map `ntdll.dll`, then `load_ntdll_functions` writes `__wine_syscall_dispatcher`, `__wine_unix_call_dispatcher` and `__wine_unixlib_handle` into ntdll's data | map it the same way and write those three exports: the syscall dispatcher is a gex trampoline (used by 32-bit code only, §2.3), the unix-call dispatcher is a gex trampoline (§6), the unixlib handle is a gex-chosen token |
| `load_apiset_dll`: map `apisetschema.dll`'s `.apiset` section, `peb->ApiSetMap` | the same (a read-only file view) |
| `server_init_process_done` → `signal_start_thread(TransferAddress, peb, teb)` | §1.3 |

Everything after that is PE code: `LdrInitializeThunk` → `loader_init` loads
`kernel32.dll` and the exe's import closure itself, through `NtOpenFile`,
`NtCreateSection(SEC_IMAGE)` and `NtMapViewOfSection`. So **Wine's PE ntdll is
the loader**. gex-ld's PE code (S5) is used by the kernel to *map one image*
(parse, place, relocate), not to bind imports.

### 1.2 Memory layout

Guest virtual address = linear-memory offset, as for every gex process. Code
must sit below `MMAP_TOP` (2 GiB) so the dispatch directory can name it. The
layout, the same for PE32 and PE32+ processes:

| range | what |
| --- | --- |
| `0x0000_0000–0x0000_1000` | unmapped (null) |
| `0x0000_1000–0x0010_0000` | gex thread contexts (runtime, as for Linux) |
| `0x0010_0000–0x0030_0000` | dispatch directory (runtime) |
| `0x0030_0000–0x003e_0000` | **host-call slots**: gex-ld's placeholder range and format (16-byte entries). A jump to an entry reaches the dispatch-miss path; the NT layer routes it to a registered native function (a per-DLL redirect) or fails loudly with the entry's `DLL!name`. |
| `0x003e_0000–0x003f_0000` | NT kernel data, runtime-owned: per-thread wait words (§3.3) |
| `0x003f_0000–0x0040_0000` | **NT trampolines**: a page of real x86 code gex writes at boot, mapped read+execute (the unix-call dispatcher, the i386 syscall dispatcher, the callback-return stub) |
| `0x0040_0000–0x7fff_0000` | the NT allocator's range: images, `NtAllocateVirtualMemory`, stacks, TEBs, PEB, heaps (the top is Windows' `MmHighestUserAddress + 1`) |
| `0x7ffe_0000–0x7ffe_1000` | `KUSER_SHARED_DATA` (fixed by the Windows ABI, carved out of the range above) |
| `0x8000_0000–0xffff_0000` | natively unused (later: data allocations of `LARGEADDRESSAWARE` programs); in the browser, the shim's heap (§10.2) |

In the browser the shim shares this address space with the guest. Its stack
and data are at fixed addresses (`0x0580_0000`–`__heap_base`) and its heap
has taken an arena span or two by the time a program loads, so boot reserves
`STACK_TOP - STACK_SIZE` (0x0500_0000) up to the heap's cursor as a
`[gex-shim]` region the allocator never hands out, grows the memory to 2 GiB
at once (KUSER_SHARED_DATA and the top-down PEB/TEBs reach the top anyway),
and moves the heap's cursor to 0x8000_0000 with its ceiling at the top of
the memory. Noita's exe (0x40_0000, 13 MiB) is well below the reservation.

**The allocator** is bottom-up first fit at 64 KiB granularity, as Windows'
`NtAllocateVirtualMemory(NULL)` is, and as gex wants: in the browser linear
memory is allocated up to the highest address in use, so low and dense is
cheap. `MEM_TOP_DOWN` is honoured (top-down from 2 GiB). The allocator is
deterministic: the same program with the same inputs gets the same addresses,
which keeps translations cacheable per (content, address).

**Image placement (the open S5 question): pack, don't honour high bases.**
- Every x64 Wine DLL prefers a base above 4 GiB (ntdll 0x1_7000_0000, the rest
  at 0x1_8000_0000), and every MinGW/MSVC x64 exe prefers 0x1_4000_0000. They
  have to move anyway.
- i386 Wine DLLs prefer 0x7b80_0000–0x7bc0_0000 and app DLLs 0x1000_0000.
  Honouring those makes every process's memory reach ~2 GiB in the browser.
- So a **relocatable image is placed by the allocator** (first fit), exactly
  like `PeLayout::pack`. An image that cannot move (`RELOCS_STRIPPED`, or no
  `.reloc` without `DYNAMIC_BASE`) is mapped at its preferred base, or the map
  fails with `STATUS_CONFLICTING_ADDRESSES`, as Windows does. PE32 exes linked
  at 0x40_0000 without relocations (common, and Noita's case) land at their
  base, which is the bottom of the allocator's range.
- `NtMapViewOfSection` returns `STATUS_IMAGE_NOT_AT_BASE` for a moved image,
  exactly as Wine does; the PE loader already handles that code.
- `gexc pe-info`/`closure` model the same thing: gex-ld's
  `PeLayout::default()` packs (`pack: true`, the coordinator's decision);
  `--no-pack` honours preferred bases that fit.

### 1.3 The initial thread, and entering PE ntdll

Wine's `init_syscall_frame` builds a `CONTEXT` on the new thread's stack with
`Rcx = entry`, `Rdx = arg`, `Rsp = StackBase - 0x28`,
`Rip = RtlUserThreadStart`, then "returns from the syscall" into
`LdrInitializeThunk(context)` with `rsp = context - 8`. gex does the same in
Rust: the `CONTEXT` is written into guest memory, the gex register context is
set to `rip = LdrInitializeThunk`, `rcx = &context`, `rsp = &context - 8`,
`GS base = TEB` (x64; `FS base = TEB` for i386), MXCSR 0x1f80, x87 CW 0x27f.
For the main thread `entry` is the exe's entry point (`TransferAddress`) and
`arg` is the PEB. `LdrInitializeThunk` runs the loader, then `NtContinue`s into
`RtlUserThreadStart` → `BaseThreadInitThunk` → the program.

New threads (`NtCreateThreadEx`) are the same frame with the thread's own
start routine, on a TEB and stack the kernel allocates. They run as gex
threads (one host thread each natively, one Worker each in the browser), using
the shim's existing thread-spawn path.

**TEB/PEB.** Wine's layouts (`include/winternl.h`), not Windows'. For a
PE32+ process: `TEB64` at the block start, `TEB32` at +0x2000 (Wine sets both
up), `WowTebOffset = 0`. For a PE32 process: a pure 32-bit process, a
`TEB32`/`PEB32` only, `WowTebOffset = 0`. gex implements the 32-bit syscall
ABI directly (§7), so a PE32 program sees a 32-bit Windows, not WoW64:
`IsWow64Process` is false.

---

## 2. How Windows syscalls reach gex-kernel

### 2.1 x86-64

Wine 11's PE `ntdll.dll` and `win32u.dll` syscall stubs (`include/wine/asm.h`,
`__ASM_SYSCALL_FUNC`):

```
4c 8b d1              mov  r10, rcx
b8 <id>               mov  eax, id
f6 04 25 08 03 fe 7f 01  test byte [0x7ffe0308], 1   ; USD.SystemCall
75 03                 jne  1f
0f 05                 syscall
c3                    ret
1: …                  call [0x7ffe1000]            ; Wine's Linux path
```

On Linux Wine sets `SystemCall = 1`, because a real `syscall` would reach the
Linux kernel. **gex leaves it 0**, so the stub executes `syscall`. The x86
lifter already turns `syscall` into `Term::Syscall`, which calls the host's
`gex.syscall` import. No new instruction support, no trap, no trampoline: an
NT syscall costs what a Linux syscall costs in gex.

Arguments: `r10, rdx, r8, r9`, then `[rsp+0x28]`, `[rsp+0x30]`, … (the stub
was `call`ed, so `[rsp]` is the return address and `[rsp+8..0x28]` the home
space). Result in `rax`. `rcx`/`r11` are clobbered by the instruction, as on
hardware.

### 2.2 Telling an NT syscall from a Linux one

**By process personality, not by number.** A process is created as Linux or
NT (`gexc run` looks at the file: ELF or PE) and stays that way; Windows has no
`exec` into ELF. The shim's host state carries `nt: Option<Arc<nt::Process>>`.
In an NT process every `syscall` is an NT call:

| `eax` | table |
| --- | --- |
| `0x0000–0x0fff` | ntdll (`ALL_SYSCALLS64` / `ALL_SYSCALLS32` from Wine 11.15's `ntsyscalls.h`) |
| `0x1000–0x1fff` | win32u (`win32syscalls.h`) |
| `0x7f00_0000+` | gex-private calls issued only by gex's own trampolines (the unix-call dispatcher, §6) |

Anything else, or a number whose name gex knows but has not implemented, is
reported on stderr with its name (`gex-nt: unimplemented NtFooBar
(ntdll 0x00ab)`) and returns `STATUS_NOT_IMPLEMENTED`; `GEX_NT_STRICT=1` kills
the process instead. The syscall tables are generated from Wine's headers by
`tools/nt-gen-sysno.sh` and committed as `nt/sysno.rs`, pinned to 11.15.

### 2.3 Where the dispatch runs: outside the kernel lock

The Linux personality runs every syscall inside `Mutex<Kernel>` and expresses
blocking as a `BlockOp` the host performs unlocked. The NT layer is entered
differently:

- The shim's `syscall` import sees `nt.is_some()` and calls
  `gex_kernel::nt::syscall(&mut dyn NtHost, pc)` **without** holding the kernel
  lock.
- `NtHost` is a small trait the shim implements: guest memory, `with_kernel`
  (take the Linux kernel's lock for VFS and the memory map), wait on a futex
  word, spawn a thread, and **run a nested guest call** (§5).
- NT state that is hot or needs waits (`nt::Process`: handle table, objects,
  threads) has its own locks. VFS and memory-map work takes the kernel lock
  for exactly as long as it needs.

This keeps the hot calls off every lock that doesn't concern them:
`NtQueryPerformanceCounter` is a clock read, and the futex pair touches only the
target thread's wait word. It also lets a handler block, or call back into the
guest, in the middle of its work, which win32u needs (§5).

---

## 3. Objects, handles and waits

### 3.1 Objects and handles

- `nt::object`: typed objects (`File`, `Directory`, `Section`, `Event`,
  `Mutant`, `Semaphore`, `Thread`, `Process`, `Key`, `Token`, `KeyedEvent`,
  later `Desktop`, `WinStation`, …) behind `Arc`, with a per-process handle
  table (multiples of 4, as Windows) holding `(object, access, attributes)`.
- Pseudo-handles `-1` (process), `-2` (thread) are resolved at lookup.
  `NtDuplicateObject` with `DUPLICATE_SAME_ACCESS` and `CLOSE_SOURCE` is the
  hot one (Noita duplicates `GetCurrentThread()` in a loop), so duplicate is an
  `Arc` clone plus a slot.
- A small object namespace (`\BaseNamedObjects`, `\Sessions\1\…`, `\KnownDlls`
  empty, `\??` → DOS devices) for named events/mutexes/sections.
- Access masks are recorded and returned by `NtQueryObject`, not enforced
  beyond the obvious (e.g. writing a read-only file handle).

### 3.2 Files → VFS

- NT paths are translated to gex VFS paths: `\??\C:\…` → the staged prefix's
  `drive_c` (mounted into the guest namespace at boot), `\??\Z:\…` → `/`,
  `\??\unix\…` refused. Lookup is case-insensitive (a directory scan on a
  miss, cached per directory).
- A `File` object wraps a gex `FileHandle` plus the NT-side state: position,
  `FILE_SYNCHRONOUS_IO_*` options, delete-on-close, directory enumeration
  cursor.
- `NtReadFile`/`NtWriteFile` on synchronous handles call the VFS directly and
  complete inline (`IO_STATUS_BLOCK`, optional event set, APC queued). Console
  handles (`std{in,out,err}` from the params) are gex's fds 0/1/2.
- Information classes are added as programs ask for them; an unknown class
  fails loudly with its number.

### 3.3 Waits

Every NT thread owns a **wait word**, a `u32` in the NT kernel-data page
(§1.2) at `0x3e_0000 + 4 * slot`. It is guest memory, so the existing futex
machinery (`FutexTable`, `Atomics.wait` in the browser) can sleep on it
unchanged.

- **`NtWaitForAlertByThreadId` / `NtAlertThreadByThreadId`** (Wine's futex:
  SRW locks, condition variables, critical sections, `WaitOnAddress`; 33% of
  Noita's calls): alert = set the target's alert flag and wake its word; wait =
  sleep on the word unless the flag is already set, then clear it. No handle
  lookup, no object lock.
- **Dispatcher objects** (events, mutants, semaphores, threads, processes)
  keep a list of waiting threads. `NtWaitForSingleObject` /
  `NtWaitForMultipleObjects` (any and all): under the objects' lock, check the
  condition; if satisfied, consume (auto-reset, semaphore count, mutant
  ownership) and return; otherwise register on every object, drop the lock,
  sleep on the wait word with the deadline, and re-check when woken. Signalling
  an object wakes the words of its waiters. Wait-all is checked and consumed
  atomically under the one lock.
- **Alertable waits and APCs**: `NtQueueApcThread` appends to the target's APC
  queue and wakes its word; an alertable wait returns `STATUS_USER_APC` and the
  kernel redirects the thread through `KiUserApcDispatcher` with a `CONTEXT`
  frame (the same frame builder as exceptions).
- **`NtDelayExecution`**: a sleep on the wait word with a deadline (so an APC
  or thread termination interrupts it); `Sleep(0)` is a host yield.
- **Message queues** (win32u, later) are waitable objects in the same model,
  which is what `MsgWaitForMultipleObjectsEx` needs.
- Threads never spin in the host: every wait is a futex sleep, which is what
  makes it work in the browser (Workers block in `Atomics.wait`).

### 3.4 Memory

`NtAllocateVirtualMemory` / `NtFreeVirtualMemory` / `NtProtectVirtualMemory` /
`NtQueryVirtualMemory` keep a Windows-shaped region list in `nt::vm` —
allocation base, allocation protection, per-page state (free, reserved,
committed) and protection, and type (private, mapped, image) — which is what
`MEMORY_BASIC_INFORMATION` reports. It is mirrored into gex-kernel's
`MemoryMap` (`insert`/`protect`/`unmap`) so the translator's "is this
executable" check and `/proc`-style diagnostics see the same map. Committed
pages are zeroed on commit. Guard pages and access faults can't be enforced
(wasm has no page protection), so `PAGE_GUARD` is recorded and reported only.

Sections: `SEC_IMAGE` sections parse the file with gex-ld once and map
copies (the loader writes to images); `SEC_COMMIT` file sections map a copy of
the file's bytes; pagefile-backed sections are shared anonymous memory within
the process. Cross-process shared views wait for multi-process support.

Interlocked operations are wasm atomics, which need natural alignment; x86
does not. A misaligned `lock` access inside one aligned 8-byte word is a
compare-exchange loop on that word (atomic); one spanning two words runs under
the global `gex_abi::SPLIT_LOCK` word, atomic against every other such access
but not against a plain access racing it (the first is reported).
`cmpxchg8b` is one 64-bit compare-exchange. `cmpxchg16b` (x64's
`SLIST_HEADER`) is done by the host (`gex_kernel::x86::serve_cmpxchg16b`,
reached through `sysreg_write`), under one lock every `cmpxchg16b` takes:
atomic among them, which is all an SList needs, not against a plain store
racing one. tests/nt/src/mtx.c runs on both arches.

gex has no page protection, and below `gex_abi::GUEST_FLOOR` (3 MiB, the end
of the dispatch directory) lie its own thread contexts and words, where
Windows has no pages at all (nothing NT maps goes below `vm::ALLOC_LOW`). A
write through a null pointer's field would land there silently. So every
x86 locked access (both tiers, any alignment) and `cmpxchg16b` compares its
address with the floor, and below it the host (`cache_op::LOW_ACCESS`)
raises STATUS_ACCESS_VIOLATION (a write, at the address) through the
exception dispatcher -- SIGSEGV for a Linux process. The CONTEXT has the
registers at the access but the block's start as its pc, so a handler that
resumes re-runs the block; unwinding to a caller's `__except` is exact.
The NT argument helpers (`out_ptr`, `iosb`, `ustr`, `obj_attr`, ...) return
STATUS_ACCESS_VIOLATION for such pointers. (The case that found it:
tests/nt/src/mtx.c passed msvcp120's `_Mtx_lock` the mutex rather than its
address -- before MSVCP 140 it takes `_Mtx_t *` -- so ConcRT locked a
critical_section at 0x9.) tests/nt/src/lownull.c and
tests/x86-programs/locked-null.c check it.

### 3.5 Threads

`NtCreateThreadEx` allocates a TEB block and a stack from the allocator, builds
the `LdrInitializeThunk` frame (§1.3), and returns a `SpawnThread` outcome to
the shim, which starts a gex thread at that context. `NtTerminateThread` /
thread return exits the host thread, signals the thread object, and frees the
TEB. `NtTerminateProcess(-1)` is the process exit, with its exit code
propagated to `gexc run`'s status. Suspend/resume counts are real for threads
created suspended (the only case the census shows).

---

## 4. Shared memory with Wine's exact layouts

All layouts come from Wine 11.15's headers, not from Windows documentation, and
are **generated**: `tools/nt-gen-layout.sh` compiles a C file against
`wine-11.15/include` with MinGW (x86-64 and i686) and records `offsetof`/
`sizeof` for every field gex touches; the output is committed as
`nt/layout.rs`. A different Wine version needs a regeneration, and the pinned
manifest (§8) says which version the DLLs are.

- **`KUSER_SHARED_DATA`** at 0x7ffe0000 (`include/ddk/wdm.h`): NT version
  10.0.19045, `NtProductType`, `ProcessorFeatures[]` matching what gex's CPUID
  advertises (SSE–SSE4.2, no AVX, no XSAVE), `NumberOfPhysicalPages`,
  `TickCountMultiplier`, `ActiveProcessorCount`, `SystemCall = 0`, the
  time zone bias, `QpcFrequency` (gex's clock is nanoseconds, so 10 MHz,
  Windows' usual). The time fields (`InterruptTime`, `SystemTime`,
  `TickCount`, `TickCountQuad`) are updated by a host ticker (1 ms natively,
  the Worker's timer in the browser) with Wine's high-part/low-part/high2
  protocol, so a guest spinning on `GetTickCount` without syscalls still sees
  time pass.
  - *Native* (`shims/native/src/nt.rs`): a host thread calls
    `nt::sysinfo::usd_tick` every millisecond until the process ends.
  - *Browser* (not built yet): the same `usd_tick`, compiled into the
    kernel's wasm and exported, is called from a timer in the process's
    coordinating Worker (`setInterval`, which a Worker clamps to ~4 ms), and
    also at the entry of every NT syscall, so a program that makes syscalls
    always reads fresh time. A guest spinning without syscalls sees time
    move in steps of 4 ms or more, which is still finer than Windows' own
    15.6 ms tick. The writes are plain stores into the `SharedArrayBuffer`.
    The high2/low/high1 store order is what lets a reader detect a torn
    read, exactly as on Windows.
- **PEB / TEB / `RTL_USER_PROCESS_PARAMETERS`**, both pointer sizes (§1.3).
- **Later, for win32u:** `peb->GdiSharedHandleTable` (`GDI_SHARED_MEMORY`),
  per-DC `DC_ATTR`, `peb->KernelCallbackTable` (read, not written), and
  win32u's session shared memory for windows/queues/desktops, which Wine 11's
  PE `user32` reads directly. These live in guest memory the kernel owns and
  updates under the win32u state lock.

---

## 5. The nested guest loop: win32u → user32 callbacks

Decision (merge plan §1): a **nested guest loop**.

**The protocol Wine uses (x86-64).** Unix `KeUserModeCallback(id, args, len,
&ret, &ret_len)` pushes a frame on the *user* stack and resumes the thread at
PE `KiUserCallbackDispatcher` with `[rsp+0x20] = args`, `[rsp+0x28] = len`,
`[rsp+0x2c] = id`. That calls `peb->KernelCallbackTable[id](args, len)` in
user32, which (directly or through the dispatcher's tail) issues
`NtCallbackReturn(ret_ptr, ret_len, status)`. That syscall does not return to
its caller: it resumes the unix code that was waiting in `KeUserModeCallback`.
i386: `KiUserCallbackDispatcher(id, args, len)` is a stdcall function the frame
calls with a fake return address.

**gex.** A win32u handler (Rust, running outside the kernel lock, §2.3) calls
`host.user_callback(id, &args) -> (status, Vec<u8>)`:

1. The host snapshots the thread's whole gex context (registers, flags, x87,
   SSE: `ctx::SIZE` bytes).
2. The kernel writes the callback frame and the argument block below the
   guest `rsp` (aligned, below the red zone Windows doesn't have), and sets
   `rip = KiUserCallbackDispatcher`.
3. The host calls the translated code's `enter(ctx, rip)` **recursively**
   from inside the `gex.syscall` import. Wasmtime supports host→wasm
   re-entry; the browser shim does the same with a nested call of the exported
   `enter`. Guest calls made by the callback (including more syscalls, more
   callbacks, and exceptions) run normally on this nested loop.
4. When the guest issues `NtCallbackReturn`, the NT dispatcher copies the
   result out of guest memory, records it on the thread's callback stack, and
   unwinds the nested `enter` with the same trap gex uses to redirect
   execution (`StopExecution`), tagged "callback returned". The nested loop's
   driver sees the tag and returns the result to the handler.
5. The host restores the saved context, and the handler carries on. When the
   outer syscall finally returns, it writes `rax` into the restored context.

Callbacks nest to any depth: each level is one host frame plus one wasm
`enter`. The per-thread callback stack records the depth so that a stray
`NtCallbackReturn` with no callback outstanding fails with
`STATUS_NO_CALLBACK_ACTIVE`, as on Windows.

**Interaction with signals, exceptions and threads.**
- The nested loop is the same loop as the top-level one
  (`run_thread_loop`): a `resume_at` redirect (exception dispatch, APC
  delivery, a translation miss) restarts `enter` at the new pc *inside the
  nested level*; only the callback-return tag ends it. An exception raised in a
  window procedure is dispatched by PE ntdll on the nested level, and if a
  handler unwinds past `KiUserCallbackDispatcher` (Windows allows this only
  through `user_callback_handler`, which terminates), the level ends with an
  error status.
- Thread creation inside a callback works: the spawn request is serviced by
  the nested loop the same way.
- Linux signals don't exist in an NT process. APCs are delivered only at
  alertable waits and `NtTestAlert`, on whatever level the thread is.
- The nesting lives on the host (native) or Worker (browser) stack of the
  thread making the call. No other thread can observe it, because callbacks
  always run on the thread that issued the syscall; cross-thread
  `SendMessage` is a posted message plus a wait, handled by the receiver's
  own `GetMessage`/`PeekMessage` path, not a callback on the sender.
- Windows has no `fork`/`exec`, so the flat-stack invariant gex needs for those
  (merge plan §4) never matters here. Thread migration between Workers is not
  done for NT processes.

**The browser (§10.2).** The `syscall` import is the shim's `gex_syscall`,
called from translated code; the nested level is a JS import,
`gex_env.nt_callback(ctx, pc)`, which runs a synchronous copy of the run
loop (`Gex.#ntCallback`) on the same Worker: `enter(ctx, pc)`, and on each
trap the same housekeeping as the outer loop (a wanted slot filled, pending
translations compiled with `new WebAssembly.Module` and installed, the
Worker's tables caught up, a staged thread posted), then re-enter at
`gex_resume_pc`. It ends when `gex_nt_callback_done(ctx)` says the thread
holds a callback result. The shim's frames below the import are live, so
after a trap the loop puts `__stack_pointer` back to its value at entry,
never to the Worker's base. Nothing may yield: there is a wasm frame below.

**Blocking waits inside a callback.** An NT wait blocks the calling thread
where it is, natively and in the browser alike: `nt::sync` parks on the
process's `FutexTable` (a condition variable; in the shared-memory build
that is `memory.atomic.wait32` on the Worker), with the kernel lock *not*
held. A wait inside a window procedure is therefore an ordinary blocked
Worker, however deep the nesting. What that means:
- Another thread wakes it the same way at any depth: the waker's
  `notify` reaches the Worker's `wait32` directly, with no event loop
  involved.
- The blocked Worker's event loop does not turn, at the outer level or
  nested. Nothing an NT process needs may arrive on it: thread start and
  module sharing are posted *from* the Worker, and sibling translations
  arrive through the shared registry (caught up at the next trap). Display
  input must come the same way, through shared memory polled at a message
  peek (`wl_poll_input`, §6a), never as a message to the Worker. So a wait
  in a callback cannot deadlock on the page.
- A wait can never be satisfied by a callback on the *same* thread (a
  thread runs one level at a time), exactly as on Windows; cross-thread
  `SendMessage` is the receiver's to dispatch (above).
- A wait that needs a message (`MsgWaitForMultipleObjects`) is woken by the
  queue's wake word, which `PostMessage` bumps. Input from the page is only
  seen at the next peek, so it ends such a wait only through the wait's own
  timeout; games pump with `PeekMessage` every frame, so this costs nothing
  there.

**Cost.** One `enter` and one trap-unwind per callback, ~µs. The census shows
~2k callbacks in a Noita session, so this is noise; the function tier's
unwinding (merge plan §4.2) is the same mechanism.

---

## 6. `__wine_unix_call` and unixlibs

**The boundary.** A PE DLL with a Unix side (opengl32, ws2_32, bcrypt, …)
gets a `unixlib_handle_t` from `NtQueryVirtualMemory(MemoryWineUnixFuncs)` on
its own module, and calls `__wine_unix_call(handle, code, args)`, which jumps
through ntdll's exported `__wine_unix_call_dispatcher` pointer. ntdll's own
Unix calls (`wine_dbg_write`, `system_time_precise`, …) use the same pointer
with `__wine_unixlib_handle`.

**gex.** The dispatcher pointer is a trampoline in the NT trampoline page:

```
x86-64:  mov r10, rcx ; mov eax, GEX_UNIX_CALL ; syscall ; ret
i386:    mov eax, GEX_UNIX_CALL ; <32-bit syscall stub> ; ret 12
```

so a unix call is one ordinary gex syscall, with `(handle, code, args)` in the
argument registers. The handle `NtQueryVirtualMemory(MemoryWineUnixFuncs)`
returns encodes which DLL asked (`(dll id << 16) | 1`). The NT layer keeps a
table per DLL:

- **Implemented natively**: ntdll's own calls (`wine_dbg_write` → stderr,
  `system_time_precise`, `unwind_builtin_dll` → `STATUS_NOT_FOUND`, since no
  `.so` builtins exist).
- **Redirects, later** (M6 per-DLL redirects): `opengl32` gets a table whose
  entries are the WGL/GL functions Wine's `unix_opengl` exposes (the param
  structs are in Wine's generated `unixlib.h`), implemented over gex's GL
  device. Wine's PE `opengl32.dll` stays unchanged; only its unix side is
  replaced. This is the hot path for Noita, and it is a syscall per GL call,
  which is fine for now and is where a later fast path (a direct host import
  from translated code) plugs in without changing the DLL.
- **Unavailable**: a DLL gex has no Unix side for gets
  `STATUS_DLL_NOT_FOUND` from `MemoryWineUnixFuncs`. Wine's DLLs check this
  and degrade (winsock reports no network, bcrypt no providers), which is what
  the census wants for Noita's update check.
- **Anything else** fails loudly with the DLL name and the function index (and
  its name, from the per-DLL enum, where gex has it), returning
  `STATUS_NOT_IMPLEMENTED`.

The same host-call-slot mechanism (§1.2) is available for redirects at the
export level (replace `dsound!DirectSoundCreate8` with a native function)
without touching the DLL's file: the kernel rewrites the export in the mapped
image to point at a slot.

---

## 6a. How a window reaches the screen

A Windows program's top-level windows go to the compositor gex already has
for Linux programs (`gex-wayland`), as surfaces the NT layer publishes
itself. GL goes through opengl32's unixlib onto gex's existing GL device.

### 6a.1 Windows as compositor surfaces

- **The surface.** Each top-level window owns a *window surface*: a 32 bpp
  BGRA DIB in guest memory, allocated and owned by the NT layer (Wine's
  `window_surface`), sized to the window. GDI output to the window, `BitBlt`
  and `StretchDIBits`, `BeginPaint` and the non-client area are all drawn
  into it by the NT layer's GDI (§10 `win32u/gdi`). Child windows are clipped
  regions of their top-level window's surface, as in Wine.
- **Publishing.** A flush (end of `BeginPaint`/`EndPaint`, `UpdateWindow`,
  a GDI flush, or at the latest the next message wait) hands the compositor
  `(window id, address, stride, width, height, damage, title, position)`.
  That is exactly what the compositor's scene records already carry for a
  Wayland client's shm buffer: pixels are never copied, only described.
  - *Browser:* the NT layer writes those records into the process's block of
    the `wl-bridge` arena (`wl_publish`), as `shims/browser/src/wl.rs` does
    for Wayland clients and `shims/hostwl` does for Xwayland. The compositor
    worker uploads damaged rows from guest memory and draws decorations.
  - *Native:* the same records go to `gex_gpu::Compositor` or to PPM files
    (`--wayland-out`), which is what the tests compare.
  - Both hosts implement one small trait the NT layer calls
    (`NtDisplay::publish`, `NtDisplay::remove`); the NT layer knows nothing
    about Wayland.
- **Input.** The compositor's input ring for the process (motion, button,
  axis, key, modifiers, focus, configure, close, already hit-tested to a
  window) is drained by the NT layer at every message wait and peek. Events
  become hardware messages in the owning thread's queue: `WM_MOUSEMOVE`,
  `WM_xBUTTONDOWN`, `WM_KEYDOWN` with evdev→VK/scan code mapping on the fixed
  US layout, focus and activation, `WM_SIZE` from configure, `WM_CLOSE`
  from close. The key state tables (`GetKeyState`/`GetAsyncKeyState`)
  follow.
- **Why not a guest Wayland client.** winewayland.drv would run a Wayland
  client inside the process, with wineserver-shaped shared memory. The NT
  layer already owns the window tree and the pixels, so describing them to
  the compositor directly removes a protocol and a copy.

### 6a.2 GL: opengl32's unixlib onto gex-gl

Noita renders with desktop GL (2.1 compatibility, GLSL 110, fixed function,
client arrays) through opengl32, whose PE side calls `__wine_unix_call` for
every GL and WGL function (3,095 functions; each params struct starts with
`TEB *`, the result last). **Decision: implement opengl32's unixlib in the
NT layer by encoding each call into gex-gl's wire protocol** (`crates/gex-gl`),
the transport Linux guests' GL already uses, and let the existing device run
it:
- natively, `gex-gl`'s host executor on the host's GL;
- in the browser, `MesaGlDevice`: mesa-wink's `gex_wgpu` gallium driver, a
  separate emscripten module on WebGPU, presenting to its own canvas that the
  compositor shows for an `F_EXTERNAL` surface.

WGL maps onto the protocol's context and surface opcodes:
`get_pixel_formats` reports one or two RGBA8/D24S8 formats;
`wglCreateContextAttribsARB` is `CONTEXT_CREATE` (desktop API, compatibility
2.1 unless the attributes ask for more); `wglMakeContextCurrentARB` makes
it current and attaches the window's drawable (`SURFACE_ATTACH` with the
window's surface id, marking it external); `wglSwapBuffers` is `PRESENT`.
The per-function table (params layout per ABI → opcode, and, for functions
taking pointers, how much memory each pointer covers) is generated from
Wine's `opengl32/unixlib.h` and the generator behind `guest/gl`'s
`gl44-generated.inc`; the pointer sizes are the same knowledge `gexgl.c`
already encodes for Linux guests. Calls batch in the ring and are flushed at
sync points (a query, a map, `glFinish`, a present), so most GL calls cost one
unix call and an encode.

**Rejected: Mesa in-process, as pewasm's `host_wgl.c` does.** That links
mesa-wink into the same module as the program, which worked because pewasm
was one emscripten module. In gex the kernel and shim are Rust
(`wasm32-unknown-unknown`) and Mesa is already a separate emscripten module
behind the gex-gl wire. Putting Mesa in-process would mean:
- a second copy of Mesa (tens of MB of wasm) in the shim;
- emscripten and JSPI inside the kernel's module;
- a second GL path to maintain beside the Linux one.

What it would buy is avoiding the encode/decode, and a later fast path gets
that anyway: a direct host import for GL calls from the unixlib trampoline
(§9) without changing the protocol. pewasm's `host_wgl.c` is still the
reference for WGL semantics (pixel formats, context attributes, swap
pacing) and for which client-array ranges Noita touches.

**Rejected: a win32u `opengl_driver_funcs` backend.** That interface lives
inside Wine's unix opengl32, which gex does not run.

## 7. The 32-bit NT ABI (PE32, Noita)

gex implements the i386 NT syscalls directly instead of running Wine's
`wow64.dll`/`wow64win.dll`/`wow64cpu.dll`, so a PE32 process contains only
i386 code (S4's i386 compatibility mode) and Wine's `i386-windows` DLLs.

- **Entry.** i386 PE ntdll's stubs are `mov eax, id; mov edx,
  __wine_syscall; call edx; ret $n`, where `__wine_syscall` jumps through
  ntdll's exported `__wine_syscall_dispatcher` variable (win32u copies ntdll's
  value at attach). gex points that variable at a trampoline that performs the
  gex syscall instruction i386 mode supports (the one S4 lifts for `int
  0x80`), with `eax` unchanged. The kernel reads the arguments from the stack:
  `[esp]` is the return into the stub, `[esp+4]` the return into the caller,
  `[esp+8…]` the stdcall arguments. The table is `ALL_SYSCALLS32`.
- **Thunks are layouts, not copies.** Handlers read and write guest structures
  through a pointer-size-aware accessor (`Abi::P32`/`Abi::P64`:
  `UNICODE_STRING`, `OBJECT_ATTRIBUTES`, `IO_STATUS_BLOCK`, `CONTEXT`, the
  information classes). One Rust implementation per call serves both ABIs;
  only the structure accessors differ. That is the bulk of wow64's 3.7k lines
  replaced by per-struct layout tables generated with the rest of `layout.rs`.
- **What differs beyond layout:** `CONTEXT` (i386 `CONTEXT` with
  `FloatSave` and `ExtendedRegisters`), the exception frame
  (`KiUserExceptionDispatcher(EXCEPTION_RECORD *, CONTEXT *)` with both on the
  stack), the callback frame (`KiUserCallbackDispatcher(id, args, len)`), the
  `FS` base as the TEB, `NtWaitForMultipleObjects32`, and the 32-bit variants
  of system information classes.
- **Until S4's i386 mode is on `merge/main`**, only x64 is built; the ABI
  accessor exists from the start so no handler assumes 64-bit pointers.

---

## 8. The vendored DLLs

`tools/nt-stage-wine.sh <rootfs>` copies Wine 11.15's PE DLLs and data files
from the host installation into a gex prefix:

- `<rootfs>/drive_c/windows/system32/` ← `/usr/lib/wine/x86_64-windows/*`
  (for PE32+ processes), `<rootfs>/drive_c/windows/syswow64/` ←
  `/usr/lib/wine/i386-windows/*`;
- `<rootfs>/nls/` ← `/usr/share/wine/nls/*.nls` (`NtInitializeNlsFiles`,
  `NtGetNlsSectionPtr`);
- nothing from `x86_64-unix/`.

It checks the host's Wine version and every file against the committed
manifest `tools/nt-wine-11.15.manifest` (path, size, SHA-256) and refuses on a
mismatch unless told to regenerate it. The DLLs are not committed. `gexc run`
finds the prefix with `--wine-prefix DIR` or `GEX_WINE_PREFIX` (default
`~/.cache/gex/wine-11.15`).

A PE32 process sees `syswow64` as its `system32` (the NT path mapping chooses
the directory by process bitness), because gex presents a 32-bit Windows to
it (§1.3).

---

## 9. Performance: the hot path, designed in

From the census (Noita): `NtQueryPerformanceCounter` 44%, the futex pair 33%,
`NtReadFile` 7%, `NtDelayExecution` 4%.

- The dispatcher is a `match` on the syscall number reached straight from the
  `gex.syscall` import, before any lock.
- QPC: a clock read and one guest store. No lock.
- Futex pair: the target thread's wait word by thread-id lookup (a
  read-mostly map), one futex wake; the waiter sleeps on its own word.
- `NtReadFile` on a synchronous file: handle lookup, then the VFS read under
  the kernel lock only for the read itself.
- Later, if measurement says so: route QPC and the unix-call path through a
  dedicated host import from the trampoline page, avoiding the generic syscall
  import's bookkeeping.

---

## 10. Module map and status

`crates/gex-kernel/src/nt/`:

| file | what |
| --- | --- |
| `mod.rs` | `Process`, `NtHost`, the dispatcher, loud failures |
| `sysno.rs` | generated syscall tables (ntdll, win32u; 64 and 32) |
| `layout.rs` | generated struct offsets |
| `abi.rs` | pointer-size-aware guest structure access |
| `boot.rs` | process creation (§1) |
| `vm.rs`, `section.rs` | memory and sections (§3.4) |
| `object.rs` | handles and the namespace (§3.1) |
| `file.rs` | files over the VFS (§3.2) |
| `sync.rs` | dispatcher objects and waits (§3.3) |
| `thread.rs` | threads, contexts, exceptions, APCs (§3.5) |
| `reg.rs` | the registry (in memory, seeded with the keys Wine's PE side reads) |
| `sysinfo.rs` | system information, time, locale, NLS |
| `unixcall.rs` | §6 |

### 10.1 Native host (`shims/native/src/nt.rs`)

The `nt` field on the host state, the branch in the `syscall` import, the
`NtHost` implementation (including the nested loop), the USD ticker thread,
and `run_nt_program`. `gexc run` sends a PE file there. The kernel is the
process's `Arc<Mutex<Kernel>>`; the NT layer takes it through
`nt::KernelCell`, implemented for `Mutex<Kernel>`.

For a hang: `GEX_NT_HANG_DUMP=S` has the ticker print every S seconds each
thread's last safepoint pc, last indirect branch and context registers, then
zero its safepoint counter, so a thread still looping stores its loop's pc
by the next report; a counter that stays 0 is a thread passing no
back-edge. (Without frames to count, the GEX_NT_HANG_SECS dump never fires.)
`tests/nt/stress.sh` runs a program again and again on two cores shared with
busy loops, natively or (`--browser`) under Node. That is how the last mtx
hang showed: under load, a stale block whose page check found a newer
translation already published (`PAGE_STALE`, not retranslated) dispatched
again through this thread's table, which had not installed it yet, and so
came straight back to itself -- every thread at one return address, the
safepoint counter frozen at 0. The host now catches up first (natively the
registry replay; in the browser a resume through JavaScript, which drains
other Workers' modules).

### 10.2 Browser host (`shims/browser/src/nt.rs`)

The same layer in a page, shared-memory build only (an NT wait blocks its
Worker, so a thread must be one). Hooks in `lib.rs`, each a line or two:
- `gex_load`: a PE is not placed; once the kernel is built, `nt::boot` splits
  the address space with the shim (§1.2), builds the `nt::Process` from the
  page's environment list (`GEX_WINE_PREFIX`, default `/wine`;
  `GEX_NT_TRACE`, `GEX_NT_STRICT`, `GEX_NT_TIME_SCALE`; every other entry
  overrides the Windows environment) and calls `nt::boot::boot`.
- `gex_syscall`: first thing, before the kernel is taken, an NT process's
  call goes to `nt::syscall`, which maps the `Outcome` onto the shim's resume
  slot and trap (a spawned thread is staged like a Linux `clone`, so the
  host gives it a Worker; the main thread's `ThreadExit` waits for the
  process, as natively).
- `gex_breakpoint`, `gex_illegal` and the dispatch miss's not-executable
  branch: `nt::fault` → `nt::hardware_exception`.
- `gex_safepoint` and every NT syscall (entry and exit): `nt::tick`, which
  updates KUSER_SHARED_DATA at most once a millisecond from a
  `performance.now()` clock. There is no thread to spare for a ticker; a
  guest that polls the tick count in a loop passes a safepoint.
- The NT layer's diagnostics go to the console through `nt::set_log`
  (`eprintln!` writes nowhere on wasm32-unknown-unknown).
- The display, when the page brought a compositor (`gex_set_wl`): `WlDisplay`
  publishes every visible top-level window as a toplevel record in this
  process's block of the scene arena (`wl_publish`, the same records
  `crate::wl` writes; client 1, surface = the window handle, XRGB8888 at the
  surface's guest address, which the compositor reads in place), and
  `poll_input` decodes this process's input ring (`wl_poll_input`, shared
  memory) into `NtDisplayEvent`s, which `message::peek` turns into hardware
  input. The compositor places and decorates the windows; coordinates are
  surface-local either way. A configure that does not change the size (the
  compositor sends one on every focus change) is dropped.
- `/dev/gex-gl` is the shim's `GlDevice`, the one Linux guests use: the
  opengl32 unixlib's submits go through `gl_submit` to the page's GL host
  (`MesaGlDevice` in the compositor's worker with `gl: 'compositor'`).

**One memory, many owners.** The guest, the shim's data and heap, every
Worker's shim stack and thread-locals, and the runtime's reserved words are
all in one linear memory with no protection between them, so a range handed
to two owners is silent corruption, and it has been, twice:

- `gex_worker_init` (each Worker's first call, which claims the Worker's
  stack) ran on wasm-ld's default stack, the top of the first 8 MiB --
  a Windows program's image. Every thread Noita started overwrote a
  kilobyte or two of noita.exe's .text below 0x800000. The glue now gives it
  one of 32 bootstrap slices below `SHIM_STACK_TOP`
  (`gex_abi::BOOT_STACK_COUNTER`); `tests/nt/src/lowmem.c` checks.
- **std's `System` allocator is a second allocator in the same memory.** On
  wasm32 it is dlmalloc, which grows the memory itself (`memory.grow`) and
  uses the new pages. std uses it behind the program's back: the per-thread
  list of thread-local destructors (any `thread_local!` whose type needs
  `Drop` registers into it on first use) and the `Thread` handle of
  `std::thread::current()` (the panic hook asks for it). The arena, which
  hands out heap spans, Worker slots and guest mappings, did not know: a
  `thread_local!` `VecDeque` made dlmalloc grow 0x82000000..0x82010000 for
  four Workers' destructor lists, and the heap later put a translation there,
  which the next Worker to register a destructor wrote into. The arena now
  serializes its own growth, records any growth it did not do as `Foreign`
  and steps over it (`OWN_TOP`, `memmap::skip_foreign`); the first is said
  out loud ("grown outside the arena"), counted in `GEX_NT_STATS`
  (`foreign=`), and `GEX_NT_FOREIGN_TRACE` looks around every NT call so the
  first one is caught near whoever did it. Still: **no `thread_local!` with a
  `Drop` type in the shim** -- it works now, but it is std allocating where
  nothing else does.

`shims/browser/src/memmap.rs` writes the ownership down: the fixed layout,
each heap span, each Worker's stack and thread-locals, the arena's guest
mappings, the Windows address space and foreign growth, with owners. An
overlap between owners (guest regions may overlap each other) stops the run
naming both and printing the map; `GEX_NT_MAP` prints it at boot,
`memmap::owner_of` names the owner of an address in the fault reports, and
`GEX_NT_ARENA_SELFTEST` (in `tests/nt/run-browser.sh`) grows a page behind
the arena's back and checks the next span keeps out of it. A Windows guest's
accesses above 2 GiB, where the shim's heap is, are bounds-guarded to a fault
(`gex_xlate::set_guard_limit`; `GEX_NT_GUARD_2G=0` to compare), and each
Worker's stack has a guard at its bottom checked at every NT call.

The page is `web/nt.html` (`web/src/nt-page.js`): it fetches a tar holding a
prefix under `/wine` and the program, and runs it through `runProgram` with
the canvas as the compositor's output. `tests/nt/browser-gui.mjs` drives it in
headless Chromium (a vite build of the page, served cross-origin isolated,
and its own browser with software GL and WebGPU): `window.exe`'s window is
checked by pixels, a click and Escape by what the program prints. The page
streams the bundle into a SharedArrayBuffer and hands `untar` the buffer, so
every file is a view of it and the start message to the process's Worker
shares it instead of copying it (a copied 1.8 GB clone fails with "Data
cannot be cloned, out of memory").

**Noita in the page (milestone c).** `node tests/nt/browser-noita.mjs
[--shot DIR]` bundles `~/src/noita-wasm/Noita` and the x86 prefix (1.8 GB),
opens the page with `gl=1&glbackend=wgpu`, plays the native recipe's clicks
through CDP (the release notes' Ok at frame 12, and at frame 90 "Nah. Destroy
it" for the autosave a prefix that has run gameplay offers; client
coordinates plus the window frame the compositor's surface includes), and
passes at frame 140, the main menu. First run, 2026-10-01: the menu after
~120 s in the page (load 13-29, software WebGPU via SwiftShader), about the
native time; `work/s12/noita-menu-browser.png`.

The GL host is `web/public/gex/gex-wgpu-replay.{js,wasm}`, Mesa's gex_wgpu
driver built from the fork's `wink` branch:
- Mesa: `third_party/mesa` at c6a33e6fbd8097432851ed2e938ad03e0895c5b0
  (mesa-wink `refs/heads/wink`). The superproject now tracks wink, at
  9016782 with S12's two gex_wgpu patches (228b0745).
- Build: `GEX_MESA_WINK_CACHE=<dir> JOBS=4 taskset -c 0-2,6-8 nice -n 10
  web/build-mesa-wasm.sh <out>`, emsdk 6.0.1 (`/usr/lib/emsdk`), the
  default emdawnwebgpu (v20250428.134257). When the Mesa checkout has
  `wink/lib/tint.sh`, the script builds Tint through the fork's own
  `wink/build.sh --variant=mt --until=tint` (Dawn cdda346c7ce9 with the
  fork's two patches and SPIRV-Tools/Headers 33e0256/2a611a9), so the pin
  lives in the fork; `GEX_MESA_TINT` still names a prebuilt one, and a fork
  without that script still gets web/tint/build-tint.sh.
- Fork-side issues, for the user (not worked around in the fork):
  wink/build.sh wants emsdk 3.1.56, so the build passes
  `WINK_ALLOW_ANY_EMSDK=1` and, for the newer clang,
  `EMCC_CFLAGS="-Wno-c2y-extensions -Wno-deprecated-pragma
  -Wno-lifetime-safety-intra-tu-suggestions"` (the flags
  web/tint/build-tint.sh already uses); and wink's own emdawnwebgpu
  (v20250822, patched) needs C++20 for `webgpu.cpp`, which the gex-wasm
  target does not set (the wink-wasm target does), so the gex build keeps
  the default emdawn.
- The fork's gex-wasm targets export `_gex_gl_setenv`, which
  web/mesa/gexgl-mesa.cpp now defines (from gex2's decoder).

**One kernel.** The process's `Kernel` is the one in the shim's `Host`, and
the NT layer reaches it only through `HostKernel`, whose `lock_kernel` takes
the shim's kernel lock (`KLock`) -- the lock `with_host` takes for every
other path: the dispatch-miss translator reading the memory map, the
scheduler and compositor pump, devices (`/dev/gex-gl`), signals and timers
(none in an NT process), the nested callback loop's exports. There is no
second copy of the kernel and no unlocked reference to it: the NT side's
own state is in `nt::Process` behind its own locks, as natively. The NT
layer never holds the kernel across a callback or a wait (natively that
would deadlock on the `Mutex`), so `KLock`'s per-Worker re-entrancy never
sees two live borrows.

Code that goes away (an unmapped image, a protection change -- LuaJIT
rewrites its machine code in place) loses its translations as natively: the
pages leave the shared page table and their dispatch-directory entries go
back to the miss stub, so the next branch from any Worker re-lifts what is
there now.

Milestones (from the S12 brief): **N1** x64 console hello through Wine's PE
ntdll/kernelbase/kernel32/msvcrt; **N2** threads, sync, SEH; **N3** the 32-bit
ABI (after S4); **N4+** USER, GDI, Noita's menu (after the coordinator's go).
Progress is in `MERGE-STATUS.md` under S12.
