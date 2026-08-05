# gex — AArch64 → WebAssembly AOT compiler & userspace Linux runtime

**Goal:** take an unmodified aarch64 Linux ELF binary (or a whole rootfs/container image),
ahead-of-time translate its machine code to WebAssembly, and run it — in a browser or any
wasm engine — against a userspace re-implementation of the Linux kernel ABI. Target
compatibility: "if it runs under gVisor, it should run under gex."

---

## 1. Scope

### In scope
- **Guest ISA:** AArch64 (A64, little-endian, EL0 userspace). ARMv8.0 baseline plus the
  extensions real distros assume: NEON/AdvSIMD, FP, CRC32, AES/SHA crypto, LSE atomics,
  FP16 (trap-and-emulate acceptable for rare extensions).
- **Binary formats:** ET_EXEC and ET_DYN (PIE) ELF, static and dynamically linked
  (glibc and musl), plus `dlopen` and guest programs that JIT (Node/V8, JVM, LuaJIT…).
- **OS surface:** the Linux syscall ABI for aarch64 (~300 syscalls, implemented in tiers),
  /proc + /dev + /sys synthetic filesystems, futex, signals, threads, fork/exec, sockets.
- **Hosts:** browsers (Workers + SharedArrayBuffer), wasmtime/wasmer/Node server-side.

### Out of scope (initially)
- AArch32/Thumb compat mode; big-endian.
- ptrace, perf events, eBPF, io_uring (return ENOSYS — glibc and most runtimes fall back).
- Full-fidelity `MAP_SHARED` file mappings (see §7.4).
- x86 guests (the architecture is ISA-parameterized, but aarch64 is the only frontend).

### Why AArch64 and not x86 as the guest
Fixed-width 4-byte aligned instructions make static discovery and re-translation radically
simpler than x86 (no instruction-boundary ambiguity). PC is not a GPR. The memory model is
weaker than wasm's atomics, so ordering is conservative-correct for free. And — decisive
for self-modifying code — the architecture *requires* explicit icache maintenance
(`IC IVAU` + `ISB`) after code writes, giving us a reliable, architecturally-guaranteed
hook for translation invalidation (§6.2). Every mainstream distro ships aarch64 images.

---

## 2. The six load-bearing decisions

1. **Hybrid AOT + runtime translation, one compiler.** Pure static translation cannot run
   `dlopen`ed code it hasn't seen, JITs, or `exec`. The translator is built as a library:
   the offline CLI (`gexc`) uses it for whole-image optimized AOT; the same library,
   itself compiled to wasm, ships inside the runtime to translate dynamically appearing
   code on the fly. AOT is the fast path and startup optimization; runtime translation is
   the completeness guarantee.

2. **Userspace kernel (Linux personality), not system emulation.** We implement syscalls,
   not a virtual MMU + devices + a real kernel image. Wasm has no page tables and no write
   traps, so system-mode emulation is forced into software page walks (container2wasm-style,
   ~100× slowdown). A syscall personality (gVisor/UML/WSL1 lineage) is the only design with
   an acceptable performance ceiling, and it's what "run any Linux *binary*" actually needs.

3. **Guest VA == linear memory offset, 4 GiB default.** Because *we* are the kernel, we
   control every address allocation (mmap, brk, stack, ELF load bias). Keep the whole guest
   address space inside one wasm32 linear memory (4 GiB max) so every guest load/store is a
   single bounds-checked wasm access with zero translation arithmetic. A memory64 build
   variant exists for guests that truly need > 4 GiB (accepting the ~10-20% bounds-check
   tax and reserving-vs-allocating differences). Non-PIE aarch64 binaries link at
   0x400000 — comfortably inside range; PIE we place wherever we like.

4. **Basic-block-per-function + tail calls as the baseline codegen.** Every translated
   block is a wasm function with one uniform signature; direct branches are `return_call`,
   indirect branches are `return_call_indirect` through a PC-indexed funcref table.
   No trampoline loop, no relooper required for correctness, wasm stack depth stays O(1)
   regardless of guest call depth, and any 4-byte-aligned address can become an entry
   point later without re-translating anything. Tier-2 then forms optimized superblock
   regions (§5.5) for performance. Tail calls are in Wasm 3.0 and shipped in all major
   engines — we take the dependency.

5. **Every guest thread is a real host thread.** Guest processes/threads run in Web
   Workers (browser) or host threads (native) over a shared-memory guest address space
   (SharedArrayBuffer). Syscalls are synchronous calls into the kernel library; blocking
   is `Atomics.wait`. No asyncify, no JSPI, no CPS transform of guest code. Browser
   embedding therefore requires cross-origin isolation (COOP/COEP) — an acceptable
   deployment constraint, and the only one that gives real futexes and real parallelism.

6. **gex is the dynamic loader.** The guest's `ld.so` never runs in the default mode:
   **gex-ld**, part of the runtime, loads the program, applies relocations, resolves
   symbols, sets up TLS, and serves the `dlopen` family itself. This flips dynamic
   linking from an obstacle into the strongest AOT lever we have — the full library
   closure (plus an explicitly declared dlopen bundle) is known at build time, so every
   object is pre-translated and pre-linked, and cross-DSO calls prebind to direct calls
   (§6.4). Running the real guest interpreter translated is retained as a per-process
   fallback (`--loader=guest`).

---

## 3. System overview

```
                       ┌────────────────────────── offline ──────────────────────────┐
  aarch64 ELF/rootfs ─▶│ gexc: ELF reader → decoder → lifter → GIR opt → wasm emit    │─▶ .gexm modules
                       │        (tier-2: CFG recovery, superblocks, reg-alloc)        │   + metadata
                       └──────────────────────────────────────────────────────────────┘   (content-addressed cache)

                       ┌────────────────────────── runtime ──────────────────────────┐
   .gexm modules  ────▶│  gex-ld ─ loads & links guest objects onto:                  │
                       │   • guest memory   (SAB, 4 GiB linear memory)                │
                       │   • dispatch table (funcref per guest instruction slot)      │
                       │   • gex-kernel     (syscalls, VFS, procs, signals, futex)    │
                       │   • gex-jit        (same compiler lib, wasm-compiled,        │
                       │                     translates mmap'd/JIT'd guest code)      │
                       └──────────────────────────────────────────────────────────────┘
                            browser shim (Workers/OPFS/WebSocket)  |  native shim (wasmtime/WASI)
```

Guest process = one wasm memory + N threads (workers) + a kernel-state handle.
Kernel state (fd tables, pipes, sockets, process table) lives in a shared "kernel heap"
SAB with fine-grained locks; the kernel is a library linked into every process instance,
so most syscalls never cross a message boundary.

---

## 4. Guest state & ABI mapping

Per-thread guest context, stored in linear memory (pointer passed to every translated
function):

| guest                         | representation                                          |
|-------------------------------|---------------------------------------------------------|
| x0–x30, sp, pc                | 8-byte slots in context struct                          |
| NZCV                          | packed byte; mostly statically eliminated (§5.4)        |
| v0–v31 (128-bit)              | 16-byte slots; ops lower to wasm SIMD128                |
| FPCR/FPSR                     | context fields; rounding-mode fidelity policy in §5.6   |
| TPIDR_EL0 (TLS)               | context field; `MRS/MSR` lowered to load/store          |
| CNTVCT_EL0, CTR_EL0, DCZID_EL0| trap-to-runtime reads; DC ZVA implemented (memset path) |

Uniform translated-function signature (tier-1):
`(ctx: i32, x0: i64, x1: i64, x2: i64, x3: i64, sp: i64) -> ()` — hot registers ride in
wasm params (which engines map to machine registers across tail calls); the rest live in
the context struct. Uniformity is required so `return_call_indirect` works for every block.
The exact hot-register set is chosen empirically in M4; starting point is ctx-only, the
param-threading variant is a measured upgrade since it must be uniform everywhere.

---

## 5. Compiler architecture

Pipeline: **decode → lift to GIR → optimize → emit wasm**, all in Rust, structured as a
library (`gex-xlate`) with two drivers (offline `gexc`, in-runtime `gex-jit`).

### 5.1 Decoder
Generated from ARM's machine-readable specification (MRS/ASL XML): decode tables, operand
extraction, and — critically — exhaustive encoding-coverage tests come from the spec, not
by hand. Hand-written fast paths for the ~100 hottest encodings. (Frequency reality:
~100 instructions cover >99% of dynamically executed code; prioritize by histogram over
real binaries — coreutils, libc, V8, CPython.)

### 5.2 GIR (gex IR)
A small SSA IR purpose-built for binary translation (not LLVM — see decision log §15):
explicit guest-register read/write nodes, explicit memory ops with ordering annotations,
flag-producing ops modeled as multi-output nodes, side-exit and syscall terminators.
Block-local by default; tier-2 builds region-level CFGs.

### 5.3 Lifter
Hand-written per-instruction semantics over GIR (mechanical, ~600 instructions in scope,
~150 needed for M1), validated instruction-by-instruction against QEMU and real hardware
via differential fuzzing (§12). ASL-derived semantics generation is a possible later
de-risking tool but not on the critical path.

### 5.4 Optimizations that matter for SBT (in impact order)
1. **Dead-flag elimination.** NZCV is only set by S-suffixed/CMP/TST ops and consumed
   sparsely. Liveness analysis kills most flag materialization; `CMP+B.cond` fuses into a
   single wasm compare-and-branch. AArch64 makes this far easier than x86.
2. **Guest-register caching.** Within a block/superblock, registers live in wasm locals;
   loads/stores to the context struct only at region boundaries and side-exits.
3. **Redundant load/store elimination, constant folding, address-mode folding** (guest
   `LDR x0,[x1,x2,LSL 3]` → single wasm load with computed address).
4. **Idiom recognition:** memcpy/memset loops, `DC ZVA` zeroing, atomics spin loops →
   runtime fast-path calls or tight wasm loops.

### 5.5 Two codegen tiers
- **Tier-1 (baseline, also used by gex-jit):** one wasm function per basic block, tail-call
  chaining, ctx-resident registers. Simple, fast to generate (goal: >10 MB/s of guest text
  on the runtime's wasm-compiled translator), correct for any control flow including
  irreducible CFGs. Every 4-byte-aligned text address gets a dispatch-table slot; blocks
  are translated lazily per entry point when tier-1 runs online.
- **Tier-2 (offline AOT, and background re-opt at runtime):** recover functions using
  symbols + `.eh_frame` unwind ranges + call-graph heuristics; build CFGs; form
  superblocks/regions; structurize with a relooper-class algorithm (irreducible regions
  fall back to a local `br_table` dispatch loop, or to tier-1 blocks); allocate hot guest
  registers to wasm locals across whole loops; emit real wasm loops so the engine's
  optimizing compiler sees them. Entry points into the middle of a region side-enter
  through the dispatch table into tier-1 blocks — both tiers coexist per-address.

### 5.6 Control-flow dispatch (the core perf problem of any SBT)
- **Direct branches / calls:** `return_call` to the known block function. `BL` writes the
  guest return address to x30 *and* tail-calls the target.
- **Indirect (`BR/BLR/RET`):** `return_call_indirect` into the **dispatch table**: a
  funcref table logically indexed by `(pc - text_base) >> 2`. Two-level (page-directory)
  layout keeps it sparse across many mapped objects; slots for untranslated addresses
  point at a stub that calls into gex-jit, translates, patches the slot, and re-enters.
- **Inline caches:** each indirect site caches its last target PC; predicted hit =
  compare + direct `return_call`. RET additionally uses a small return-target stack in the
  context (Rosetta-style) since call/return pairing predicts extremely well.
- **Safepoints:** each block entry checks nothing; loop back-edges (tier-2) and a
  every-N-tail-calls counter (tier-1) poll a per-thread interrupt flag for signal
  delivery/preemption (needed for pthread_kill, timers, and Go-style preemption signals).
  At a safepoint the full guest state is materialized in ctx — precise signal frames.

### 5.7 FP & SIMD
NEON maps well onto wasm SIMD128 (both 128-bit). Fidelity policy, per-build flag:
- **fast (default):** wasm FP semantics; NaN payloads and denormal/FTZ behavior may
  diverge; rounding mode assumed round-to-nearest (FPCR writes tracked; non-default modes
  trigger fallback).
- **strict:** software-assisted exact FPCR modes, NaN canonicalization to ARM rules —
  for the rare workload that notices.
Scalar FP is bit-exact in both modes for the default rounding mode (wasm and ARM are both
IEEE-754). Relaxed-SIMD used opportunistically where it matches ARM exactly on the host.

### 5.8 Atomics & memory ordering
- LSE atomics (`LDADD`, `CAS`, `SWP`…) → wasm atomic RMW ops (seq-cst ≥ ARM acquire/release: correct).
- LL/SC (`LDXR/STXR`) → CAS-loop transformation with monitor emulation for the exotic
  cases (mismatched-size SC fails conservatively).
- Plain accesses stay plain (guest data races remain data races — same contract).
- `DMB/DSB/ISB` → `atomic.fence` / translation-barrier handling for ISB (§6.2).

---

## 6. The hard cases (where "any binary" is won or lost)

### 6.1 Code discovery is *not* trusted
AOT does recursive-descent + heuristics to translate eagerly, but correctness never
depends on discovery: any missed target hits an untranslated-slot stub and gets tier-1
translated at runtime. Data-executed-as-code, hand-written assembly, jump tables we
couldn't bound — all safe.

### 6.2 Guest JITs & self-modifying code
Wasm cannot write-protect pages, so we cannot trap stray writes to code. We don't need
to: AArch64 architecture requires guests to execute cache maintenance
(`DC CVAU; DSB; IC IVAU; DSB; ISB` sequence, or `membarrier`) before executing modified
code — hardware would break them otherwise. We intercept:
- `IC`-class instructions, `ISB`, `membarrier(2)` → invalidate translations covering the
  affected range (content-hash pages to skip no-op invalidations),
- `mmap/mprotect/munmap` touching PROT_EXEC regions → invalidate + (re)translate on next
  entry (the standard JIT flow — write RW, mprotect RX — lands here).
Runtime translation compiles small wasm modules via `WebAssembly.Module` in the JIT
worker; guest thread blocks on `Atomics.wait` meanwhile. Translations are content-hash
cached, so a JIT re-emitting identical stubs pays once. Exit criterion: Node.js and
OpenJDK run (M3/M4).

### 6.3 fork / vfork / exec / clone
- `clone(CLONE_VM)` (threads): new worker, same memory — native.
- `vfork` / `posix_spawn`: parent legitimately suspends until child execs — implement
  exactly; this is the dominant spawn path and it's cheap.
- `fork`: new process instance (fresh wasm memory), copy the parent's *mapped* pages
  (we track exact mappings, so it's a bounded memcpy, not 4 GiB), duplicate fd-table
  entries in the kernel heap. No COW without traps — accept the copy cost; document.
- `exec`: tear down mappings, load + (cache-hit) translate the new image in-place —
  same instance, same worker.

### 6.4 Dynamic linking: gex-ld *is* the loader

By default gex, not the guest, is the dynamic linker. `execve` of a dynamic binary hands
`PT_INTERP`'s job to **gex-ld**, which does everything the interpreter would — with
whole-world knowledge no guest loader can have.

**Build time:** `gexc build app --sysroot rootfs/ --with-so 'plugins/*.so'` resolves the
full `DT_NEEDED` closure against the sysroot, **plus a dlopen manifest** — an explicit
list of additional .so files bundled for later `dlopen`. Every object is AOT-translated;
the bundle records the original ELF images, symbol/version tables, relocations, TLS
layout, and a fixed load-layout prototype (prelink-style; ASLR can re-randomize at load
by replaying the precomputed relocation lists).

**Load time**, gex-ld performs the interpreter's duties:
- **Relocation:** all aarch64 dynamic types — `RELATIVE`, `GLOB_DAT`, `JUMP_SLOT`,
  `ABS64`, `COPY`, `IRELATIVE`, and the TLS family (`TLSDESC`, `TPREL`,
  `DTPMOD/DTPREL`). Binding is eager (`BIND_NOW`); under `RTLD_LAZY` semantics an
  unresolved *function* symbol binds to a stub that raises the error on first call —
  matching what sloppy plugins observe under glibc.
- **Symbol resolution:** GNU symbol versioning (verdef/verneed), correct scope and
  interposition order — `LD_PRELOAD`, `DT_RPATH`/`DT_RUNPATH` with `$ORIGIN`,
  `LD_LIBRARY_PATH`, `/etc/ld.so.cache` from the rootfs, secure-exec (`AT_SECURE`)
  rules. (`LD_AUDIT`: ignored + logged initially.)
- **Loader introspection, owned by us:** `dlopen dlmopen dlsym dlvsym dlclose dladdr
  dladdr1 dlinfo dlerror dl_iterate_phdr` and glibc's `_dl_find_object` bind to
  guest-callable runtime trampolines. `dl_iterate_phdr`/`_dl_find_object` are how C++
  exception unwinding locates `.eh_frame`, so they — and `/proc/self/maps` — must
  reflect gex-ld's object list exactly. (`dlmopen` namespaces: stubbed initially.)
- **ifuncs:** `IRELATIVE` and ifunc-symbol resolvers are guest code, executed translated
  at load time with our `AT_HWCAP`. Since gex defines hwcaps deterministically, `gexc`
  can pre-run resolvers at build time and prebind the winner (prelink-style); load-time
  execution remains the correctness baseline.
- **TLS:** aarch64 variant-1 layout, TLSDESC-first. Closed-world bonus: because the
  dlopen set is declared up front, *every* module's TLS is allocated in the static TLS
  block — all TLSDESC resolutions become constant offsets and no DTV management exists.
  Open-world dlopen of never-declared libraries gets real dynamic TLS + DTV when runtime
  translation lands (M3).
- **Init/fini:** dependency-ordered `DT_INIT`/`INIT_ARRAY` (cycles broken as glibc
  does); `dlclose` refcounts with NODELETE semantics for bundled objects — glibc rarely
  truly unloads either, and it sidesteps translation-unload entirely.

**Per-libc strategy** — the guest libc has opinions about its loader:

| guest | strategy |
|-------|----------|
| glibc dynamic | **Full replacement.** `libc.so.6` talks to ld.so only through exported `GLIBC_PRIVATE` symbols (`_dl_open`, `__tls_get_addr`, `_dl_allocate_tls*`, `_rtld_global*`, `_dl_find_object`, …), so gex-ld materializes a **synthetic `ld-linux-aarch64.so.1`** — a real object mapped into guest memory exporting that surface (trampolines, plus a `_rtld_global` laid out per supported glibc version) — and performs ld.so's active duties (initial TCB/DTV, calling `__libc_early_init`). glibc-internal loads (NSS modules, gconv) go through `_dl_open`, i.e. through us — so `getaddrinfo` works. Being a real mapped object keeps `AT_BASE`, maps, and debuggers honest. |
| musl dynamic | **Cooperative mode.** musl fuses the loader into `libc.so` and its state is internal (non-symbolic), so full replacement means chasing hidden structs across builds. Instead musl's tiny single-stage loader runs *translated* once at startup, while gex-ld projects the dlopen manifest into the VFS as an overlay and attaches pre-translated modules on `mmap(PROT_EXEC)` via content-hash. Prebinding still applies: gexc simulates musl's (simple, unversioned) resolution at build time, guarded by a fingerprint. Same manifest, same observable behavior. |
| static / static-PIE | gex-ld applies `RELATIVE`/`IRELATIVE` + TLS setup; nothing else to do. |
| unrecognized `PT_INTERP` | `--loader=guest`: run that interpreter translated like any guest code — the always-correct fallback, also selectable per-process to debug suspected gex-ld divergences. |

**dlopen resolution tiers.** `dlopen(name)` resolves through, in order:
1. the **bundle registry** — manifest objects, pre-translated and pre-linked; attach
   cost is O(relocations), works fully offline and before M3 exists ("fake dlopen");
2. the **VFS + translation cache** — a real file whose content hash has a cached
   module. Note `gexc image` pre-translates every .so in a rootfs, so inside an AOT'd
   image this tier *always* hits — Python C-extensions, NSS modules, Apache modules all
   dlopen with zero runtime translation;
3. **runtime tier-1 translation** of a never-before-seen .so (M3+);
4. `dlerror`. `--strict-dlopen` pins deployments to tiers 1–2 for reproducible closed
   worlds.

**The payoff of being the loader:** gex-ld fixes every `JUMP_SLOT`/`GLOB_DAT` binding
before tier-2 code runs, so cross-DSO calls stop being indirect — `bl memcpy@plt`
compiles to a direct `return_call` to the (post-ifunc) translated body: no GOT load, no
dispatch-table hop; cross-module inlining of hot leaf functions becomes legal inside a
pinned bundle. Prebinding is guarded by a resolution fingerprint — an environment that
changes resolution (`LD_PRELOAD` of an unforeseen interposer, altered search paths)
demotes affected call sites back to the generic GOT-mediated path rather than running
with stale bindings. The guest-visible GOT/PLT bytes are still materialized in memory
(programs read them), but translated code no longer pays for them. Content-addressing
(§10) still applies: glibc/libstdc++ translate once per build-id and live on the CDN.

---

## 7. Memory subsystem

1. **Layout (wasm32 build):** one shared 4 GiB linear memory per process.
   Runtime reserves a small low region (context structs, kernel-visible scratch);
   guest gets ~[16 MiB, 4 GiB): ELF images, brk heap, mmap zone (top-down), main stack.
   ASLR: on, within-range, deterministic-seed option for record/replay.
2. **mmap/brk/mprotect:** fully implemented in the kernel's VMA tree (we are the
   allocator). Protection bits are *bookkeeping* (returned by /proc/self/maps, used for
   exec-invalidation and fault synthesis on kernel-mediated access) — not enforced on raw
   guest loads/stores. A stray guest write through a "PROT_NONE" page won't fault. This
   is the fidelity cost of trap-free wasm; gVisor-class apps don't care. (A debug build
   can enforce via check-instrumented loads/stores at ~2× cost.)
3. **memory64 variant:** same codebase, guest VA space up to engine limit (16 GiB today),
   for JVM-with-big-heap-class workloads. Chosen per-image at `gexc` time or by the
   loader when ELF headers/limits demand it.
4. **File mappings:** MAP_PRIVATE = eager copy (correct). MAP_SHARED on regular files =
   shared kernel-heap-backed pages when all mappers are gex processes; writeback on
   msync/munmap/exit with whole-mapping dirty granularity. MAP_SHARED coherence with
   external writers is out of scope.

---

## 8. gex-kernel: the Linux personality

Rust, compiled to wasm, linked into every process; shared state in the kernel-heap SAB
guarded by futex-based locks. Syscall entry: `SVC #0` lowers to a call with ctx (number
in x8, args x0–x5 — already in the context / params).

**Syscall tiers** (implement in this order, gated by real-workload strace corpora):
- **T0 (hello world / musl static, ~25):** read write openat close brk mmap munmap
  exit_group set_tid_address ioctl(TTY basics) writev fstat …
- **T1 (shell + coreutils, ~80):** full VFS surface (stat family, dirents, pipes, dup,
  fcntl), rt_sigaction/sigreturn/sigprocmask, wait4, execve, clone(vfork), getrandom,
  clock_gettime, uname, prlimit …
- **T2 (real apps, ~150):** clone(threads)+futex+robust lists, epoll/poll/ppoll,
  eventfd/timerfd/signalfd, socket family, sendfile, madvise (advisory), procfs surface …
- **T3 (long tail):** inotify, unix-socket SCM_RIGHTS fd passing, itimers, sched_*,
  xattrs, statx, memfd, io_uring→ENOSYS, seccomp (no-op success behind a flag; audited).

**Subsystems:**
- **VFS:** mount-table + dcache over pluggable backends: `tmpfs` (in-memory),
  `opfs` (browser persistent), `hostfs` (WASI/native passthrough), `httpfs` (lazy
  chunked read-only rootfs images with index — the "boot Alpine from CDN" path),
  `overlayfs` (rootfs image + writable layer), synthetic `procfs`/`devfs`/`sysfs`
  (self/maps and self/exe synthesized from real VMA state — glibc, openssl, JVMs read these).
- **Signals:** pending sets per thread/process; delivery at safepoints; full ucontext
  (guest register frame from ctx) so sigsetjmp/longjmp, Go, and crash handlers work.
  Synchronous faults (SIGSEGV from kernel-mediated paths, SIGILL from undefined
  instructions, SIGSYS) synthesized precisely; note: stray *raw* wild pointer writes
  don't fault (§7.2) — wasm traps (out-of-bounds ≥4 GiB) are caught per-thread and
  mapped to SIGSEGV.
- **Futex:** direct mapping onto `Atomics.wait/notify` on the shared memory — near-native.
- **Time:** monotonic/realtime clocks from host; synthetic guest **vDSO** whose
  clock_gettime reads a runtime-updated shared time page — keeps gettimeofday-heavy
  workloads off the syscall path. CNTVCT_EL0 reads served from the same page.
- **Net backends (pluggable):** native = passthrough sockets; browser = (a) WebSocket/
  WebTransport relay ("gexnetd") for real TCP/UDP, (b) in-runtime user-mode TCP/IP
  (slirp-style) for loopback + host-mediated fetch. Loopback works fully in-runtime from
  day one (many apps only need 127.0.0.1).
- **TTY/PTY:** line discipline + pty pairs (needed the moment anyone runs a shell);
  browser terminal via xterm.js in the shim.

---

## 9. Embeddings

- **Browser (primary for Puter):** kernel+guest in Workers over SABs; requires
  cross-origin isolation. Main-thread JS shim owns: worker lifecycle, OPFS handles
  (sync access handles in workers), terminal, net relay socket, module compilation
  (`WebAssembly.Module` is transferable — compile once, share). Guest wasm modules get
  engine code-caching on second load for free.
- **Native (wasmtime):** thin Rust host: threads, real FS/net passthrough. This is the
  CI/testing workhorse — every guest test runs here first, browsers run the same .gexm
  bit-identically.
- The `.gexm` container: wasm module(s) + metadata (guest→wasm PC maps, entry table,
  build-id, VMA prototype). Big binaries shard across multiple wasm modules
  (engine per-module function-count limits: ~1M functions — a 30 MB text section at
  block-per-function granularity will exceed it; sharding is designed-in, sharing the
  dispatch table via imports).

---

## 10. Caching & distribution

Content-addressed translation store keyed by (ELF build-id/content hash, gex version,
tier, build flags): local (OPFS / on-disk) → remote (CDN). A distro rootfs is AOT-built
once server-side by `gexc image`, producing per-.so modules; a user's `apt install`-ed
odd binary tier-1 translates locally and upgrades in the background. Code-size reality:
tier-1 wasm expansion is ~4–8× guest text — lazy per-object fetch and per-function lazy
engine compilation keep startup sane; tier-2 output is smaller and brotli-compresses well.

---

## 11. Tooling & DX

- `gexc build <elf|rootfs|oci-image>` → .gexm bundle. Key flags: `--sysroot <dir>`
  (closure resolution root), `--with-so <path|glob>` / `--dlopen-manifest <file>`
  (extra libraries bundled for offline dlopen, §6.4), `--loader=gex|guest`,
  `--strict-dlopen` (dlopen tiers 1–2 only), `--layout=fixed|aslr`.
- `gexc run` (native); `gexc serve` (browser dev server with COOP/COEP headers).
- **strace mode** (kernel-level, zero guest cost when off) and **guest gdbserver stub**
  (breakpoints = dispatch-slot swap to trap stub; single-step = tier-1 block splitting;
  PC maps make `info registers`/backtrace work). These two pay for themselves in our own
  debugging within weeks.
- Deterministic mode (fixed seeds, virtual clock) → groundwork for record/replay later.

---

## 12. Testing & verification (continuous, not a phase)

1. **Instruction-level differential fuzzing:** generated + mutated instruction sequences
   executed under gex vs QEMU-user vs real hardware (Graviton/RPi CI runners); ARM's
   `risu` corpus. Every lifter bug becomes a permanent regression vector.
2. **ASL-derived coverage:** decoder must classify 100% of the 32-bit encoding space
   (execute, UNDEFINED→SIGILL, unallocated) — generated from the spec XML.
3. **Syscall conformance:** gVisor's syscall test suite (designed for exactly this),
   LTP subset.
4. **Loader conformance:** glibc/musl `dlfcn` test subsets; search-order tests
   (`LD_PRELOAD`, rpath/runpath/`$ORIGIN`, ld.so.cache); C++ exception unwinding across
   dlopened objects; a per-glibc-release CI matrix (Debian/Ubuntu/Fedora images)
   exercising the `GLIBC_PRIVATE` shim.
5. **Integration ladder** (each is a CI gate once green): musl hello → busybox sh →
   coreutils+git+python3 → glibc/Debian userland → sqlite/openssl/zlib benches → Node
   (JIT path) → OpenJDK → build gex inside gex.
6. **Concurrency:** litmus tests (herd7 aarch64 corpus) for atomics mapping sanity.

---

## 13. Performance model & targets

Overheads stack multiplicatively: translation quality × wasm-engine codegen × syscall
emulation. Reference points: QEMU user-mode ≈ 5–20× slower than native; Rosetta 2 ≈
1.2–2×; good SBT literature ≈ 2–4×. Through a wasm engine, targets:

| tier | target vs native aarch64 (compute) |
|------|------------------------------------|
| tier-1 blocks | ≤ 8–10× (acceptable-not-good; it's the completeness tier) |
| tier-2 regions | **≤ 3–4× geomean** (CoreMark, zlib, sqlite, SPEC-like set) |
| syscall-bound / IO | near-parity (kernel is in-process; futex is native) |

Perf work is measurement-driven from M1: per-block exec counters, IC hit rates, dispatch
misses, flag-elimination rates — all cheaply sampled in the runtime and dumped as a
flamegraph over *guest* symbols.

---

## 14. Risks & mitigations

| risk | mitigation |
|------|-----------|
| Engine tail-call perf varies (esp. `return_call_indirect`) | benchmark M0 across V8/JSC/SpiderMonkey/wasmtime; fallback codegen: threaded dispatch loop per shard (design keeps emitter swappable) |
| Code-size blowup breaks browser delivery | lazy per-object fetch, tier-2 for hot objects, sharding, brotli; measure from M1 |
| Dispatch table memory (1 funcref / 4 bytes text) | two-level tables, per-object slabs, trap-stub default fill |
| `fork()`-heavy workloads (shells do fork+exec constantly) | vfork/posix_spawn fast path first; bounded-copy fork; measure real shell workloads early |
| FP fidelity long tail (FPCR modes, NaN payloads) | strict mode escape hatch; differential fuzzing finds divergence early |
| Browser constraint: cross-origin isolation, OPFS quirks | hard requirement stated up front; native embedding unaffected |
| Syscall long tail is endless | tier by strace corpora of target apps; gVisor tests quantify coverage; ENOSYS-with-logging telemetry to prioritize |
| glibc `GLIBC_PRIVATE` loader interface drifts across releases | the consumed surface is small and slow-moving; shim data layouts are per-glibc-version, pinned by a per-distro CI matrix; `--loader=guest` (run the real ld.so translated) is the always-correct fallback |
| Wild-pointer writes don't fault (no page traps) | documented fidelity gap; debug build with checked accesses; matters for crash-*reporting* fidelity, rarely for correct programs |
| Single 4 GiB space per process | memory64 variant; per-process (not global) limit keeps multi-process workloads fine |

---

## 15. Decision log (alternatives rejected)

- **Port QEMU TCG to wasm / interpret:** works today (container2wasm), 20–100× slow,
  dead-end perf ceiling. Rejected as the core; QEMU remains our test oracle.
- **Lift to LLVM IR, compile with LLVM→wasm:** best-possible code quality, but seconds-
  per-function translation kills runtime tiering, and LLVM's assumptions fight
  binary-translation semantics (precise flags, side-exits). Possible *offline tier-3*
  for hot libraries later; not on the critical path.
- **Relooper-everything single-function-per-guest-function from day 1:** correctness
  trap — irreducible CFGs, mid-function entries, and lazy translation all get ugly.
  Tail-call block graph is correct first; structure is an optimization (tier-2).
- **Asyncify/JSPI for blocking syscalls on the main thread:** huge code-size/perf tax and
  a second control-flow discipline. Workers + Atomics.wait everywhere instead.
- **System-mode (boot a real kernel):** no MMU traps in wasm → software page walks on
  every access. Rejected (and it wouldn't improve app compat for userspace targets).
- **Run the guest's `ld.so` translated vs become the loader:** chose gex-ld (§6.4).
  Running ld.so is maximally faithful but forfeits whole-closure AOT, prebinding,
  deterministic layout, and offline fake-dlopen — and drags runtime translation onto
  the startup path. It survives as `--loader=guest`; musl uses a cooperative hybrid
  because its loader state is fused into libc.so with no symbolic interface.

---

## 16. Milestones

Estimates assume 2–3 systems engineers; each milestone has a demo + CI gate.

- **M0 — vertical slice (≈6 wks):** ELF loader; decoder+lifter for ~90 instructions;
  tier-1 emitter with tail-call chaining + dispatch table; T0 syscalls; wasmtime host.
  **Exit:** static musl `hello`, `cat`, `echo` run; tail-call perf spike measured on all
  4 engines; differential-fuzz harness runs in CI.
- **M1 — real userspace (≈8 wks):** full base-ISA (integer) lifter; T1 syscalls; TTY;
  tmpfs+hostfs; signals v1; browser embedding v0 (single process).
  **Exit:** interactive busybox sh in a browser tab; coreutils pass; fuzzer clean on base ISA.
- **M2 — glibc, threads, NEON (≈10 wks):** gex-ld v1 — closure loading, eager
  relocation, symbol versioning, ifuncs, static-TLS bundles, the glibc private-interface
  shim, dlopen manifest served from the bundle registry (fake dlopen); musl cooperative
  mode; build-id module cache; clone/futex/TLS; NEON/FP lifter (fast mode); T2 syscalls;
  OPFS + httpfs rootfs.
  **Exit:** Alpine *and* Debian rootfs boot to shell; `python3` (incl. C-extension
  dlopen), `git`, `gcc -O0 hello.c` work; `LD_PRELOAD` interposition demo; a plugin app
  dlopens a manifest-bundled .so with zero runtime translation; pthread + litmus pass.
- **M3 — dynamic code (≈8 wks):** gex-jit runtime translation (mmap/mprotect/IC-IVAU
  invalidation, module compile pipeline); open-world dlopen (tier-3: translate a
  never-seen .so at runtime) + dynamic TLS/DTV; fork/vfork/exec complete; loopback net +
  net relay.
  **Exit:** Node.js runs (JIT on); `npm install` completes, native addons load via
  open-world dlopen; shell pipelines/job control solid.
- **M4 — performance (≈10 wks):** tier-2 optimizer (CFG recovery, superblocks, reg-alloc,
  flag elimination, inline caches, return cache); cross-DSO prebinding → direct calls +
  bundle-scoped inlining (§6.4); memory64 variant; CDN translation cache.
  **Exit:** ≤4× geomean on the bench suite; OpenJDK runs; gex builds itself inside gex.
- **M5 — product hardening (≈8 wks):** gdb stub + strace polish; `gexc image` for OCI
  images; multi-process browser session manager; quota/limits; docs.
  **Exit:** Puter integration demo — persistent Linux environment in a tab, apt install works.
- **M6 — stretch:** framebuffer/`/dev/fb0`→canvas, Wayland proxy, GL→WebGPU thunking,
  record/replay debugging, x86_64 frontend feasibility study.

---

## 17. Repo layout

```
gex/
  crates/
    gex-isa/        # decoder (spec-generated tables) + instruction defs
    gex-ir/         # GIR: SSA nodes, CFG, liveness, passes
    gex-lift/       # aarch64 → GIR semantics
    gex-emit/       # GIR → wasm (tier-1 & tier-2 backends), .gexm packaging
    gex-xlate/      # facade: the translator library (used offline & in-runtime)
    gex-ld/         # dynamic loader: closure resolution, relocations, symbol scopes,
                    #   TLS, dlopen service + bundle registry, glibc private-iface shim
    gex-kernel/     # Linux personality: syscalls, VFS, signals, procs, futex
    gex-vfs-*/      # tmpfs, hostfs, opfs, httpfs, overlay backends
    gex-rt/         # loader, dispatch tables, jit driver, thread runtime
    gexc/           # CLI: build | run | serve | image
  shims/
    web/            # JS: workers, OPFS, terminal, net relay client, COOP/COEP dev server
    native/         # wasmtime host
  tests/
    diff-fuzz/      # instruction differential fuzzer + risu integration
    syscalls/       # gVisor-suite runner, LTP subset
    integration/    # the app ladder (busybox → node → jdk)
  docs/
```

## 18. Prior art to mine (not adopt wholesale)

Rosetta 2 (return-address prediction, content-hashed AOT cache), FEX-Emu (thunking, hot
personality details), Box64 (wrapped-library bridges — future GL/SSL thunks), QEMU
(oracle + syscall-emulation reference), gVisor (syscall surface + test suite), CheerpX
(proof wasm DBT is product-viable), container2wasm/v86 (browser plumbing patterns),
wasm2c / Binaryen relooper literature (structurization), Wasmer WASIX (fork-in-wasm
war stories).
