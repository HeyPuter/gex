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
- **wasm32 Linux programs.** `tools/gexcc` is a C/C++ compiler for gex's
  wasm32 Linux target. It is used like `gcc`. The debdemo repository
  builds Debian packages with it and assembles a Debian rootfs around them.
- **32-bit Windows programs, through Wine.** `tools/wine-wasm` builds
  Wine's own Unix side as wasm32 for gex, plus a ready-to-run prefix.
  The Windows code itself is translated like any other x86 code. 64-bit
  Wine is in progress.
- **Graphics.** `gex-wayland` is a Wayland compositor, and `gex-xwm` the X
  window manager that a rootless Xwayland needs. Wayland and X11 programs
  share one desktop. Xwayland can run as native WebAssembly in the page,
  outside any guest. A guest's OpenGL calls go through gex's own libGL and
  libEGL (`guest/gl`) to Mesa, which is compiled to WebAssembly and draws
  with the browser's WebGPU (`third_party/mesa`, Gallium's `gex_wgpu`).
  Desktop GL is supported up to 4.6.

## How it works

```
 guest ELF/PE ─▶ gex-ld ─▶ gex-xlate ─▶ gex-isa(-x86) ─▶ gex-lift(-x86) ─▶ gex-emit ─▶ wasm
                  load      discover     decode            GIR               tier 1
                                              │
                                              ├─▶ gex-fjit   function JIT (GIR → wasm)
                                              └─▶ gex-cgen   function tier (GIR → C → clang → wasm)

 page: shims/browser (gex-kernel per process) + shims/kernel (shared kernel) + Mesa + Xwayland
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
  `web/eject-site.mjs` makes the web build of an
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
| `crates/gex-xlate` | code discovery and the translation driver |
| `crates/gex-ld`, `gex-dylink` | ELF and PE loading; shared libraries for wasm32 programs |
| `crates/gex-kernel` | the Linux personality: syscalls, VFS, signals, memory map, processes |
| `crates/gex-abi`, `gex-mem`, `gex-atomic`, `gex-kmem` | the layout contract and guest-memory access |
| `crates/gex-cache`, `gex-key` | the compiled-module cache and the content keys every host agrees on |
| `crates/gex-wayland`, `gex-xwm`, `gex-gpu` | the compositor, the X window manager, WebGPU compositing |
| `crates/gex-gl` | OpenGL ES passthrough for the native host |
| `crates/gex-eject` | ejected projects, built and run ahead of time |
| `shims/browser` | the browser host: the kernel compiled to wasm, beside each guest |
| `shims/kernel` | the shared kernel module: process table, filesystem, pipes, sockets |
| `shims/hostwl` | the compositor for an X server that runs in the page |
| `shims/native` | the native host, on wasmtime |
| `guest/` | libraries installed into the guest: GL and EGL, GLX setup, `libsteam_api` |
| `web/` | the page, its build, and the Mesa and Xwayland builds |
| `tools/gexcc` | the wasm32 C/C++ compiler and its sysroot |
| `tools/wine-wasm` | Wine for gex: dist, prefix, runner |
| `tools/eject` | recording a Windows program for eject |
| `third_party/` | wasmtime (with local changes), Mesa, the Khronos GL registry and CTS |

## Building

You need the Rust toolchain from `rust-toolchain.toml` (it installs the
`wasm32-unknown-unknown` target), clang and `wasm-ld`, and Node.js. Building
Mesa for the page also needs Emscripten. Running AArch64 tests needs an
`aarch64-linux-musl` cross toolchain, with `qemu-aarch64` as the reference.

```console
$ cargo build --release -p gexc                      # AArch64 guests
$ cargo build --release -p gexc --features x86 --target-dir target/x86
                                                     # x86-64 and i386 guests

$ gexc run <elf> [args...]          # translate and run
$ gexc run --wayland SOCKET <elf>   # with the compositor, at SOCKET
$ gexc build <elf> -o out.wasm      # translate only
$ gexc disasm <elf>                 # disassemble
$ gexc ir <elf> --at 0x400abc       # the GIR for one block
$ gexc run --strace <elf>           # syscall trace
```

The page:

```console
$ git submodule update --init third_party/mesa
$ web/build-all.sh                  # multi-process build, web/dist
$ web/build-all.sh --sp             # single-thread build, web/dist-sp
$ node web/serve-dist.mjs 8088 --sp-port 8089
```

The multi-process build gives each guest thread its own Worker on shared
memory. It needs SharedArrayBuffer, so the page must be served with
COOP/COEP headers. The single-thread build runs every thread as a fiber in
one Worker, and works on any static host. See `web/README.md`.

Wine needs the x86 `gexc` at `target/x86/release/gexc`, as built above
(or `GEXC=` naming another):

```console
$ make -C tools/wine-wasm           # the Wine dist and a prefix
$ tools/wine-wasm/run.sh app.exe    # run a Windows program
```

## Tests

Each crate's own tests run with `cargo test -p <crate>`. `tests/diff-fuzz`
and `tests/diff-fuzz-x86` fuzz the lifters instruction by instruction against
QEMU.

## Licence

gex is free software under the GNU Affero General Public License, version 3
only (`LICENSE`). Copyright (C) 2026 Puter Technologies Inc. Third-party
code keeps its own licence; `NOTICE` lists it.
