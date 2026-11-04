# gex

gex runs unmodified Linux and Windows programs in a web browser. It
translates their machine code to WebAssembly and runs it against its own
implementation of the Linux kernel ABI. That kernel is written in Rust and
compiled to WebAssembly as well. A program needs no source changes and no
rebuild, and nothing in it knows it is not on a Linux machine.

```console
$ file hello
hello: ELF 64-bit LSB executable, ARM aarch64, statically linked

$ gexc run hello
hello from aarch64
```

The browser is the target. `gexc` also runs programs natively on wasmtime,
for debugging and for building what a page loads ahead of time.

## What runs

- **Linux programs for AArch64, x86-64 and i386.** Static and dynamically
  linked ELF images, with musl or glibc. gex supports threads, `fork`,
  `exec`, signals, pipes, sockets and a full filesystem (tmpfs, host
  passthrough, `/proc`, `/dev`). Each guest process gets its own address
  space. The kernel's shared state is one module that every process's
  Workers share.
- **wasm32 Linux programs.** `toolchain/gexcc` is a C/C++ compiler for gex's
  wasm32 Linux target. It is used like `gcc`. The debdemo repository
  builds Debian packages with it and assembles a Debian rootfs around them.
- **Windows programs, through Wine.** `wine` builds Wine's own
  Unix side as wasm32 for gex, plus a ready-to-run prefix. The Windows code
  itself is translated like any other x86 code. The build is 64-bit Wine
  with WoW64, which runs 64-bit and 32-bit Windows programs in one prefix.
  The older 32-bit-only Wine build is still there, marked legacy, until it
  is removed.
- **Graphics.** `gex-wayland` is a Wayland compositor, and `gex-xwm` the X
  window manager that a rootless Xwayland needs. Wayland and X11 programs
  share one desktop. Xwayland can run as native WebAssembly in the page,
  outside any guest. A guest's OpenGL calls go through gex's own libGL and
  libEGL (`guest/gl`) to Mesa, which is compiled to WebAssembly and draws
  with the browser's WebGPU (`third_party/mesa`, Gallium's `gex_wgpu`).
  Desktop GL is supported up to 4.6.
- **In the page, or ahead of time.** The same programs run in the browser
  page (`page/`) and natively under `gexc`. An ejected project (below) runs
  only what was built for it ahead of time, as a static site whose
  JavaScript library a page of your own imports.

## How it works

```
 guest ELF/PE ─▶ gex-ld ─▶ gex-xlate ─▶ gex-isa(-x86) ─▶ gex-lift(-x86) ─▶ gex-emit ─▶ wasm
                  load      discover     decode            GIR               tier 1
                                              │
                                              ├─▶ gex-fjit   function JIT (GIR → wasm)
                                              └─▶ gex-cgen   function tier (GIR → C → clang → wasm)

 page: hosts/browser (gex-kernel per process) + hosts/kernel (shared kernel) + Mesa + Xwayland
```

- **Guest virtual address equals linear-memory offset.** gex is the
  kernel, so it allocates every guest address itself, inside the guest's
  wasm memory. A guest load is a wasm load, with no translation arithmetic.
- **Three tiers, one ABI.** Tier 1 translates each basic block into a wasm
  function, at load time or when a branch first reaches it. Guest JITs and
  late-loaded libraries need no special handling. The function JIT
  (`gex-fjit`) turns a recovered guest function into one wasm function. The
  function tier (`gex-cgen`) does the same through C and clang, ahead of
  time. All three tiers call each other through one function table.
- **Discovery is never load-bearing.** Code the translator did not find
  lands on a stub, which translates it on demand. A wrong guess costs time,
  never a wrong branch.
- **Ahead-of-time builds.** `gexc run --record` records what a run loaded
  and executed. `gexc eject` turns a recording into a project that builds
  everything ahead of time, and runs only what that build made.
  `site/eject-site.mjs` makes the web build of an
  ejected project into a static site. The site is a JavaScript library that
  your own page imports.

## Layout

| path | what it is |
|------|------------|
| `crates/gexc` | the command-line driver |
| `crates/gex-isa`, `gex-isa-x86` | AArch64 and x86-64 decoders |
| `crates/gex-ir` | GIR, the SSA IR translation goes through |
| `crates/gex-lift`, `gex-lift-x86` | instruction semantics in GIR |
| `crates/gex-f80` | exact x87 80-bit arithmetic |
| `crates/gex-emit` | the wasm writer and tier-1 code generation |
| `crates/gex-fjit` | the function JIT |
| `crates/gex-cgen`, `gex-split` | the function tier, and splitting its units into hot and cold parts |
| `crates/gex-units` | what a build ships of its function-tier units: whole, instrumented, or cut to a slot set |
| `crates/gex-xlate` | code discovery and the translation driver |
| `crates/gex-ld`, `gex-dylink` | ELF and PE loading; shared libraries for wasm32 programs |
| `crates/gex-kernel` | the Linux personality: syscalls, VFS, signals, memory map, processes |
| `crates/gex-abi`, `gex-mem`, `gex-atomic`, `gex-kmem` | the layout contract and guest-memory access |
| `crates/gex-cache`, `gex-key` | the compiled-module cache and the content keys every host agrees on |
| `crates/gex-wayland`, `gex-xwm`, `gex-gpu` | the compositor, the X window manager, WebGPU compositing |
| `crates/gex-gl` | OpenGL ES passthrough for the native host |
| `crates/gex-eject` | ejected projects, built and run ahead of time |
| `hosts/browser` | the browser host: the kernel compiled to wasm, beside each guest |
| `hosts/kernel` | the shared kernel module: process table, filesystem, pipes, sockets |
| `hosts/hostwl` | the compositor for an X server that runs in the page |
| `hosts/native` | the native host, on wasmtime |
| `guest/` | libraries installed into the guest: GL and EGL, GLX setup, `libsteam_api` |
| `page/` | the page: its source, runtime assets and build (`make page`) |
| `ports/` | the Mesa, Xwayland and Tint builds the page ships |
| `site/` | eject and demo sites built from the page, and recording a Windows program for eject |
| `toolchain/` | gexcc, the wasm32 C/C++ compiler, its sysroots, busybox, and `pins`, the version of every fetched toolchain |
| `wine/` | Wine for gex: dist, prefix, runner |
| `scripts/` | generators for the GL transport and compatibility tables, source fetching, the script lint, profile and unit helpers |
| `test/` | the runtime's tests and their runner, `test/run.mjs`, on two hosts: a headless browser page and Node |
| `third_party/` | wasmtime (with local changes), Mesa, the Khronos GL registry and CTS |

## Building

`make help` lists the targets. Every one of them builds from this tree and
the pins in it, and prints what it made on its last line.

### Run a Linux program

```console
$ make gexc
...
target/release/gexc target/x86/release/gexc

$ target/release/gexc run hello
hello from aarch64
```

`target/release/gexc` runs AArch64 guests, and `target/x86/release/gexc`
runs x86-64 and i386 guests. Some other `gexc` commands:

```console
$ gexc run --wayland SOCKET <elf>   # with the compositor, at SOCKET
$ gexc build <elf> -o out.wasm      # translate only
$ gexc disasm <elf>                 # disassemble
$ gexc ir <elf> --at 0x400abc       # the GIR for one block
$ gexc run --strace <elf>           # syscall trace
```

### Build the page

```console
$ make page                         # the multi-process build, page/dist
$ make serve                        # http://localhost:8088/
```

`page/README.md` describes the two builds (`make page-sp` is the
single-thread one), the runtime package (`make runtime`) and ejected
programs' sites.

### Run a Windows program

```console
$ make wine
...
WINE64_DIST=...
WINE64_PREFIX=...
```

`wine/README.md` covers running a program on it.

### Build a program with gexcc

```console
$ make gexcc
...
out/gexcc/bin/gexcc
$ out/gexcc/bin/gexcc -O2 -o hello hello.c && target/x86/release/gexc run hello
```

### What the build needs

- the Rust toolchain from `rust-toolchain.toml` (it installs the
  `wasm32-unknown-unknown` target);
- clang and `wasm-ld` (LLVM 22), and Node.js 22;
- for the page: Emscripten (`emsdk`) and `wasm-tools`
  (`cargo install wasm-tools`);
- for Wine: the host's MinGW-w64 GCC 12 (`x86_64-w64-mingw32-gcc`, and
  `i686-w64-mingw32-gcc` for WoW64); it is a prerequisite, not a pin;
- for AArch64 tests: `qemu-aarch64` as the reference. The musl cross
  toolchains are fetched by `make toolchains` at the versions in
  `toolchain/pins`.

Fetched sources (musl, LLVM, Wine) are kept in bare mirrors under
`~/.cache/gex` (`CACHE=`). A rerun fetches nothing it already has. The
Makefile's header lists the settings.

## Tests

`make test` runs the runtime's tests (`node test/run.mjs`; `test/README.md`
gives its options, passed as `ARGS="..."`). `make check` runs each crate's
own tests (`cargo test`). Every other test lives in the gexdev repository.

## Licence

gex is free software under the GNU Affero General Public License, version 3
only (`LICENSE`). Copyright (C) 2026 Puter Technologies Inc. Third-party
code keeps its own licence; `NOTICE` lists it.
