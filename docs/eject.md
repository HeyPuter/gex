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

This is eject **v0** (merge plan M4). What v1 (M5) adds is at the end.

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
   - translates each program image into a tier-1 module
     (`build/modules/NAME-HASH.wasm`) with gex's library intrinsics (the
     `mem*` functions) and the replacements;
   - copies `rootfs/` and `user/files/` into `build/rootfs/`;
   - writes `build/build.json`, including a hash of everything a user can
     change (`gex-project.json`, `user/eject.json`, replacement modules).

**Coverage** (`user/eject.json` `"coverage"`):

- `"profile"` (default): what discovery finds from the symbol table and the
  data, plus every block the recorded session ran (`profile.json`), with every
  page those touch translated whole.
- `"exhaustive"`: every decodable address. Bigger, and nothing reachable by a
  static path can be missing.

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
gexc: gex failed at guest pc=0x...: gex-eject: code at 0x... (/path + 0x...) was not
translated when the ejected project ... was built: the recorded session never reached
it and discovery did not find it. Record a session that reaches it and re-eject, set
"coverage": "exhaustive" in user/eject.json, or run with --jit-fallback
```

Likewise a program the project has no module for (an `execve` of a program
the session did not run) fails naming it, and a project changed since its
build (`user/eject.json`, a replacement module) refuses to run until
`build.sh` has been run again.

`--jit-fallback` ships nothing new in v0: the runtime is the full gexc, so it
just turns runtime translation back on.

## Library replacement

A replacement serves a guest function with a user wasm function. In
`user/eject.json`:

```json
{
  "replace": [
    { "object": "ack", "sha256": "sha256:3c1f...", "symbol": "ack",
      "export": "my_ack", "abi": "c" }
  ]
}
```

| Field | Meaning |
| --- | --- |
| `object` | The object defining the function: its guest path, or its file name when that is unambiguous. |
| `sha256` | The object's hash (from `gex-project.json`). Required. If the object changes, the build fails and names the stale replacement instead of binding it to a function that may have changed. |
| `symbol` | The function's symbol. It must name exactly one function in the object's text. |
| `export` | The wasm function (default: `symbol`). |
| `wasm` | The module, relative to the project (default `build/user.wasm`, which `build.sh` compiles from `user/src/*.c`). Any producer's module works if it meets the rules below. |
| `abi` | `"c"` (default) or `"ctx"`. |

**How it is bound.** The build puts `Intrinsic::Replace(id)` at the symbol's
address, which generalises gex's `memcpy`-style intrinsics
(`gex-xlate/src/intrinsic.rs`) from a fixed set of wasm instructions to any
user function. That block is one host call, `sysreg_write(pc,
REPLACE_SYSREG, id)`, and a return. So every way of reaching the address runs
the replacement: a direct call, a PLT stub through its resolved GOT slot, a
function pointer, a tail call. None of the original function's code runs
(entries into its middle, which a normal program never makes, still reach the
original blocks).

**The two ABIs.**

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

**Memory.** A guest address is a linear-memory offset, so a guest pointer is
a C pointer (`GEX_PTR(addr)` for a 64-bit value). The replacement runs in the
guest's memory. Its own data, heap and stacks live in
`[0x5800000, 0x7000000)`, a range the guest's `mmap` never uses
(`gex_kernel::mem::hits_runtime`; the browser shim keeps its stack there).
The first 8 MiB are data and heap, and the rest is one 128 KiB stack per
guest thread, which the runtime points `__stack_pointer` at. The region is
recorded as a mapping, so `fork` copies it.

**Module rules** (checked by the build, before anything runs):

- It may import only `env.memory`: shared, with a maximum of at least the
  guest's 65535 pages. Anything else (an Emscripten runtime's imports, WASI)
  is refused, naming the import.
- If it imports memory, it exports a mutable i32 `__stack_pointer`.
- The export exists and has a type the ABI can call.

`build.sh` compiles with:

```sh
clang --target=wasm32 -O2 -nostdlib -ffreestanding -fvisibility=default -matomics -mbulk-memory -Igen \
  -Wl,--no-entry -Wl,--export-dynamic -Wl,--import-memory -Wl,--shared-memory \
  -Wl,--max-memory=4294901760 -Wl,--global-base=92274688 \
  -Wl,--export=__stack_pointer -Wl,--export=__heap_base
```

`USER_CFLAGS` and `USER_LDFLAGS` add to it. There is no libc: the code is
freestanding.

**Example.** Replace the corpus's `ack` kernel's recursive function:

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

## Where it runs

- **Native (wasmtime):** `run.sh`. Everything above.
- **Node and the browser:** not in v0. The browser shim fails loudly on a
  replacement (`REPLACE_SYSREG` panics there) rather than skip it.

## Limits of v0

- **Statically linked programs only.** The build refuses an object whose role
  is `interp` or `library`. A dynamic program needs the closure placed at
  pinned bases and its libraries' modules installed up front.
- **Replacements are of functions defined in an executable.**
- **One ISA per gexc**, as everywhere in gex: eject a session with the gexc
  that ran it (`gexc` for AArch64, the `x86` build for x86-64 and i386).
- **The runtime is the whole gexc.** Syscalls, subsystems and the translator
  are all there; only the translator is switched off.
- **Replacement calls go through the host**, not a direct wasm call: one
  host-call round trip per call. Fine for library-sized work, not for a
  function called in a tight loop.
- Distributing a project is distributing the program's code and data (and
  its libraries'): that is the user's licensing responsibility. `gexc eject`
  says so and never publishes anything.

## v1 (merge plan M5)

- Function overrides keyed by symbol, or by (object hash, offset) plus a hash
  of the original bytes, reporting stale ones.
- `gexc eject decompile <fn>`: the function tier's C for a function, as an
  editable start.
- A slim runtime: gex-kernel subsystems behind cargo features, the syscall
  table trimmed to the session's, single-process builds.
- `--jit-fallback` as a build flag that decides whether the translator ships.
