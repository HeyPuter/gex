# gexcc

A C and C++ compiler for gex's wasm32 Linux target, used like `gcc`. It
compiles with clang (`--target=wasm32-unknown-linux-musl`) against its own
sysroot: musl with gex's wasm32 port, `libgex` and compiler-rt. A linked
program is a position-independent wasm module in the ELF container that
`gexc` runs. On an x86 Linux build machine the container also runs itself
under `gexc`, so configure checks and test suites can run it like a native
binary.

```sh
tools/gexcc/install.sh PREFIX        # PREFIX/bin/gexcc, gexc++, the sysroot
PREFIX/bin/gexcc -O2 -o hello hello.c
gexc run hello
```

Libraries are static by default. `-shared` writes an archive, so a build that
insists on `libfoo.so` still links. With `GEXCC_SHARED=1`, `-shared` writes a
real wasm side module instead, which gex's loader binds at run time. The debdemo repository builds Debian
packages with gexcc and assembles a Debian root filesystem around them. The
`gexcc` script's header lists its options and environment.

C++ needs libc++ built from `llvm.pin`'s commit of our LLVM tree (clang's
version with our runtime changes): `LLVM_SRC=CHECKOUT tools/gexcc/install.sh
PREFIX`, or `tools/gexcc/build-cxx.sh PREFIX CHECKOUT`.

## Windows programs

`windows/` is gexcc's Windows target: MinGW-named tools
(`x86_64-w64-mingw32-gcc`, `-g++`, `-windres`, ...) that build Windows x64
programs and DLLs as native wasm modules against 64-bit Wine's native PE
side. A link writes a PE skeleton (`X.exe`) and its module (`X.exe.wasm`)
beside it; Wine's loader runs the pair with no x86 code.

```sh
GEX_WASM64_SYSROOT=SYSROOT tools/gexcc/windows/install.sh PREFIX WINE_BUILD WINE_SRC
tools/gexcc/build-cxx-win.sh PREFIX LLVM_CHECKOUT    # C++
PREFIX/bin/x86_64-w64-mingw32-gcc -O2 -mwindows -o app.exe app.c res.o -lcomctl32
```

`-s` links a release module (the names gex does not read stripped). A dist's
cores export every DLL export by default (`WINE64_EXPORT_ALL=1`), so such
programs' calls into any of its DLLs bind; a site that ships known programs
can build with `WINE64_EXPORT_ALL=0 WINE64_EXPORTS=FILE`, FILE listing their
imports (`DLL!name`, one a line). `windows/gexcc-win`'s header lists the rest.
