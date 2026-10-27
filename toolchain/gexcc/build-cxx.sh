#!/bin/sh
# libc++, libc++abi and libunwind for a gexcc install (C++ with wasm
# exception handling), from an LLVM source tree of clang's version.
#
#   toolchain/gexcc/build-cxx.sh PREFIX LLVM_SRC [BUILD_DIR]
#
# LLVM_SRC needs runtimes/, cmake/, llvm/cmake, llvm/utils, libcxx/,
# libcxxabi/, libunwind/, libc/ (libc++ uses libc's shared headers) and
# third-party/: a checkout of toolchain/pins' LLVM_COMMIT. Installs into PREFIX/sysroot: include/c++/v1, lib/libc++.a,
# libc++abi.a, libunwind.a (gexc++ links them).
set -eu
PREFIX=$(realpath "$1")
SRC=$(realpath "$2")
B=$(realpath -m "${3:-$PREFIX/build/cxx}")
SR=$PREFIX/sysroot
# CMake's feature checks link test programs: without the sysroot's builtins
# every one fails, and the build then dies far off (no -nostdinc++, so
# libc++'s headers include an older copy of themselves: errno undeclared)
for f in "$SR/lib/libc.a" "$SR/lib/libclang_rt.builtins-wasm32.a"; do
  [ -f "$f" ] || { echo "build-cxx.sh: no $f (toolchain/gexcc/install.sh PREFIX first)" >&2; exit 1; }
done
JOBS=${JOBS:-6}
export PATH=$PREFIX/bin:$PATH
rm -rf "$B"
# LLVM_SRC at toolchain/pins' LLVM_COMMIT (our changes to the runtimes are commits
# there): any other tree builds a libc++ the gate never tested
. "$(dirname "$0")/llvm-pin.sh"
llvm_pin_check "$SRC"
# wasm is not ELF, so libunwind would mark its exports __declspec(dllexport)
# unless told the symbols are hidden (one static module: they need not be
# exported at all).
# (and LLVM_SRC's path as /llvm: libc++abi's assert()s name their files)
F="-fwasm-exceptions -D_LIBUNWIND_HIDE_SYMBOLS -ffile-prefix-map=$(realpath "$SRC")=/llvm"
# A deque of a type not yet complete where it is declared (a member queue
# of a class declared later, as libstdc++ allows): libc++'s ABI flag for it
cmake -G Ninja -S "$SRC/runtimes" -B "$B" \
  -DCMAKE_TOOLCHAIN_FILE="$PREFIX/share/gexcc/toolchain.cmake" \
  -DCMAKE_STAGING_PREFIX="$SR" -DCMAKE_INSTALL_PREFIX="$SR" -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_COMPILER_WORKS=ON -DCMAKE_CXX_COMPILER_WORKS=ON \
  -DCMAKE_C_FLAGS="$F" -DCMAKE_CXX_FLAGS="$F" \
  -DLLVM_ENABLE_RUNTIMES="libcxx;libcxxabi;libunwind" \
  -DLIBCXX_ENABLE_SHARED=OFF -DLIBCXXABI_ENABLE_SHARED=OFF -DLIBUNWIND_ENABLE_SHARED=OFF \
  -DLIBCXX_ABI_DEFINES=_LIBCPP_ABI_INCOMPLETE_TYPES_IN_DEQUE \
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
