# gex-eject: standalone projects from a gex session

`gex-eject` turns what a gex run learned into a project that builds and runs
on its own, the way a pewasm project does: the program's code translated
ahead of time, the files it needs, a runtime, and a place for your own code
(library replacements) that survives regeneration.

```sh
gexc run --record rec ./prog 5          # 1. record a session
gexc eject rec --out prog-project       # 2. write the project
prog-project/build.sh                   # 3. build it (translate, bind replacements)
prog-project/run.sh                     # 4. run it: prog 5, as recorded
prog-project/run.sh 7                   #    or with other arguments
```

The code is in `crates/gex-eject` (the project model, the eject, the build,
the run), `shims/native/src/aot.rs` (the runtime side: prebuilt modules and
replacement calls) and `crates/gex-xlate/src/intrinsic.rs`
(`Intrinsic::Replace`). The session format is `docs/profile-format.md`.

This covers eject v0 and v1 (merge plan M4, M5); the status is at the end.

## The project

```text
prog-project/
  gex-project.json    what the project is                          generated
  rootfs/             the closure and the files the session read    generated
  session/            session.json and profile.json it came from    generated
  runtime/gexc        the runtime: a copy of the gexc that ejected  generated
  gen/gex_replace.h   the guest ABI, for replacement code           generated
  build.sh            the build step                                generated
  run.sh              the native launcher                           generated
  .gitignore          ignores build/                                generated
  user/               yours                                         created once
    eject.json          settings: replacements, coverage, arguments
    src/*.c             replacement functions
    files/              extra files for the guest's filesystem
  build/              build output (build.sh)
    modules/*.wasm      one tier-1 module per program image
    user.wasm           user/src/*.c, compiled
    rootfs/             rootfs/ with user/files/ over it
    build.json          what was built from what
```

- **Generated files** are rewritten by every eject, from the session and the
  runtime alone. Re-ejecting the same session with the same gexc gives the
  same bytes (`diff -r` of two ejects is empty).
- **`user/`** is created from templates when it is missing and never written
  again. A re-eject into an existing project keeps it.
- **`build/`** is deleted by an eject, because it was built from what the
  eject replaces.
- An eject refuses to write into a non-empty directory that is not a project.

### What goes into `rootfs/`

From `session.json` (see `docs/profile-format.md`):

- **Objects** (the executable, and for a dynamic program the interpreter and
  libraries), each read from where the run read it (`host_path`, the sysroot,
  or the host) and checked against the hash the session recorded. A changed
  file fails the eject.
- **Files the session read**, at their guest paths, with their hashes in
  `gex-project.json`.
- **Directories** it opened, and its working directory, are created.

Left out, and listed in `gex-project.json` so the reader can see them:

- `missing`: paths the session probed and did not find. They stay missing.
- `outputs`: files it created or only wrote.
- `/proc`, `/dev`, `/sys`: gex makes those.

A file the session read from `/tmp` fails the eject: under a sysroot, which is
how a project runs, gex mounts a fresh tmpfs on `/tmp`.

**Adding files:** put them in `user/files/` at their guest path
(`user/files/etc/foo.conf` is `/etc/foo.conf`) and rebuild. They win over
`rootfs/`. Editing `rootfs/` itself fails the build: it is generated, and its
hashes are checked.

## Building

`build.sh`:

1. compiles `user/src/*.c`, if there are any, into `build/user.wasm` with
   `clang --target=wasm32` (flags below);
2. runs `runtime/gexc eject build .`, which
   - checks `rootfs/` against `gex-project.json`;
   - binds and checks every replacement (below);
   - places the closure (below) and translates each object into a tier-1
     module (`build/modules/NAME-HASH.wasm`) with gex's library intrinsics
     (the `mem*` functions) and the replacements;
   - copies `rootfs/` and `user/files/` into `build/rootfs/`;
   - writes `build/build.json`, including a hash of everything a user can
     change (`gex-project.json`, `user/eject.json`, replacement modules).

**Coverage** (`user/eject.json` `"coverage"`):

- `"profile"` (default): every block the recorded session ran
  (`profile.json`), with every page those touch translated whole, and
  nothing followed off them onto fresh pages: what was seen. An object the
  session ran no code in gets its entry point only. Without a
  `profile.json` this is `"static"`.
- `"static"`: `"profile"` plus what discovery finds from the symbol table and
  the data. Covers paths other inputs take.
- `"exhaustive"`: every decodable address.

Measured on the host's dynamically linked gzip (x86-64, glibc): `"static"`
translated 342,515 blocks of libc into a 168 MB module that took Cranelift
7 CPU-minutes; `"profile"` translated 45,591 (24 MB). The corpus kernels and
a static gzip run the same under both. A static gzip ejected from a `-9`
compression still compresses at `-1` (same pages), and fails loudly at `-d`.

**Dynamically linked programs.** The closure is placed the way `gexc build
--closure` places one: the executable at `PIE_BASE`, the interpreter at
`INTERP_BASE` (both where gex's loader puts them), and the libraries up from
`MMAP_BOTTOM` in the session's order, 64 KiB aligned. The program's module
owns the memory and dispatch tables, and every other object's module
continues its slot numbering. At run time the libraries are pinned
(`RunOptions::pins`, as `gexc run --pin`), and their modules are installed
into the runtime's module registry before the program starts, so threads and
forked children get them too. A pin that does not land is reported under
`--verbose`; code at the wrong address then misses loudly.

## Running

`run.sh` is `runtime/gexc eject run .`:

```sh
./run.sh                      # the recorded argv (or user/eject.json "args")
./run.sh ARGS...              # other arguments after argv[0]
./run.sh --strace -- ARGS...  # trace system calls
./run.sh --jit-fallback -- ARGS...
```

The guest sees `build/rootfs` as `/`, starts in the recorded working
directory, with the recorded environment. Its image module comes from the
build (looked up by the loaded image's identity: entry, mode and placed
segments), never from translation.

**Nothing is translated at run time** unless `--jit-fallback` is given. A
branch to code the build does not have fails loudly:

```text
gexc: gex failed at guest pc=0x102382c: gex-eject: code at 0x102382c (in the program's
image) was not translated when the ejected project ... was built: the recorded session
never reached it and discovery did not find it. Record a session that reaches it and
re-eject, set "coverage" to "static" or "exhaustive" in user/eject.json, or run with
--jit-fallback
```

Code the program generates at run time (an anonymous executable mapping) is
reported as such: only `--jit-fallback` can run it.

Likewise a program the project has no module for (an `execve` of a program
the session did not run) fails naming it, and a project changed since its
build (`user/eject.json`, a replacement module) refuses to run until
`build.sh` has been run again.

`--jit-fallback` (on a run), or `"jit_fallback": true` in `user/eject.json`
(for every run of the build, native and web), turns runtime translation back
on. It is off by default. Both runtimes already contain the translator, so
it ships nothing extra yet; with gex-kernel's feature gating it becomes what
decides whether the translator ships.

## Library replacement

A replacement serves a guest function with a user wasm function. In
`user/eject.json`:

```json
{
  "replace": [
    { "object": "ack", "sha256": "sha256:3c1f...", "symbol": "ack",
      "export": "my_ack", "abi": "c" },
    { "import": "getenv", "object": "gzip", "export": "my_getenv" },
    { "library": "libSDL2-2.0.so.0", "wasm": "user/sdl2.wasm", "prefix": "" }
  ]
}
```

Each entry names what it replaces in exactly one of four ways:

| Target | Binds | Staleness check |
| --- | --- | --- |
| `"symbol": "f"` | the function `f` defines in `object`, at its definition: direct calls, PLT calls through the GOT, pointers, tail calls | `sha256` (the object) and `bytes` (the function), each if given |
| `"offset": "0x...", "size": "0x..."` | the function at that link-time address of `object`, for a stripped object | `bytes` required, `sha256` if given |
| `"import": "f"` | `object`'s PLT stub for `f` (every object's, with no `object`): only that importer's calls; the definition is untouched | `sha256` if given |
| `"library": "libX.so"` | every function the library exports, each served by the module's export of the same name (after `prefix`) | `sha256` if given |

| Field | Meaning |
| --- | --- |
| `object` | The object: its guest path, file name or soname. |
| `sha256` | The object's hash, from `gex-project.json`. If the object changes, the build fails and names the stale replacement instead of binding it to a function that may have changed. |
| `bytes` | `sha256:` of the replaced function's own bytes (`[address, address + size)`, `size` from the symbol table or the entry). Catches a changed function in a changed object, and keeps an `offset` from binding to whatever moved there. |
| `export` | The wasm function (default: the symbol; `fn_<offset>` for an offset). |
| `prefix` | For a library: what the module's export names start with. |
| `wasm` | The module, relative to the project (default `build/user.wasm`, which `build.sh` compiles from `user/src/*.c`). Any producer's module works if it meets the rules below. |
| `abi` | `"c"` (default) or `"ctx"`. |

`gexc eject override <project> <object> <symbol|0xoffset> [--size N]`
prints a checked entry, with both hashes filled in:

```text
$ gexc eject override . ack 0x1011720 --size 0x60
{
  "object": "ack",
  "sha256": "sha256:723b0a60...",
  "offset": "0x1011720",
  "size": "0x60",
  "bytes": "sha256:934f6a41...",
  "export": "fn_1011720",
  "abi": "c"
}
```

**Swapping a whole library.** `{"library": "libsq.so"}` binds every
function the library exports. A function the module does not provide is
bound to a placeholder that stops the run naming it, when (and only when) it
is called, like pewasm's aborting import placeholders; the build lists them.
An IFUNC export fails the build (its address is a resolver). The library
itself is still loaded, so `ld.so` resolves against it and its initialisers
run; none of its exported functions' code does.

**How it is bound.** The build puts `Intrinsic::Replace(id)` at the bound
address (the definition, or for an import the PLT stub), which generalises gex's `memcpy`-style intrinsics
(`gex-xlate/src/intrinsic.rs`) from a fixed set of wasm instructions to any
user function. That block is one host call, `sysreg_write(pc,
REPLACE_SYSREG, id)`, and a return. So every way of reaching the address runs
the replacement: a direct call, a PLT stub through its resolved GOT slot, a
function pointer, a tail call. None of the original function's code runs
(entries into its middle, which a normal program never makes, still reach the
original blocks).

**The three ABIs.**

- `"c"`: an ordinary C function, typed by its wasm signature. The runtime
  passes the guest's integer argument registers (`x0`-`x7`, or `rdi`, `rsi`,
  `rdx`, `rcx`, `r8`, `r9`) to its i32/i64 parameters in order, and its
  floating-point argument registers (`v0`-`v7` or `xmm0`-`xmm7`) to its
  f32/f64 ones, and writes the result to `x0`/`rax` (an i32 zero-extended) or
  `v0`/`xmm0`. More arguments than registers is a build error; use `"ctx"`.
- `"ctx"`: `void f(gex_ctx c)`, like a pewasm import binding. The function
  reads and writes the guest's registers through the context itself, with
  the macros in `gen/gex_replace.h` (`GEX_ARG(c, i)`, `GEX_RET(c)`,
  `GEX_SP(c)`, `GEX_STACK_ARG(c, i)`, `GEX_FARG_F64(c, i)`, ...). It must not
  return for the guest: the block does that after it.
- `"entry"`: `void f(int ctx)` that *is* the function, its return included:
  on return, the context's PC says where the guest goes next, and the block
  (`Intrinsic::Override`) dispatches there. What `gexc eject decompile`
  writes (below). Only for a function bound by `symbol` or `offset`.

Any replacement module may import, besides `env.memory`, the `gex.*` host
functions translated code calls (`syscall`, `safepoint`, `sysreg_read`,
`sysreg_write`, `cache_op`, `fma`, `illegal`, `breakpoint`,
`dispatch_miss`, as `gen/gex_abi.h` declares them). They are the very
functions the guest's own modules get, so the syscall allow-list applies to
them too; a host call that stops the guest (an exit, a fork, a signal)
unwinds through the replacement exactly as through translated code, which
is sound when the replacement left the context architectural first, as the
function tier's C does.

**Memory.** A guest address is a linear-memory offset, so a guest pointer is
a C pointer (`GEX_PTR(addr)` for a 64-bit value). The replacement runs in the
guest's memory. Its own data, heap and stacks live in
`[0xF0000000, 0xF1000000)`, above `MMAP_TOP`: the guest is never given an
address there and the browser shim's arena never claims one (the shim itself
keeps its stack and data in `[STACK_TOP, MMAP_BOTTOM)`). The first 8 MiB are
data and heap, then one 128 KiB stack for each of up to 64 guest threads,
which the runtime points `__stack_pointer` at. Natively the region is
recorded as a mapping, so `fork` copies it; in the browser the memory is
grown to cover it at the first replacement call.

**Module rules** (checked by the build, before anything runs):

- It may import only `env.memory`, with a maximum of at least the guest's
  65535 pages: shared for the native runtime, unshared for the web build. Anything else (an Emscripten runtime's imports, WASI)
  is refused, naming the import.
- If it imports memory, it exports a mutable i32 `__stack_pointer`.
- The export exists and has a type the ABI can call.

`build.sh` compiles each `user/src/*.c` with the function tier's flags
(`gex_cgen::toolchain::CFLAGS`: `--target=wasm32 -O2 -mtail-call -msimd128
-matomics -mbulk-memory ... -ffreestanding -nostdlib`) plus
`-fvisibility=default -Igen`, and links twice:

```sh
wasm-ld --no-entry --export-dynamic --import-memory --max-memory=4294901760 \
  --global-base=4026531840 --export=__stack_pointer --export=__heap_base \
  --shared-memory -o build/user.wasm build/obj/*.o     # native
  ...             -o build/user.web.wasm build/obj/*.o # the web build's plain memory
```

`USER_CFLAGS` (clang) and `USER_LDFLAGS` (wasm-ld) add to it. There is no
libc: the code is freestanding.

**Examples**, all run on both guests unless noted:

| Program | Replaced | ABI | Calls | Output |
| --- | --- | --- | --- | --- |
| corpus `ack 3` | the program's `ack` | `"c"` | 8 | same `result`; kernel 79 ms → 9 ms (AArch64) |
| corpus `strings 3` | musl's `strtol` (and its alias `strtoll`) | `"ctx"`, writes `*endptr` itself | 720,000 | same `result` |
| host `gzip -9` (x86-64, dynamic glibc) | glibc's `getenv`, called through gzip's PLT | `"c"` | 2 | same bytes |
| a program linked against `libsq.so` (x86-64) | the whole library (`sq`, `cube`; `unused_fn` left missing) | `"c"` | 10 + 10 | 2310 → 2330; dropping `cube` from the module stops the run at its first call, naming it |
| the same | only the program's import of `sq` | `"c"` | 10 | 2320 |

The first:

```c
/* user/src/ack.c */
#include <stdint.h>
static uint32_t calls;
uint32_t my_ack(uint32_t m, uint32_t n) {
    calls++;
    while (m != 0) {
        n = n == 0 ? 1 : my_ack(m, n - 1);
        m--;
    }
    return n + 1;
}
uint32_t my_ack_calls(void) { return calls; }
```

## Decompiling a function

```sh
gexc eject decompile <project> <object> <symbol|0xoffset> [--size N] [--callees] [--force]
```

writes the function tier's C for a guest function into
`user/src/decompiled_<name>.c` and prints the `user/eject.json` entry that
binds it (with the object's hash and the function's bytes' hash, so a changed
binary reports the override as stale):

```text
$ gexc eject decompile . ack ack
user/src/decompiled_ack.c: 1 function of C; bind it in user/eject.json with:
{ "object": "ack", "sha256": "sha256:723b...", "symbol": "ack",
  "bytes": "sha256:60bd...", "export": "decompiled_ack", "abi": "entry" }
```

- **The C** is exactly what the function tier (`gex-cgen`, Phase B) emits for
  the function, from the same recovery (`gex_xlate::func::recover`, with the
  object's `.eh_frame`), summaries and filters (`gex_cgen::select`), as one
  standalone unit (`gex_cgen::standalone`): guest registers are C locals,
  guest memory goes through `gen/gex_abi.h`'s helpers (the tier-1 bounds
  semantics), system calls and safepoints through the `gex.*` host
  functions. Its export, `decompiled_<name>(int ctx)`, is bound with the
  `"entry"` ABI.
- **It round-trips:** unmodified and bound, it gives the original's output
  (natively and in the web build). Checked on both 64-bit guests: corpus
  `ack` (ack), `qsort` (qsort with `--callees`, kernel with `--callees`:
  the whole tree including the libc it calls), `strings` (kernel with
  `--callees`), and gzip (`longest_match`, 468,088 calls; `updcrc`;
  `deflate` with `--callees`: 114 functions on AArch64, 74 on x86-64).
- **What it does not hold, it leaves** as the function tier does: a call to
  a function not in the file writes every register back, sets PC to the
  callee and returns; the rest of the function then runs as the original
  translated code once the callee returns into it. So an edit after such a
  call does not take effect. `--callees` puts every function it reaches by
  direct call (that the tier takes) into the file too, so those calls stay
  in C. Indirect calls go through the file's own `gex_call_known` and leave
  when the target is not in it.
- **Refused, loudly:** a function recovery did not find, one the tier does not
  take (self-modifying pages, x87 statements with no C lowering yet), and
  32-bit (i386) code.
- The file is yours: decompile refuses to overwrite it without `--force`.

## The web build (Node and the browser)

`gexc eject ... --web-shim gex_browser.wasm` vendors gex's browser runtime
into `runtime/web/`: the shim you give it (built from the same source, for
the same guest: `shims/browser`, `wasm32-unknown-unknown`, with
`shims/browser/build.sh`'s link flags) and the `gex.mjs` this gexc was built
with. `build.sh` then also writes `build/web/`:

- the program translated for the browser's single-Worker shim (a plain,
  unshared memory). A static program is one image module. A dynamic one is
  run the way gex's own pages run a closure (`gexc build --closure
  --shared-image`): every object, the executable included, is a demand-filled
  library module, and the process starts from a seed module;
- `manifest.json`: modules, files (`build/rootfs`), arguments, environment,
  pins, replacements, the guest ABI the replacements are called with, and the
  syscall allow-list.

Launchers, sharing `runtime/web/eject.mjs`:

```sh
node run.mjs [--jit-fallback] [--strace] [--] [ARGS...]   # Node
node serve.mjs 8000        # then open http://localhost:8000/?args=5
```

The page needs cross-origin isolation (gex's runtime uses
`SharedArrayBuffer`); `serve.mjs` sends the two headers. Replacements run
in JavaScript's hands: the user module is instantiated on the guest's memory
and called with the same `c`/`ctx` ABI as natively. `build.sh` links
`build/user.web.wasm` for it, since a module importing a shared memory will
not link to the shim's plain one (checked at build).

Checked on both guests in Node (`run.mjs`, JIT off): the corpus kernels,
static gzip and, on x86-64, the host's dynamically linked glibc
gzip, and a program whose whole library is swapped for wasm. Checked in
headless Chromium: the `ack` page, with arguments.

**Limits of the web build:**

- **One process, one thread.** A session that forked or ran threads gets a
  manifest that says so, and the launchers refuse it.
- **File times** are the bundle's (`manifest.json` `mtimes`, from
  `build/rootfs`, which keeps the times of the files the session read), and
  a file the guest writes is stamped when it is written, so `gzip` without
  `-n` matches the native run. (Until `merge/web-fixes` the browser's tmpfs
  answered 0.)

Found and fixed on the way (`shims/browser`): with the syscall trace on, a
miss the single-Worker shim translated (`--jit-fallback --strace`) asked
`gex_closure_owner` from inside the kernel borrow and panicked ("the kernel
was re-entered"); it asks the held form now. And the break of a dynamic program
started above the interpreter (`Program::brk`), which sits at
`INTERP_BASE == BRK_TOP`, so it could never grow and glibc's malloc fell
back to mmap: a different path through libc than natively. It now follows
the executable, as natively. And the launcher copies what the guest writes
before handing it to Node, whose asynchronous stdout otherwise sent bytes the
guest had since overwritten.

## The slim runtime (runtime level)

- **System calls:** only those the recorded session made are allowed, by
  canonical number, natively (`gex-native`'s syscall import) and in the
  browser (the launcher's guard). Anything else stops the run, naming it:

  ```text
  gexc: gex failed at guest pc=0x1011b04: gex-eject: system call mmap (222) was never
  made by the session the ejected project ... was ejected from, and its runtime allows
  only those. Record a session that makes it, or add it to "allow_syscalls" in
  user/eject.json
  ```

  `user/eject.json`: `"allow_syscalls": ["mmap"]` adds some (by name or
  number); `"syscalls": "all"` turns the list off.
- **Single process:** a session that never forked gives a runtime that
  refuses `fork` (natively; the web build is single-process anyway).
- Not yet: gex-kernel subsystems behind cargo features (waits for S12's N4,
  as gex-kernel is being edited heavily).

## Features

An ejected build starts as the whole runtime a gex page runs, and
`user/eject.json` `"features"` takes parts out of it: each is `true` (kept,
the default) or `false` (left out). The list is `crates/gex-eject/src/
features.rs`; a feature switched off that no build can remove yet fails the
build and says why, rather than doing nothing.

| Feature | What it is | Off |
| --- | --- | --- |
| `compositor` | the scene compositor: every window in one WebGPU canvas, drawn by a worker, with server-side decorations, its own cursor and hit testing (`compositor.js`, `compositor-worker.js`, `gpu-device.js`) | each window is its own canvas in the page, laid out by `web/src/bare-desktop.js` and composited by the browser; no decorations (Wine draws its own frames). The page is built with `__GEX_COMPOSITOR__` false (vite `define`, from `GEX_FEATURES_OFF=compositor`), so the code is not in the bundle |
| `gl` | the guest's OpenGL: Mesa on WebGPU (`gex/gex-wgpu-replay.*`) | the page ships no Mesa |
| `multiprocess` | more than one guest process | not yet: a W3 program is several processes |
| `translation` | run-time translation (tier-1, the function JIT) behind the units | not yet: the shim's translator is not a separate build (kernel and shim feature gating) |

Only a W3 page's build applies them so far; a Linux or NT-layer project
with a feature off fails its build.

## W3 pages

A W3 program (Wine as native wasm32: every Wine process a guest process
running `wine`, its PE side in `winepe.wasm`, the Windows images'
code in function-tier units) runs in the same page (`nt.html`) as any
demo. Ejecting one makes that page a standalone site, with the features
the user keeps:

```sh
# 1. record: run the page to its window (headless Chromium)
node tools/eject/record-w3.mjs http://127.0.0.1:8000/ rec --serve SITE --bundle mspaint.tar
# 2. eject: the bundle, the units it used, the runtime, a build step
gexc eject rec --out mspaint-project --web-src web --unit-cache UNITS --features -compositor
# 3. build: the page with those features, the site's inputs in build/web
mspaint-project/build.sh
# 4. the static site
node web/eject-site.mjs mspaint-project site --strip --prune --dedupe
node tests/eject/w3-page.mjs site      # the leg: it reaches "Untitled - Paint"
```

- **The recording** (`w3-session.json`) is how the page was started (its
  `__nt.session`: exe, arguments, environment, working directory, size,
  page parameters) and what every Worker said in `[gex-record]` lines
  (gex.mjs): the processes (each one's main image and command line), the
  units each image used and the images that had none, and the native wasm32
  modules (`wine`, `winepe.wasm`, wasm DLLs) by content key.
- **The project** holds the recording (`session/`), the bundle
  (`bundle/rootfs.tar`, hash checked), the units (`units/KEY/`), the
  browser shim, kernel and seed (`runtime/web/gex/`) and `gex-project.json`
  (`"kind": "w3"`). `page_source` names the gex checkout's `web/`: the page
  is built at build time, because the features decide what goes into it.
- **The build** (`gexc eject build`) runs `web/build-eject-page.sh` with
  `GEX_FEATURES_OFF`, puts the project's runtime wasm in it, drops what a
  feature off ships (Mesa for `gl`), checks the bundler really left out
  what `compositor` off names, and writes `build/web/` (`page/`,
  `rootfs.tar`, `units/`, `w3.json`: the launch).
- **The site** (`web/eject-site.mjs`) is the page, the bundle, the units
  with a preload list, the launcher (which starts nt.html as recorded) and
  `sw.js`.

Everything else is the page as gex runs it: several processes, the
kernel, run-time translation behind the units.

## Limits

- **Dynamic linking for the recorded program only.** An `execve` of a
  static program the session also ran works (its image module is found by
  identity); an exec'd *dynamic* program gets no closure modules, so its
  interpreter's first block misses loudly.
- **Import binding is PLT stubs.** A call through the GOT without a PLT
  (`-fno-plt`) or a function pointer loaded from the GOT is not an import
  binding's; bind the definition (or the library) for those.
- **A library swap keeps the library loaded**, so `ld.so` can resolve
  against it and its initialisers run.
- **One ISA per gexc**, as everywhere in gex: eject a session with the gexc
  that ran it (`gexc` for AArch64, the `x86` build for x86-64 and i386).
- **The native runtime is the whole gexc.** The syscall list and the
  single-process rule are enforced; the code for the rest is still there.
- **Replacement calls go through the host** (natively a host call, in the
  browser a JS call), not a direct wasm call: one round trip per call. Fine
  for library-sized work, not for a function called in a tight loop.
- Distributing a project is distributing the program's code and data (and
  its libraries'): that is the user's licensing responsibility. `gexc eject`
  says so and never publishes anything.

## Status

- v0 (M4): eject, build, run; library replacement by symbol.
- v1 (M5): overrides by offset with a bytes hash and stale detection;
  import and whole-library binding; the web build and launchers; the
  runtime-level slim runtime; `--jit-fallback` (off by default).
- `gexc eject decompile` (M5 item 5): the function tier's C as an editable
  override (the `"entry"` ABI).
- Features (`user/eject.json` `"features"`) and W3 pages: `compositor` and
  `gl` switch; the rest wait for gex-kernel and shim feature gating.
