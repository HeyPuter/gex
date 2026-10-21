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

Libraries are static. `-shared` writes an archive, so a build that insists
on `libfoo.so` still links. `debian/` builds Debian packages with gexcc and
assembles a Debian root filesystem around them. The `gexcc` script's header
lists its options and environment.
