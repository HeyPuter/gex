# gex-eject

`gexc eject` turns what a gex run learned into a project that builds and
runs on its own: the program's code translated ahead of time, the files it
needs, a runtime, and a place for your own code (library replacements) that
survives regeneration.

```sh
gexc run --record rec ./prog 5          # 1. record a session
gexc eject rec --out prog-project       # 2. write the project
prog-project/build.sh                   # 3. build it (translate, bind replacements)
prog-project/run.sh                     # 4. run it: prog 5, as recorded
prog-project/run.sh 7                   #    or with other arguments
```

The code is this crate (the project model, the eject, the build, the run),
`shims/native/src/aot.rs` (the runtime side: prebuilt modules and
replacement calls) and `crates/gex-xlate/src/intrinsic.rs`
(`Intrinsic::Replace`).

## The project

```text
prog-project/
  gex-project.json    what the project is                          generated
  rootfs/             the closure and the files the session read    generated
  session/            the recorded session and profile              generated
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
  same bytes.
- **`user/`** is created from templates when it is missing and never written
  again. A re-eject into an existing project keeps it.
- **`build/`** is deleted by an eject, because it was built from what the
  eject replaces.
- An eject refuses to write into a non-empty directory that is not a project.

### What goes into `rootfs/`

- **Objects** (the executable, and for a dynamic program the interpreter and
  libraries), each read from where the run read it and checked against the
  hash the session recorded. A changed file fails the eject.
- **Files the session read**, at their guest paths, with their hashes in
  `gex-project.json`.
- **Directories** it opened, and its working directory, are created.

Left out, and listed in `gex-project.json`:

- `missing`: paths the session probed and did not find. They stay missing.
- `outputs`: files it created or only wrote.
- `/proc`, `/dev`, `/sys`: gex makes those.

A file the session read from `/tmp` fails the eject: a project runs under a
sysroot, where gex mounts a fresh tmpfs on `/tmp`.

**Adding files:** put them in `user/files/` at their guest path
(`user/files/etc/foo.conf` is `/etc/foo.conf`) and rebuild. They win over
`rootfs/`. Editing `rootfs/` itself fails the build: it is generated, and
its hashes are checked.

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

- `"profile"` (default): every block the recorded session ran, with every
  page those touch translated whole, and nothing followed off them onto
  fresh pages: what was seen. An object the session ran no code in gets its
  entry point only. Without a profile this is `"static"`.
- `"static"`: `"profile"` plus what discovery finds from the symbol table
  and the data. Covers paths other inputs take, at a much larger build:
  for a dynamically linked glibc program, several times the code of
  `"profile"`.
- `"exhaustive"`: every decodable address.

**Dynamically linked programs.** The closure is placed the way `gexc build
--closure` places one: the executable at `PIE_BASE`, the interpreter at
`INTERP_BASE` (both where gex's loader puts them), and the libraries up from
`MMAP_BOTTOM` in the session's order, 64 KiB aligned. The program's module
owns the memory and dispatch tables, and every other object's module
continues its slot numbering. At run time the libraries are pinned (as
`gexc run --pin`), and their modules are installed into the runtime's module
registry before the program starts, so threads and forked children get them
too. A pin that does not land is reported under `--verbose`; code at the
wrong address then misses loudly.

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
branch to code the build does not have fails loudly, naming the address and
the image, and says how to cover it: record a session that reaches it and
re-eject, set `"coverage"` to `"static"` or `"exhaustive"`, or run with
`--jit-fallback`. Code the program generates at run time (an anonymous
executable mapping) is reported as such: only `--jit-fallback` can run it.

Likewise a program the project has no module for (an `execve` of a program
the session did not run) fails naming it, and a project changed since its
build (`user/eject.json`, a replacement module) refuses to run until
`build.sh` has been run again.

`--jit-fallback` (on a run), or `"jit_fallback": true` in `user/eject.json`
(for every run of the build, native and web), turns run-time translation
back on. It is off by default.

## Library replacement

A replacement serves a guest function with a wasm function of your own. In
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
| `abi` | `"c"` (default), `"ctx"` or `"entry"`. |

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

**Swapping a whole library.** `{"library": "libsq.so"}` binds every function
the library exports. A function the module does not provide is bound to a
placeholder that stops the run naming it, when (and only when) it is
called; the build lists them. An IFUNC export fails the build (its address
is a resolver). The library itself is still loaded, so `ld.so` resolves
against it and its initialisers run; none of its exported functions' code
does.

**How it is bound.** The build puts `Intrinsic::Replace(id)` at the bound
address (the definition, or for an import the PLT stub). That block is one
host call and a return, so every way of reaching the address runs the
replacement: a direct call, a PLT stub through its resolved GOT slot, a
function pointer, a tail call. None of the original function's code runs
(entries into its middle, which a normal program never makes, still reach
the original blocks).

**The three ABIs.**

- `"c"`: an ordinary C function, typed by its wasm signature. The runtime
  passes the guest's integer argument registers (`x0`-`x7`, or `rdi`, `rsi`,
  `rdx`, `rcx`, `r8`, `r9`) to its i32/i64 parameters in order, and its
  floating-point argument registers (`v0`-`v7` or `xmm0`-`xmm7`) to its
  f32/f64 ones, and writes the result to `x0`/`rax` (an i32 zero-extended)
  or `v0`/`xmm0`. More arguments than registers is a build error; use
  `"ctx"`.
- `"ctx"`: `void f(gex_ctx c)`. The function reads and writes the guest's
  registers through the context itself, with the macros in
  `gen/gex_replace.h` (`GEX_ARG(c, i)`, `GEX_RET(c)`, `GEX_SP(c)`,
  `GEX_STACK_ARG(c, i)`, `GEX_FARG_F64(c, i)`, ...). It must not return for
  the guest: the block does that after it.
- `"entry"`: `void f(int ctx)` that *is* the function, its return included:
  on return, the context's PC says where the guest goes next, and the block
  dispatches there. What `gexc eject decompile` writes (below). Only for a
  function bound by `symbol` or `offset`.

Any replacement module may import, besides `env.memory`, the `gex.*` host
functions translated code calls (`syscall`, `safepoint`, `sysreg_read`,
`sysreg_write`, `cache_op`, `fma`, `illegal`, `breakpoint`, `dispatch_miss`,
as `gen/gex_abi.h` declares them). They are the very functions the guest's
own modules get, so the syscall allow-list applies to them too; a host call
that stops the guest (an exit, a fork, a signal) unwinds through the
replacement exactly as through translated code, which is sound when the
replacement left the context architectural first.

**Memory.** A guest address is a linear-memory offset, so a guest pointer is
a C pointer (`GEX_PTR(addr)` for a 64-bit value). The replacement runs in
the guest's memory. Its own data, heap and stacks live in
`[0xF0000000, 0xF1000000)`, above `MMAP_TOP`, where the guest is never given
an address: the first 8 MiB are data and heap, then one 128 KiB stack for
each of up to 64 guest threads, which the runtime points `__stack_pointer`
at. Natively the region is recorded as a mapping, so `fork` copies it; in
the browser the memory is grown to cover it at the first replacement call.

**Module rules** (checked by the build, before anything runs):

- It may import only `env.memory` and the `gex.*` functions above, the
  memory with a maximum of at least the guest's 65535 pages: shared for the
  native runtime, unshared for the web build. Anything else (an Emscripten
  runtime's imports, WASI) is refused, naming the import.
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

An example, replacing a program's `ack` with the `"c"` ABI:

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
binds it (with the object's hash and the function's bytes' hash, so a
changed binary reports the override as stale):

```text
$ gexc eject decompile . ack ack
user/src/decompiled_ack.c: 1 function of C; bind it in user/eject.json with:
{ "object": "ack", "sha256": "sha256:723b...", "symbol": "ack",
  "bytes": "sha256:60bd...", "export": "decompiled_ack", "abi": "entry" }
```

- **The C** is exactly what the function tier (`gex-cgen`) emits for the
  function, from the same recovery, summaries and filters, as one standalone
  unit: guest registers are C locals, guest memory goes through
  `gen/gex_abi.h`'s helpers (the tier-1 bounds semantics), system calls and
  safepoints through the `gex.*` host functions. Its export,
  `decompiled_<name>(int ctx)`, is bound with the `"entry"` ABI.
- **It round-trips:** unmodified and bound, it gives the original's output,
  natively and in the web build.
- **What it does not hold, it leaves** as the function tier does: a call to
  a function not in the file writes every register back, sets PC to the
  callee and returns; the rest of the function then runs as the original
  translated code once the callee returns into it. So an edit after such a
  call does not take effect. `--callees` puts every function it reaches by
  direct call (that the tier takes) into the file too, so those calls stay
  in C. Indirect calls go through the file's own `gex_call_known` and leave
  when the target is not in it.
- **Refused, loudly:** a function recovery did not find, one the tier does
  not take (self-modifying pages, x87 statements with no C lowering yet),
  and 32-bit (i386) code.
- The file is yours: decompile refuses to overwrite it without `--force`.

## The web build

The web build is a **library**, not a page. Whatever the project (a Linux
program or a 32-bit Wine page), the browser half is `app.mjs` exporting
`start()`, and the page that calls it is yours. `gexc eject` writes an
example, `user/index.html`, once and never again:

```html
<script type="module">
  import { start } from './app.mjs';
  const app = await start({ container: document.body });
  // app.exited (a promise of the exit status), app.kill(), app.resize('fit' | {w, h})
</script>
```

Nothing about the page is special: any page that imports `app.mjs` and
gives `start()` an element runs the program
(`tests/eject/wine32-shell-min.html` is a hand-written one, with its own box,
arguments and `onExit`).

**`start(options)`** resolves once the program runs, with
`{ exited, kill(), resize(size), state, describe() }`. It rejects with a
message when the run cannot start: no cross-origin isolation, no WebGPU for
a build that draws with it, a download that failed, or an option for a
piece the build left out (`start({ compositor: true })` on a
`"compositor": false` build names `"features": {"compositor": true}` in
user/eject.json). Options:

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
| `debug` | any other runtime switch, `{name: value}` |
| `compositor`, `gl`, `translation`, `hostx`, `mem64` | `true` asks for a runtime piece (an error if the build left it out); `compositor: false` draws the windows straight into the page in a build that has both |
| `onLog(text)`, `onExit(status)` | the program's terminal output; its exit |
| `onProgress(p)` | the start-up's progress, for the page's loading screen (below); none by default |
| `ready` | `'frame'` (the default): the start-up is over at the first frame shown; `'manual'`: when the page calls the handle's `ready()` |
| `baseUrl` | where app.mjs's files are (default: beside app.mjs) |

### The loading screen

The library draws nothing over the desktop while it starts; it says how far
along it is, and the page draws that. `onProgress(p)` is called whenever
the start-up moves (a few times a second at most while bytes arrive; at
once on a phase change, when a file is whole, and when a fetch of the
running program starts or ends), and the handle's `state.progress` holds
the same, for a page that polls:

| `p.` | What |
| --- | --- |
| `phase` | `'download'`, `'compile'`, `'boot'`, `'first-frame'`, `'ready'`, in that order and never back; then `'exited'` or `'failed'` |
| `download` | `{done, total}` bytes as sent (compressed), `{filesDone, files}`; `cached`, the bytes of `done` the page already had (pieces kept from an earlier visit) |
| `compile` | `{done, total}` units compiled ahead (they compile as they arrive) |
| `at` | each phase's start, ms since `start()` |
| `elapsed` | ms since `start()`, at this call |
| `error` | with `'failed'`: why |
| `fetch` | after the start, what the program is fetching, or `null` when nothing is in flight: `{kind, name, done, total, inflight}`. `kind` `'file'`: a lazy file, `name` its file name, `done` its bytes read so far, `total` its size. `kind` `'code'`: a unit of translated code, `name` the module it is for (`''` if not known), `done` its bytes once in, `total` 0. `inflight`: how many fetches are under way |
| `boot` | from `'boot'` on, what the program has reached: `units` `{done, total}`, the build's units installed in some process so far, of all it ships; `programs` `{done, total}`, the same for the `.exe` images; `program`, the latest program started, and `image`, the latest image loaded (file names, or `null`). A build without units has totals of 0 |
| `log` | the latest events, oldest first (at most 50): `{t, kind, text}`, `t` in ms since `start()`, `kind` `'phase'`, `'program'` (`Started X.exe`), `'image'` (`Loaded X.dll`) or `'fetch'` (the first fetch of a lazy file or a unit's code) |

The totals are whole from the first call: a Wine page counts its bundle and
units from the site's pack, a Linux program's page from its manifest. A
Linux program without windows downloads until the launcher has read every
file, then goes to `'boot'` and `'ready'` together.

`'boot'` is the program's first process started, `'first-frame'` a window's
first pixels in the page. `'ready'` is the first frame unless the build's
`"start": {"ready": "manual"}` or the page's `ready: 'manual'` says the page
decides: a program whose first window is a splash, or a desktop that paints
its wallpaper long before its taskbar, calls `handle.ready()` when what it
waits for is there (`describe()` says what the windows are).

The library never reads the page's URL: the runtime's switches come from
these options, and its files from `baseUrl`, so a site can live under any
path.

### Build or start

What changes the build's output (its wasm, its bundle, what ships) is
`user/eject.json`, at build time. Everything else is a `start()` option,
whose default the build records: user/eject.json `"start"` (`args`, `env`,
`cwd`, `size`, `mtimes`, `strace`, `trapstack`, `persist`, `ready`) over the
recording's, and the page's options over both. A `"start"` key for a build
setting (`"compositor"`, say) fails the build and says where it goes.

### A Linux program's web build

`gexc eject ... --web-shim gex_browser.wasm` vendors gex's browser runtime
into `runtime/web/`: the shim you give it (built from the same source, for
the same guest: `shims/browser`, `wasm32-unknown-unknown`, with
`shims/browser/build.sh`'s link flags) and the `gex.mjs` this gexc was
built with. `build.sh` then also writes `build/web/`:

- the program translated for the browser's single-Worker shim (a plain,
  unshared memory). A static program is one image module. A dynamic one is
  run the way gex's own pages run a closure (`gexc build --closure
  --shared-image`): every object, the executable included, is a
  demand-filled library module, and the process starts from a seed module;
- `manifest.json`: modules, files (`build/rootfs`), arguments, environment,
  pins, replacements, the guest ABI the replacements are called with, and
  the syscall allow-list.

Launchers, sharing `runtime/web/eject.mjs`:

```sh
node run.mjs [--jit-fallback] [--strace] [--] [ARGS...]   # Node
node serve.mjs 8000        # then open http://localhost:8000/ (user/index.html)
```

`app.mjs`'s `start()` takes `container` (its output goes to a `<pre>` there
unless `onLog`), `args`, `env`, `cwd`, `strace`, `translation` (the JIT
fallback) and `onExit`; `kill()` stops waiting for and hearing the run (it
is on the page's thread), `resize()` does nothing (a terminal program).

The page needs cross-origin isolation (gex's runtime uses
`SharedArrayBuffer`); `serve.mjs` sends the two headers. Replacements are
instantiated on the guest's memory and called with the same `c`/`ctx` ABI
as natively. `build.sh` links `build/user.web.wasm` for them, since a module
importing a shared memory will not link to the shim's plain one.

Limits of a Linux program's web build:

- **One process, one thread.** A session that forked or ran threads gets a
  manifest that says so, and the launchers refuse it.
- **File times** are the bundle's (`manifest.json` `mtimes`, from
  `build/rootfs`), and a file the guest writes is stamped when it is
  written.

## The slim runtime

- **System calls:** only those the recorded session made are allowed, by
  canonical number, natively and in the browser. Anything else stops the
  run, naming it and the setting that allows it. `user/eject.json`:
  `"allow_syscalls": ["mmap"]` adds some (by name or number);
  `"syscalls": "all"` turns the list off.
- **Single process:** a session that never forked gives a runtime that
  refuses `fork` (natively; the web build is single-process anyway).

## Features

An ejected build starts as the whole runtime a gex page runs, and
`user/eject.json` `"features"` takes parts out of it: each is `true` (kept,
the default) or `false` (left out). The list is `src/features.rs`; a
feature switched off that no build can remove yet fails the build and says
why, rather than doing nothing.

| Feature | What it is | Off |
| --- | --- | --- |
| `compositor` | the scene compositor: every window in one WebGPU canvas, drawn by a worker, with server-side decorations, its own cursor and hit testing | each window is its own canvas in the page, laid out by `web/src/bare-desktop.js` and composited by the browser; no decorations (Wine draws its own frames). The compositor's code is not in the bundle |
| `gl` | the guest's OpenGL: Mesa on WebGPU | the page ships no Mesa |
| `multiprocess` | more than one guest process | not yet: a Wine program is several processes |
| `translation` | run-time translation (tier-1, the function JIT) behind the units | not yet: the translator is not a separate build |

Only a 32-bit Wine page's build applies them so far; a Linux project with a
feature off fails its build.

## 32-bit Wine pages

A 32-bit Wine program (Wine as native wasm32: every Wine process a guest
process running `wine`, its PE side in `winepe.wasm`, the Windows images'
code in function-tier units) runs on the same runtime as any gex page.
Ejecting one makes it the app library (above), built with the features you
keep, and a standalone site around the project's page:

```sh
# 1. record: run the page to its window (headless Chromium); --boot-units-out
#    also lists the images whose units it used shortly after the window
node tools/eject/record-wine32.mjs http://127.0.0.1:8000/ rec --serve SITE --bundle app.tar \
  --boot-units-out rec/boot-units.txt
# 2. eject: the bundle, the units it used, the runtime, a build step
gexc eject rec --out app-project --web-src web --unit-cache UNITS --features -compositor
# 3. build: the page with those features, the site's inputs in build/web
app-project/build.sh
# 4. the static site
node web/eject-site.mjs app-project site --strip --prune --dedupe \
  --pieces --boot-units rec/boot-units.txt
node tests/eject/wine32-page.mjs site      # check: the site reaches the program's window
```

- **The recording** is how the page was started (exe, arguments,
  environment, working directory, size, page parameters) and what every
  Worker reported: the processes (each one's main image and command line),
  the units each image used and the images that had none, and the native
  wasm32 modules (`wine`, `winepe.wasm`, wasm DLLs) by content key.
- **The project** holds the recording (`session/`), the bundle
  (`bundle/rootfs.tar`, hash checked), the units (`units/KEY/`), the
  browser shim, kernel and seed (`runtime/web/gex/`), `gex-project.json` and
  the page, `user/index.html` (written once, yours). `page_source` names the
  gex checkout's `web/`: the library is built at build time, because the
  features decide what goes into it.
- **The build** (`gexc eject build`) runs `web/build-eject-page.sh` with the
  features switched off, puts the project's runtime wasm in it, drops what a
  feature off ships (Mesa for `gl`), checks the bundler really left out what
  `compositor` off names, and writes `build/web/` (`page/`: the app library,
  `rootfs.tar`, `units/`, and the launch file with user/eject.json
  `"start"`).
- **The site** (`web/eject-site.mjs`) is the project's page at `/`, the app
  library as plain files, the bundle and the units as `FILE.zst` with a
  preload list, `pack.json` (their sizes and the build id) and the hosting
  files (`serve.mjs`, `_headers`). There is no service worker: the host
  sends the isolation headers (COOP `same-origin`, COEP `require-corp`), and
  the library fetches each packed file and decodes it itself, straight into
  the buffer that is then moved to the first process's Worker. One copy of
  the bundle stays resident: the kernel's. The browser's HTTP cache does the
  caching (`FILE.zst?v=BUILD`).

Everything else is the page as gex runs it: several processes, the kernel,
run-time translation behind the units.

### The bundle split

A Wine bundle is a whole Wine prefix plus Wine itself, of which a run to the
window opens a small part. `user/eject.json` `"bundle": "split"` (a config
without it means `"whole"`) ships only those files in the site's bundle:

- **Recording:** the kernel marks every file something opens, and the
  recorder keeps the set as the session's `files_read`.
- **Build:** `gexc eject build` puts that list in the launch file; a split
  without a read set fails the build and says so.
- **Site:** `web/eject-site.mjs` keeps the read files (and the loader) in
  `rootfs.tar` and makes every other file a lazy file: stored once by
  content as `project/build/web/lazy/SHA`, listed with its size and time in
  `lazy.json`. A guest that reads one gets it from the host in 1 MiB ranges,
  synchronously, the first time; the site's `serve.mjs` answers ranges (a
  host without them still works, sending the whole file per read).

Nothing the program needs is lost: what the recording did not open is a
fetch away. What it costs is a wait the first time an unrecorded path reads
a file. Record the paths users take to bring them into the bundle.

### Resources

`"resources": "trim"` (a config without the setting means `"whole"`) zeroes,
in the site's bundle, every resource of every PE that no lookup the site's
Wine makes can return (`src/rsrc.rs`). No file changes size: a resource
directory loses entries, rewritten in place, and what nothing points at any
more is zeros, which cost the download nothing.

- **Languages.** Wine looks a language up in a list made from the request,
  the neutral language, the thread, user and system locales and English,
  and takes the first entry only when none of them is there. The site's
  locale is the page environment's (`LANG`, none: en-US), so an entry whose
  primary language is neither neutral nor the locale's goes, unless it is
  all its name has. A program that asked for another language by number
  would get not-found.
- **Ribbon images.** A ribbon's markup (`UIFILE`) names each command's
  images by size and minimum DPI, and the ribbon takes the highest DPI not
  above the screen's (the prefix's `LogPixels`). A bitmap named only by
  records the ribbon never takes, and referenced nowhere else in the image,
  goes.
- **Bytes no directory reaches**: what the rules above dropped, and copies
  a resource editor left behind.

An image is left whole when a relocated word or another data directory
points into what would be zeroed, or when it has code and no relocations. A
unit's key hashes every byte of its image, so a trimmed image's unit is
shipped under the key the page computes for the trimmed image. The build
first checks that it computes the recorded key for the image as it was;
only the project's own gexc (`runtime/gexc`, which `build.sh` runs) does,
since a key hashes the sources of the gexc that made it. Where the check
fails the image is shipped whole, and the build says so.

### Cold code

A unit holds every function of its image, and a run to the window calls a
small fraction of them. The default (`"cold": "none"`) ships every unit
whole, in one archive: `web/eject-site.mjs` tars every unit into
`project/build/web/units.tar`, packed as one zstd file. The page decodes it
once and compiles every module while the bundle downloads; nothing is
fetched on a call.

A split is opt-in: a clustering in `"cold"` ships only the functions the
recording ran up front. The rest are fetched the first time something calls
one. Everything is still compiled ahead of time at build time, and nothing
is lifted in the page.

- **Coverage.** Build with `"cold": "instrument"`. Every guest function of
  every unit then reports itself on entry. Record that site with
  `record-wine32.mjs URL OUT --serve SITE --coverage-into REC/wine32-session.json`,
  which adds the run's functions to the recording the project is ejected
  from. Runs add up: record the paths users take. Then eject again
  (`user/` is kept) and set the clustering.
- **Build.** For each unit, `gexc eject build` keeps the functions in the
  recording's coverage in its primary module. Every other slot gets a stub
  that loads its cluster and then tail-calls its own table slot. The other
  functions go into cluster modules (`gex-split`), position-independent
  modules with the primary's imports that fill their own slots. The entry
  list does not change, so the runtime sees the same unit.
- **Site.** `web/eject-site.mjs` packs each unit's clusters into `cold.bin`,
  one zstd frame per cluster, plus an index. On a cluster's first call the
  Worker fetches the frame with a synchronous Range request, decodes it,
  compiles it, shares the module with its siblings and instantiates it into
  the table. The Worker that runs the call blocks for that time, as for a
  lazy file.

Clustering sets how much one fetch brings:

- `function`: each cold function is its own module.
- `graph[:KiB]`: the cold functions a root reaches by direct calls.
- `chunk[:KiB]`: consecutive cold functions by address, up to that much
  code. An image's code from one source file sits together, so a dialog's
  functions mostly come as a few chunks.
- `graphchunk[:KiB]`: the graph's clusters, then packed by address.
- `unit`: all of a unit's cold code in one module.

On a real network each fetch costs a round trip, so the number of fetches
decides the wait. Per-function fetching is too chatty, and the direct-call
graph hardly helps, because most calls between cold functions go through
tables (vtables, callbacks) it cannot see. Grouping by address keeps the
fetch count low and still compresses well. `chunk:256` is a good start;
behind a slow link, `chunk:1024` makes fewer round trips. A function the
recording missed at start-up is a fetch too: add runs to the coverage.

### Cold code on the function JIT

`"cold": "jit"` keeps the recording's coverage as clang's code and ships
nothing else: no cold cluster, no fetch. Every other slot holds a stub that
leaves through the function table for its guest address, and the page's
function JIT translates the function from the guest's code on its first
call. Tier-1 runs only what the JIT declines (writable code, an entry
inside a function it is recovering).

It wants the same coverage as a cluster split (`"cold": "instrument"` and
`--coverage-into`, above), and `"prune": "none"`: a function the proof
would leave out is cold here too, and the JIT serves it instead of
trapping.

### Services

`"services"` sets Windows services' start types in the site's prefix:
`{"PlugPlay": "demand", "StiSvc": "disabled"}`, each `boot`, `system`,
`auto`, `demand` or `disabled`. The build writes them into the bundle's
`system.reg` in place, and fails on a service the prefix has no key for.

What it is for: Wine's start-up is serial. `wineboot --init` starts
`services.exe`, which starts every auto-start service one after another and
waits for each to say it is running; only then does the first program go
on. A program that calls a demand-start service waits for that service's
process to start in the same way. In gex a process start is a fork, a
Worker and the Wine loader, so every service on that path costs time before
the window appears. A service the site never uses can come off it; a
service the program asks about at start-up (a scanner service, say) can be
`disabled` if the browser has no such device, at the cost of the features
that need it.

### Helpers

`"helpers": "strip"` leaves out of the site the parts of Wine's start-up a
game does not use. `gexc eject` writes it into a new Wine project's
`user/eject.json`, except when the program is `explorer.exe` (a desktop
shell, which runs these itself). `"keep"`, and a config without the
setting, ship the prefix as recorded.

- The services `PlugPlay`, `NDIS`, `nsiproxy` and `MountMgr` are set
  `disabled`, the ones the prefix has (see "Services"). This takes
  `plugplay.exe` and the `winedevice.exe` that hosts the drivers off the
  start-up path. An entry in `"services"` for one of them wins.
- `winemenubuilder.exe` is left out of the bundle, from the prefix and from
  Wine's own directories, along with its unit, and out of the prefix's
  `RunServices`, where `wineboot --init` starts it each session to write
  desktop menu entries, which a page has none of.

`services.exe` stays: `wineboot` starts it, and a program that asks for a
service (`rpcss` for COM between processes, say) goes through it.

### Desktop

`"desktop": "none"` runs the program with no desktop process: no
`explorer.exe /desktop`, which Wine otherwise starts from the prefix's
`RunServices` and again whenever a program finds no desktop window.
`gexc eject` writes it into a new Wine project's `user/eject.json` with
`"helpers": "strip"`. `"explorer"`, and a config without the setting, keep
Wine's desktop.

The build takes the desktop out of `RunServices` and starts the page with
`WINEGEX_NO_DESKTOP=winewayland.drv`. With that, Wine's `win32u` gives
every process the desktop window services get: the wineserver makes it and
no thread owns it. Each process loads the display driver itself, which the
desktop process would otherwise name, and sizes the desktop window to the
screen, at its start and after a display mode change.

What it is for: the desktop process is a process start, a Worker and its
memory, and Wine's other processes wait for its window. A fullscreen game
draws only its own windows. What goes with it: the desktop window has no
window procedure (messages to it are dropped), and there is no shell
(taskbar, tray), clipboard manager or local COM server registration
(`ShellWindows`) that explorer would provide. A windowed program, or one
that needs any of these, keeps `"explorer"`.

### Wineboot

`"wineboot": "skip"` boots the session without `wineboot --init`, which
Wine otherwise runs before the first program: one more process, and the
services and helpers it starts. `"run"`, the default `gexc eject` writes,
and a config without the setting, keep it.

What wineboot makes that the prefix keeps is in the bundle already. What it
makes afresh each session is not: its volatile keys, the hardware
description (`HKLM\Hardware\Description\System`: processor, BIOS), the
active computer name and the user's volatile environment. They come from
the recording: a page run with `WINEGEX_RECORD_HARDWARE=1` in its
environment has wineboot write them as `[gex-record] reg` lines, which
`record-wine32.mjs` keeps as the session's `volatile_keys`. The build
writes them into the prefix's `system.reg` and `user.reg` as ordinary keys
and starts the page with `WINEGEX_NO_WINEBOOT=1`: the first process sets up
the session's shared data and marks it booted itself. A recording without
them is an error.

What goes with it: no `services.exe`, so no service runs (a game that
starts or asks one keeps `"run"`), no `RunServices`, `RunServicesOnce` or
`RunOnce` entries, no prefix update, and no `KnownDlls` (the loader finds
those DLLs by path). It goes with `"desktop": "none"`, which wineboot would
otherwise start from `RunServices`.

### Pruning

A unit holds every function of its image, including the ones no execution
can reach. `user/eject.json` `"prune"` leaves those out: the slot keeps a
stub that names the function's guest address on the console and traps, so
a proof that is wrong says so by name instead of corrupting a run.

- `"none"` (the default) keeps every function.
- `"fanout"` proves it across all the site's images at once
  (`src/fanout.rs`). A site ships a closed set of images -- the exe and
  every DLL it can load -- so what reaches an image's code is only what the
  other images and the runtime do.
- `"static"` proves it one image at a time (`src/reach.rs`), which holds
  whatever else a site ships and is therefore far weaker: it has to treat
  every export as called and every relocated address as taken, and in a
  PE32 every absolute address is relocated. It is kept for an image whose
  callers are not in the project.

`"fanout"` iterates to a fixed point over every x86 PE in the bundle:

- **Roots:** the exe's entry, and every image's entry and TLS callbacks --
  any image in the bundle may be loaded, so its initialisation runs.
- **Exports** count when a reachable import slot binds to them (an import
  table, a delay import, or a reachable export forwarding to them); when a
  reachable address holds their name, which is what a `GetProcAddress` by
  name can find; or when nothing in the set imports their image, which the
  runtime therefore loaded by name from outside the set -- a service, a COM
  server, a proxy -- and entered at an export of. An ordinal-named export
  counts when a reachable address names its module, since nothing bounds
  which ordinal a lookup passes. `.eh_frame` and `.pdata` entries never
  root anything.
- **Relocation targets** count when the relocation *site* is in a reachable
  function or a reachable data region. A site inside a recovered function's
  extent is that function's whether or not a decoded instruction covers it:
  recovery decodes around bytes it cannot read rather than over them, and
  in an image whose read-only data is merged into `.text` those gaps hold
  the vtables and the operands recovery could not place. Where there is an
  instruction it says how far the reference reaches: an absolute memory
  operand with no index touches only the pointer-sized word at the target,
  while an address that goes into a register or onto the stack may be used
  at any offset from the object it points into, and so reaches that whole
  object. A word-sized read of an address a function covers is a reference
  to that function: what sits there is an instruction, not a pointer.
- **Data reachability propagates.** Once an object is reachable its own
  relocations are followed, so a vtable's methods, a jump table's cases, a
  handler table and a `WNDCLASS`'s `lpfnWndProc` are all reached from
  whatever reachable code registers them.
- **Objects** are bounded by the addresses something stores a pointer to: a
  region runs from one relocation target to the next, cut at section bounds
  and at every byte a recovered function covers.
- **The site's own native code** -- the Wine core, ntdll, explorer and the
  other wasm32 modules in the bundle -- is not a unit and is never pruned,
  but it reaches these images through `GetProcAddress`, so every name it
  holds seeds the lookup set.
- **Kept whole:** an image with no base relocations, with a CLR header,
  with a get-PC idiom (`call $+5`), or whose unit and image cannot be lined
  up.

`build/prune.json` carries the per-image numbers and the conservative keeps
by kind. Two ways to check a proof: `gexc eject prune <project>` prints the
report without building a site, and `gexc eject prune-check <project>
<console log>` takes a run of a site built with `"cold": "instrument"` and
names every function that ran which the proof would have left out.

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
  its libraries'): that is your licensing responsibility. `gexc eject` says
  so and never publishes anything.
