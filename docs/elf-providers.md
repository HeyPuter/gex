# ELF export providers (stream W, merge/elf-prov)

Status (2026-10-05 night): the binding is built natively
(`shims/native/src/elfprov.rs`) and in the browser
(`shims/browser/src/cgen/elfprov.rs`); glibc's mem*/str* (`glibc_mem.c`) and
string printf family (`glibc_printf.c`) are provided; the heap
(`glibc_malloc.c`) is written and conformant but not registered (no
faster, §6). Conformance: `tests/elf-prov/run.sh` (native) and
`run-browser.sh`. The goal is set by
docs/merge-plan.md §10.1: translated Linux programs eventually get the
**whole** libc (libc, libm, libpthread) as native wasm, built by gexcc and
linked into the program's link group. Until gexcc can build glibc, ELF export
providers are the bridge. They replace glibc's hot exports with native code,
bound where the library maps. The binding is general from the start, so the
whole-libc provider reuses it and doesn't replace it.

## 1. What exists

- **PE providers** (`crates/gex-cgen/src/provider.rs`, stream R): C sources in
  `crates/gex-cgen/providers/*.c` with a `/* PROVIDES dll i386 */` header.
  `GEX_PROVIDE(fn, "export")` binds an `F_body` by export name. Each source
  is compiled to a PIC module without data or a stack. `nt_cgen` installs the
  functions in the function table at the exports' guest addresses, so every
  caller lands on them. They are i386 and PE only.
- **Stateful library providers** (`shims/native/src/libprov.rs`, stream U):
  a whole library replaced by a wasm module with data, a stack and a heap.
  `on_linux_maps` binds an ELF `.so`'s dynamic symbols when it maps (Unity's
  Mono, through `GEX_WASM_DLLS`). This is already "bind by ELF symbol", but
  it is whole-library only (the library's text is marked replaced), it is
  native only, and it has no versions and no IFUNCs.
- **`gex_ld::elf::Image::dynsyms`**: name, value, size, weak, ifunc, tls,
  for 64-bit images only. There is no symbol versioning (versym and verdef
  are not read).
- **SysV x86-64 argument helpers**: `tools/unity-dotnet/provider/gexprov64.h`.

## 2. Binding (general: any export set of a soname)

**Identity.** A provider names a soname (`libc.so.6`, `libm.so.6`) and an ABI
floor (`GLIBC_2.34`). It binds by **versioned symbol**, so it fits any glibc
build that exports that version. The build-id is recorded, not required. A
provider that depends on a layout outside the versioned ABI (glibc-private
`FILE` internals, malloc chunk headers seen by `malloc_usable_size`) must
declare the build-ids it was checked against, and it is skipped elsewhere.

**Versioned symbols.** gex-ld reads `DT_VERSYM` and `DT_VERDEF`, and each
`DynSym` gets `version` and `default`. A provider writes
`GEX_PROVIDE(fn, "memcpy@@GLIBC_2.14")`; `memcpy@GLIBC_2.2.5` (the old,
memmove-like one) is a different export. A bare name means the default
version.

**Where the code is bound.**
- *An ordinary function* is bound at the symbol's address, as for PE. Calls
  through the PLT reach it, because ld.so put that address in the GOT; so do
  `dlsym` and the library's own calls that go through its PLT. glibc calls
  malloc, free, calloc and realloc through the PLT internally (they are
  interposable by design), so `strdup` and `fopen` allocate with the
  provided allocator too.
- *An IFUNC* (`STT_GNU_IFUNC`: `memcpy`, `strlen`, `strchr` and most string
  functions on x86-64). The symbol's address is the **resolver**, and the
  implementations it picks (`__memmove_ssse3`...) are local, often
  stripped symbols. The provider replaces the resolver. A generated resolver
  body returns the guest address of a **provider stub**, and that address is
  bound in the function table to the provided function. ld.so runs resolvers
  for every `R_X86_64_IRELATIVE` and for `JUMP_SLOT`/`GLOB_DAT` relocations
  against IFUNC symbols, so every PLT and GOT caller lands on the stub, and
  so do libc's own internal IRELATIVE slots. Stubs live in one gex-owned
  guest page per process, one 16-byte slot per bound IFUNC. Direct internal
  calls to a variant that bypass the IFUNC (rare in glibc) keep running
  translated code, which is correct, only slower.
- *A set that must be bound together* (`malloc`, `free`, `calloc`,
  `realloc`, `memalign`, `posix_memalign`, `aligned_alloc`, `valloc`,
  `pvalloc`, `malloc_usable_size`, `mallinfo2`, `malloc_trim`, `mallopt`,
  `__libc_memalign`, `reallocarray`) is declared as a group. Either every
  member binds or none does. Two allocators over one heap are fatal.
- **Partial replacement.** Unlike libprov's whole-library mode, the rest of
  the library keeps running as translated code. Only bound addresses change.

**Calling convention** (SysV x86-64 first; i386 cdecl for 32-bit glibc
next). Integer and pointer arguments come in rdi, rsi, rdx, rcx, r8, r9,
then the stack at rsp+8. Doubles and floats come in xmm0 to xmm7. The result
goes in rax or xmm0. The caller pops. The provider clobbers only the
caller-saved set. The helpers move from `gexprov64.h` into the provider
header (`gp64_arg`, `gp64_xmm`, `gp64_ret`), with guest pointers as 64-bit
addresses under `--mem64`.

**Guest-visible state the provider must keep.**
- `errno` is glibc's TLS variable. The provider writes it at
  `fs_base + tpoff(errno)`, where the offset comes from libc's TLS segment
  and `errno@@GLIBC_PRIVATE`'s TLS symbol, resolved when the library binds.
- The stack canary at `%fs:0x28` and the pointer guard at `%fs:0x30` are only
  read by guest code. The provider doesn't touch them, but anything that
  mangles pointers (`setjmp`, `atexit` lists) stays translated until the
  whole-libc provider owns the TCB.
- `FILE` and the ctype tables are read directly by guest code
  (`__ctype_b_loc`, `getc` macros, `_IO_putc_unlocked`). They stay glibc's.
  A provided stdio function must use glibc's layout through the declared
  build-ids, or stay translated.
- Callbacks into guest code (`qsort`'s comparator, `bsearch`) go through
  `gex.call_guest`, as PE providers already do for thiscall.

## 3. Where it runs

- **Natively:** an `on_elf_maps` hook beside libprov's `on_linux_maps`,
  using gex-cgen's provider builder (one module per provider, cached, keyed
  by fingerprint).
- **In the browser:** the same hook in `shims/browser`, at the point where
  the guest maps an ELF image (the browser has no libprov yet). This is the
  target that counts (the browser is the perf target).
- **Units:** ELF images run on gex-xlate today, not on gex-cgen units, so
  there is no unit-key change for ELF images. The provider set joins the
  image's translation key where gex-xlate caches modules.
- **gex-cgen edits:** `provider.rs` (ELF identity, versioned names, IFUNC
  and group attributes, the x86_64 class) and `provider.h` (SysV helpers).
  These re-key every unit, so they go in **one commit**, and the
  coordinator schedules its landing between E/F measurement series. New
  provider `.c` sources don't re-key anything.
- Switches: `GEX_PROVIDERS=0` and `GEX_PROVIDER_SKIP`, as for PE.
  `GEX_PROVIDER_TRACE` lists the bindings.

## 4. Conformance (from the start)

The risk is compatibility, not speed (§10.1): one wrong export breaks every
Linux program.
- `tests/elf-prov/`: x86-64 C programs built with the host gcc against glibc,
  run under gexc twice, once with providers and once with `GEX_PROVIDERS=0`
  (translated glibc). stdout and the exit status must match byte for byte,
  as lockstep compares.
  - mem*/str*: every size from 0 to 256 plus large sizes, every alignment
    pair from 0 to 15, overlapping moves, bytes at a page end before an
    unmapped page (no over-read past what glibc reads), NUL at every
    position, and `memcmp`/`strcmp` sign conventions.
  - malloc: random alloc/free/realloc traces with checksummed contents,
    alignments, `malloc_usable_size`, many threads, and fork plus free in
    the child.
  - libm: bitwise against translated glibc over a fixed input grid plus
    special values (±0, subnormals, inf, NaN payloads, huge arguments for
    sin/cos), plus `errno` and fenv flags where glibc sets them. A function
    that can't match glibc's bits isn't provided.
- The same programs run in the browser (run-node), as tests/wasm32 does.
- A trace mode (`GEX_PROVIDER_CHECK=1`) runs both the provider and the guest's
  code on a copy for every call and reports the first divergence. It is for
  debugging a game, not for gating.

## 5. Contents, in order (each ranked by profile, gated on a paired page run)

The order is set by the measured share of guest work (§6): mem*/str*, then
malloc as a group, then libm, then whatever else the profile shows.

As built:
- **IFUNCs** need no generated resolvers: the resolver's address is bound
  to `gexp__ifunc` (`GEX_ELF_IFUNC`), which returns the resolver's address
  plus one, and that address is bound to the provided function. Nothing is
  written into guest memory.
- **glibc_mem** (stateless): memcpy, memmove, mempcpy, memset, memcmp, bcmp,
  __memcmpeq, memchr, strlen, strchr, strcmp. memcmp is compared by sign:
  glibc's own variants disagree on its magnitude.
- **glibc_printf** (stateful): snprintf, vsnprintf, sprintf, vsprintf, on
  musl's printf core with exact double conversion; what it cannot match
  (positional arguments, %m, wide, %a, %L, %#g, a non-"." decimal point,
  errors) goes to the library's `__vsnprintf_chk`/`__vsprintf_chk`, found
  by name in the mapped image. The `_chk` and FILE-based forms stay
  translated.
- **glibc_malloc** (stateful, a GEX_GROUP): conformant, not registered.
- Bindings outlive an unmapping (a forked child shares the table, and its
  parent's GOT holds resolver+1 addresses); an object mapped over them
  takes them back. In the browser a forked child binds what it inherited
  before it runs, and a provided address whose slot lost its bridge is
  re-bridged on its miss.

## 6. Measurements

**Warfork dedicated server, x86-64, 6 bots in deathmatch (gooner, native
gexc `--record`, 2026-10-05).** The weight is block bytes times executions,
a proxy for executed guest instructions. Steady state is the 1020 s run
minus the 420 s run (`scratch/W/elf/tools/diff.py` and `share.py`;
profiles in `scratch/W/elf/wf420`, `wf1020`):
- **libc.so.6: 14.7% of guest work.**
  - mem*: 7.6% (`__memmove_ssse3` 4.8%, `__memset_sse2_unaligned[_erms]`
    2.8%).
  - printf family: about 4.7% (`__vfprintf_internal` 2.7%,
    `_IO_default_xsputn` 1.0%, `vsnprintf`, the `_IO` string-stream setup).
  - malloc/free: about 1.5%.
  - str*: about 0.7%.
- **libm.so.6: 1.3%** (`__sincos_sse2` 0.8%, `__floorf_c` 0.4%, atan2
  0.1%).
- The rest is the game: AngelScript (libangelwrap) 36.9%, the server 30.2%,
  the game module 16.6%.
- glibc's IFUNCs pick the SSE2/SSSE3 variants under gex's CPUID (no AVX).
- Load phase (the 420 s run alone): libc 17.4%, mostly time-zone code
  (`__offtime`, `__tzfile_compute`: localtime per log line), mem*, and
  malloc.
- Muck (Unity, `-batchmode -nographics`, `--mem64`): no profile yet. The
  run doesn't exit on SIGINT or SIGTERM, so `--record` writes nothing.
  Loading took 175-205 s; it needs a clean exit or a periodic profile dump.

**Microbenchmark, providers off against on** (`scratch/W/bench-mem/mb.c`
under run-node, i.e. V8 as in a page, worldmachine, 12 ABBA pairs,
medians, 2026-10-05; not a page run, and not a game):
- mem*/str* loop: 1.86x faster with providers (87 against 162 ms);
- snprintf loop: 2.34x faster (339 against 792 ms);
- malloc/free loop: no faster (177 against 175 ms), so glibc_malloc stays
  unregistered.
If Warfork's libc share (§6) sped up alike, its guest work would drop by
about 6% (an estimate from the profile, not a measurement).

Found on the way:
- `select` on a stdin pipe was always "readable" natively, so Warfork's
  server blocked in read after loading. Fixed on branch `fix/stdin-poll`
  293345c4 (gex-kernel `STDIN_READY`, set by gexc).
- A TCP `accept` (sv_http's listener) returns ENOTSOCK: gex-kernel has no
  inet listeners (`sys_unix_accept` only).
