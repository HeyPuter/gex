#!/bin/sh
# libc++, libc++abi and libunwind for a gexcc install (C++ with wasm
# exception handling), from an LLVM source tree of clang's version.
#
#   tools/gexcc/build-cxx.sh PREFIX LLVM_SRC [BUILD_DIR]
#
# LLVM_SRC needs runtimes/, cmake/, llvm/cmake, llvm/utils, libcxx/,
# libcxxabi/, libunwind/, libc/ (libc++ uses libc's shared headers) and
# third-party/. Installs into PREFIX/sysroot: include/c++/v1, lib/libc++.a,
# libc++abi.a, libunwind.a (gexc++ links them).
set -eu
PREFIX=$(realpath "$1")
SRC=$(realpath "$2")
B=$(realpath -m "${3:-$PREFIX/build/cxx}")
SR=$PREFIX/sysroot
JOBS=${JOBS:-6}
export PATH=$PREFIX/bin:$PATH
rm -rf "$B"
# Our changes to the LLVM runtimes (patches/libcxx-*.patch, each saying why),
# applied to LLVM_SRC once
for p in "$(dirname "$0")"/patches/libcxx-*.patch; do
  [ -e "$p" ] || continue
  if ! patch -d "$SRC" -p1 -R -s -f --dry-run < "$p" > /dev/null 2>&1; then
    patch -d "$SRC" -p1 -s -N < "$p" || { echo "build-cxx.sh: $p does not apply" >&2; exit 1; }
  fi
done
# wasm is not ELF, so libunwind would mark its exports __declspec(dllexport)
# unless told the symbols are hidden (one static module: they need not be
# exported at all).
# (and LLVM_SRC's path as /llvm: libc++abi's assert()s name their files)
F="-fwasm-exceptions -D_LIBUNWIND_HIDE_SYMBOLS -ffile-prefix-map=$(realpath "$SRC")=/llvm"
cmake -G Ninja -S "$SRC/runtimes" -B "$B" \
  -DCMAKE_TOOLCHAIN_FILE="$PREFIX/share/gexcc/toolchain.cmake" \
  -DCMAKE_STAGING_PREFIX="$SR" -DCMAKE_INSTALL_PREFIX="$SR" -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_COMPILER_WORKS=ON -DCMAKE_CXX_COMPILER_WORKS=ON \
  -DCMAKE_C_FLAGS="$F" -DCMAKE_CXX_FLAGS="$F" \
  -DLLVM_ENABLE_RUNTIMES="libcxx;libcxxabi;libunwind" \
  -DLIBCXX_ENABLE_SHARED=OFF -DLIBCXXABI_ENABLE_SHARED=OFF -DLIBUNWIND_ENABLE_SHARED=OFF \
  -DLIBCXX_HAS_MUSL_LIBC=ON -DLIBCXX_CXX_ABI=libcxxabi -DLIBCXXABI_USE_LLVM_UNWINDER=ON \
  -DLIBCXXABI_ENABLE_STATIC_UNWINDER=ON -DLIBCXX_ENABLE_STATIC_ABI_LIBRARY=ON \
  -DLIBCXX_ENABLE_EXCEPTIONS=ON -DLIBCXXABI_ENABLE_EXCEPTIONS=ON \
  -DLIBCXX_USE_COMPILER_RT=ON -DLIBCXXABI_USE_COMPILER_RT=ON -DLIBUNWIND_USE_COMPILER_RT=ON \
  -DLIBCXX_INCLUDE_BENCHMARKS=OFF -DLIBCXX_INCLUDE_TESTS=OFF -DLIBCXXABI_INCLUDE_TESTS=OFF \
  -DLIBUNWIND_INCLUDE_TESTS=OFF > "$B.cmake.log" 2>&1 || { tail -20 "$B.cmake.log"; exit 1; }
ninja -C "$B" -j"$JOBS" install > "$B.build.log" 2>&1 || { grep -E 'error' "$B.build.log" | head -20; exit 1; }
# (the C++ exception tag is libgexcc's, rt/cpp_exception.s: in libc.so, one
# for every module of a process)
echo "libc++: $SR/lib/libc++.a"
