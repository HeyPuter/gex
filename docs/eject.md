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
    eject.json          settings: replacements, coverage, arguments, start() defaults
    index.html          the web build's page: an example that calls app.mjs's start()
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

The web build is a **library**, not a page. Whatever the project (a Linux
program, an NT-layer Windows program, a W3 page), the browser half is
`app.mjs` exporting `start()`, and the page that calls it is the user's.
`gexc eject` writes an example, `user/index.html`, once and never again:

```html
<script type="module">
  import { start } from './app.mjs';
  const app = await start({ container: document.body });
  // app.exited (a promise of the exit status), app.kill(), app.resize('fit' | {w, h})
</script>
```

plus a line that shows an error. Nothing about the page is special: any
page that imports `app.mjs` and gives `start()` an element runs the program
(tests/eject/w3-shell-min.html is a hand-written one, with its own box,
arguments and onExit). gex's own test page, `web/nt.html`, is a test page
only: an ejected build never ships or names it. `web/build-eject-page.sh`,
`gexc eject build` (W3), `gexc eject` (`--web-page`) and
`web/eject-site.mjs` each fail if their output has or names it.

**`start(options)`** (web/src/app.js) resolves once the program runs, with
`{ exited, kill(), resize(size), state, describe() }`. It rejects with a
message when the run cannot start: no cross-origin isolation, no WebGPU for
a build that draws with it, a download that failed, or an option for a piece
the build left out (`start({ compositor: true })` on a `"compositor": false`
build names `"features": {"compositor": true}` in user/eject.json). Options:

| Option | What |
| --- | --- |
| `container` / `canvas` | the element the desktop fills (a canvas or a box is made in it), or the canvas to draw into |
| `size` | `'fit'` (the container's size), `{w, h}` or `'WxH'` |
| `args` | the program's arguments, an array, instead of the recorded ones |
| `env` | more environment, `['K=V']` or `{K: V}`; a name given replaces the build's |
| `cwd`, `exe` | the working directory, the program (a path in the bundle) |
| `mtimes` | the bundle's file times for the guest |
| `persist` | `false`, or `{dir, db}`: a guest directory kept in IndexedDB |
| `strace` | `true` (every process) or a pid |
| `trapstack` | a trap's wasm frames |
| `debug` | any other runtime switch, `{name: value}` (the table below) |
| `compositor`, `gl`, `translation`, `hostx`, `mem64` | `true` asks for a runtime piece (an error if the build left it out); `compositor: false` draws the windows straight into the page in a build that has both |
| `onLog(text)`, `onExit(status)` | the program's terminal output; its exit |
| `baseUrl` | where app.mjs's files are (default: beside app.mjs) |

The library never reads the page's URL: the runtime's switches come from
these options (`GEX_PAGE_SEARCH`, web/src/page-env.js), and its files from
`baseUrl` (`GEX_ASSET_BASE`), so a site can live under any path.

**Build or start.** What changes the build's output (its wasm, its bundle,
what ships) is `user/eject.json`, at build time. Everything else is a
`start()` option, whose default the build records: user/eject.json
`"start"` (`args`, `env`, `cwd`, `size`, `mtimes`, `strace`, `trapstack`,
`persist`) over the recording's, and the page's options over both. A
`"start"` key for a build setting (`"compositor"`, say) fails the build and
says where it goes. Each of gex's test page's parameters, and where it went:

| nt.html parameter | Side | Why |
| --- | --- | --- |
| `tar`, `lazy`, `pack`, `preload`, `zstd` | build | the bundle, its split, its packing and the preload list are what the site ships (`"bundle"`) |
| `cgen`, `cgenurl`, `unguarded`, `elfunits`, `wasmdlls`, `pins`, `dense`, `unitcache` | build | the units and providers: which wasm runs |
| `aot`, `fjit`, `fjitlibs` | build | run-time translation or not (`"translation"`); `start({ translation: true })` only where the build has it |
| `compositor` | build | `"features"`: the scene compositor in or out; `start({ compositor: false })` may still choose the bare desktop |
| `gl`, `glbackend` | build | `"features": {"gl"}`: Mesa shipped or not (wgpu is the only backend) |
| `hostx` | build | ships Xwayland |
| `mem64` | build | a different shim, seed and kernel |
| `exe`, `args`, `argv`, `env`, `cwd` | start | how the program starts; nothing in the build changes (`exe`, `args`, `env`, `cwd`) |
| `size` | start | the desktop's size (`size`; `resize()` later) |
| `mtimes` | start | file times for the guest (`mtimes`) |
| `persist`, `persistdb` | start | saves in IndexedDB (`persist`) |
| `persistms` | start | `debug.persistms` |
| `strace`, `stracepid`, `trapstack` | start | traces (`strace`, `trapstack`) |
| `direct`, `present`, `pointerlock`, `gllimits`, `mesaenv`, `wgslcache`, `glcapture`, `wgpustats`, `gltrace`, `glcanvas`, `sharetrace`, `xverbose`, `libs`, `single` | start | presentation and diagnostics: `debug.NAME` |
| `exittrace`, `callouttrace`, `heartbeat`, `doorstats`, `densedebug`, `tracemisses`, `attachtrace`, `liftmax`, `t1fold`, `xlatereport`, `inlinedispatch` | start | runtime diagnostics: `debug.NAME` |

**Where it lives.** A W3 or NT-layer project's library is
`web/build-eject-page.sh`'s output (app.mjs, its chunks under assets/, the
runtime's wasm under gex/, the zstd decoder under vendor/): `build/web/page`
(W3) or `runtime/web/page` (NT layer, `--web-page`). It reads the build's
launch file, `project/build/web/nt.json` beside it. `web/eject-site.mjs` lays
a site out that way (the page at `/`, the library beside it, the project's
files under `project/`), and so does the project's own `serve.mjs`. A Linux
program's `app.mjs` (generated, the single-Worker runtime below) has the same
`start()`, and its `serve.mjs` serves user/index.html at `/`.

### A Linux program's web build

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
node serve.mjs 8000        # then open http://localhost:8000/ (user/index.html)
```

`app.mjs`'s `start()` takes `container` (its output goes to a `<pre>` there
unless `onLog`), `args`, `env`, `cwd`, `strace`, `translation` (the JIT
fallback), `onExit`; `kill()` stops waiting for and hearing the run (it is
on the page's thread), `resize()` does nothing (a terminal program).

The page needs cross-origin isolation (gex's runtime uses
`SharedArrayBuffer`); `serve.mjs` sends the two headers. Replacements run
in JavaScript's hands: the user module is instantiated on the guest's memory
and called with the same `c`/`ctx` ABI as natively. `build.sh` links
`build/user.web.wasm` for it, since a module importing a shared memory will
not link to the shim's plain one (checked at build).

Checked on both guests in Node (`run.mjs`, JIT off): the corpus kernels,
static gzip and, on x86-64, the host's dynamically linked glibc
gzip, and a program whose whole library is swapped for wasm. Checked in
headless Chromium: the `ack` page, with arguments, and
(`tests/eject/elf-page.mjs`, 2026-10-13) the example page over `app.mjs`.

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
code in function-tier units) runs on the same runtime as any demo.
Ejecting one makes it the app library (above), built with the features the
user keeps, and a standalone site around the project's page:

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
node tests/eject/w3-page.mjs site --placed   # and Wine moves, sizes and stacks its windows
node tests/eject/w3-page.mjs site --shell tests/eject/w3-shell-min.html   # a hand-written page
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
  (`"kind": "w3"`), and the page, `user/index.html` (written once, the
  user's). `page_source` names the gex checkout's `web/`: the library is
  built at build time, because the features decide what goes into it.
- **The build** (`gexc eject build`) runs `web/build-eject-page.sh` with
  `GEX_FEATURES_OFF`, puts the project's runtime wasm in it, drops what a
  feature off ships (Mesa for `gl`), checks the bundler really left out
  what `compositor` off names, checks the library has no test page in it,
  and writes `build/web/` (`page/`: the app library, `rootfs.tar`, `units/`,
  `w3.json`: the launch, with user/eject.json `"start"`).
- **The site** (`web/eject-site.mjs`) is the project's page at `/`
  (user/index.html), the app library as plain files, the bundle and the
  units as `FILE.zst` with a preload list, `pack.json` (their sizes and the
  build id) and the hosting files (`serve.mjs`, `_headers`). There is **no
  service worker**: the host sends the isolation headers (COOP
  `same-origin`, COEP `require-corp`), and the library fetches each packed file and
  decodes it itself, straight into the one buffer that is then moved to
  pid 1's Worker, which lets go of it once the kernel has the files (moved
  on to a closed `MessagePort`: `ArrayBuffer.prototype.transfer(0)` alone
  leaves a buffer that arrived by `postMessage` resident until the Worker
  collects). The first load is the only one, and one copy of the bundle is
  resident: the kernel's. The browser's HTTP cache does the caching
  (`FILE.zst?v=BUILD`).

Everything else is the page as gex runs it: several processes, the
kernel, run-time translation behind the units.

### The bundle split

A W3 bundle is a whole Wine prefix plus Wine itself: for Paint 2509 files
(447 MiB), of which a run to the window opens 221. `user/eject.json`
`"bundle": "split"` (the W3 template's setting; a config without it means
`"whole"`) ships only those in the site's bundle:

- **Recording:** the kernel marks every file something opens
  (`TmpFs::opened`, `k_fs_opened` in shims/kernel), the test page reads the set
  from a side Worker (`__ntOpened()`, web/src/fs-opened.js) and
  record-w3.mjs keeps it as the session's `files_read`.
- **Build:** `gexc eject build` puts that list in build/web/w3.json
  (`"bundle": {"split": true, "read": [...]}`); a split without a read set
  (an older recording) fails the build and says so.
- **Site:** web/eject-site.mjs keeps the read files (and the loader) in
  rootfs.tar and makes every other file a lazy file (app.js `lazy`):
  stored once by content as `project/build/web/lazy/SHA`, listed with its
  size and time in `lazy.json`. A guest that reads one gets it from the host
  in 1 MiB ranges, synchronously, the first time; the site's serve.mjs
  answers ranges (a host without them still works, sending the whole file
  per read). A lazy file's time is the bundle's (`mtimes=1`: Wine's wineboot
  rebuilds a prefix whose wine.inf looks newer than its install).

Nothing the program needs is lost: what the recording did not open is a
fetch away. What it costs is a wait the first time an unrecorded path reads
a file (Paint's Ctrl+P reads winspool.drv and compstui.dll that way). Record
the paths users take (for now: run them before the recorder's settle ends)
to bring them into the bundle.

Paint (the same recording ejected twice, `--strip --prune --dedupe`;
worldmachine, bench, headless Chromium, 3 alternated runs each):

| | whole | split |
| --- | --- | --- |
| download | 76.5 MB | 54.2 MB (29% less) |
| to the window | 22.0, 22.0, 22.0 s | 21.0, 20.9, 21.1 s (1.0 s sooner) |
| browser footprint at settle | 3.39-3.41 GiB | 3.01-3.05 GiB (0.37 GiB less) |
| lazy reads to the window | | 0 |

### Cold code

A unit holds every function of its image, and a run to the window calls
about one in twelve of them (Paint: 10,040 of 116,343 across 59 units).
`user/eject.json` `"cold"` (the W3 template's is `"chunk:256"`; a config
without it means `"none"`) ships only those functions up front. The rest
are fetched the first time something calls one. Everything is still
compiled ahead of time at build time, and nothing is lifted in the page.

- **Coverage.** Build with `"cold": "instrument"`. Every guest function of
  every unit then calls `gexcov.hit(slot)` on entry (`gexc cgen-split
  instrument`), and gex.mjs flushes the slots hit as `[gex-cov]` lines.
  Record that site with `record-w3.mjs URL OUT --serve SITE --coverage-into
  REC/w3-session.json`, which adds the run's functions to the recording the
  project is ejected from. Runs add up: record the paths users take. Then
  eject again (`gexc eject REC ...`; user/ is kept) and set the clustering.
- **Build.** For each unit, `gexc eject build` keeps the functions in the
  recording's `coverage` in its primary module. Every other slot gets a
  stub that calls `gex.cold(cluster)` and then tail-calls its own table
  slot. The other functions go into cluster modules `cold/N.wasm`
  (gex_cgen::split). Each cluster module is a PIC module with the
  primary's imports. It fills its slots with `table.init` from a passive
  segment. A direct call from one module to a function in another becomes
  a `call_indirect` through the slot. The entry list does not change, so
  the shim sees the same unit.
- **Site.** web/eject-site.mjs packs each unit's clusters into `cold.bin`,
  one zstd frame per cluster, plus `cold.idx`, a zstd-packed list of frame
  and unpacked lengths. On a cluster's first call the Worker reads the
  index once per unit, fetches the frame with a synchronous Range request,
  decodes it with zstddec, compiles it, shares the Module with its
  siblings and instantiates it into the table. The Worker that runs the
  call blocks for that time, as for a lazy file.

Clustering (`gex_cgen::split::Clustering`) sets how much one fetch brings:

- `function`: each cold function is its own module.
- `graph[:KiB]`: the cold functions a root reaches by direct calls.
- `chunk[:KiB]`: consecutive cold functions by address, up to that much
  code. An image's code from one source file sits together, so a dialog's
  functions mostly come as a few chunks.
- `graphchunk[:KiB]`: the graph's clusters, then packed by address.
- `unit`: all of a unit's cold code in one module.

Paint's first use of three cold features, from the action to the dialog's
window, with the same recording and hot set in every split site
(worldmachine, bench, headless Chromium, localhost, 3 alternated runs each):

| | whole | chunk:256 | chunk:64 | graph:64 |
| --- | --- | --- | --- | --- |
| download (units) | 30.0 MB | 4.37 MB | 4.38 MB | 4.52 MB |
| cold, fetched on use | | 31.2 MB, 619 clusters | 35.2 MB, 2406 | 79.7 MB, 77,874 |
| to the window | 16.4, 16.4, 16.1 s | 16.5, 16.5, 16.4 s | 16.5, 16.2, 16.5 s | 16.8, 16.7, 16.7 s |
| Resize/Skew (Ctrl+W) | 594, 508, 516 ms | 799, 686, 698 ms (34 fetches, 1.9 MB) | 767, 764, 764 ms (46, 0.7 MB) | 944, 955, 1042 ms (105, 0.2 MB) |
| Open (Ctrl+O) | 1865, 1506, 1976 ms | 2269, 2147, 2253 ms (34, 1.6 MB) | 2542, 2478, 2582 ms (95, 1.4 MB) | 3833, 3818, 3855 ms (340, 1.0 MB) |
| Print (Ctrl+P) | 994, 1020, 995 ms | 1010, 1009, 1062 ms (6, 0.4 MB) | 995, 1059, 1066 ms (10, 0.2 MB) | 1094, 1004, 988 ms (22, 0.07 MB) |

On localhost a fetch costs 4-6 ms: Range, decode, compile. On a real
network it costs a round trip, so the number of fetches decides the wait.
Computed from an instrumented run's per-action coverage, modelled as
(round trip + 1 ms) per fetch plus bytes at 50 Mbit/s:

| clustering | clusters | cold MB | Open: fetches, MB | Open at 3 ms / 50 ms round trip |
| --- | --- | --- | --- | --- |
| function | 106,303 | 94.1 | 788, 0.98 | 3.3 s / 40.3 s |
| graph:64 | 77,874 | 79.7 | 424, 1.36 | 1.9 s / 21.8 s |
| chunk:64 | 2406 | 35.2 | 149, 2.35 | 1.0 s / 8.0 s |
| chunk:256 | 619 | 31.2 | 71, 3.97 | 0.9 s / 4.3 s |
| chunk:1024 | 181 | 28.7 | 31, 5.98 | 1.1 s / 2.5 s |
| unit | 59 | 26.1 | 11, 13.4 | 2.2 s / 2.7 s |

Fetching per function is too chatty: the Open dialog alone calls 788 cold
functions. The call graph alone hardly helps either, because most calls
between them go through tables (vtables, callbacks) that the direct-call
graph cannot see. Grouping by address keeps the fetch count low and still
compresses well (whole-unit frames 26 MB, per-function frames 94 MB).
`chunk:256` is the default. Behind a slow link, `chunk:1024` makes fewer
round trips. A function the recording missed at startup is a fetch, too:
add runs to the coverage.

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
- The W3 bundle split (`"bundle": "split"`): the files the recorded run
  opened in the bundle, the rest lazy (Paint: 29% less download, 0.37 GiB
  less memory, 1.0 s sooner to the window). Not yet: units of only the
  functions a recording ran (the units are most of what is left).
