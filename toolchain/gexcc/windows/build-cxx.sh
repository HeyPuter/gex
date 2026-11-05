#!/bin/bash
# libc++, libc++abi and libunwind for gexcc's Windows target (Windows x64
# programs as native wasm): Wine's C runtime headers, the win32 thread API,
# wasm exception handling, from a checkout of toolchain/pins' LLVM_COMMIT.
#
#   toolchain/gexcc/windows/build-cxx.sh PREFIX LLVM_SRC [BUILD_DIR]
#
# PREFIX is a windows/install.sh install; the libraries go to PREFIX/cxx
# (include/c++/v1, lib/libc++.a, libc++abi.a, libunwind.a), which its
# x86_64-w64-mingw32-g++ links.
set -eu
PREFIX=$(realpath "$1")
SRC=$(realpath "$2")
B=$(realpath -m "${3:-$PREFIX/build/cxx-win}")
[ -x "$PREFIX/bin/x86_64-w64-mingw32-g++" ] || { echo "build-cxx-win.sh: no $PREFIX/bin/x86_64-w64-mingw32-g++ (toolchain/gexcc/windows/install.sh PREFIX first)" >&2; exit 1; }
# (../llvm-pin.sh's check, run as from toolchain/gexcc, whose ../pins it reads)
sh -c '. "$(dirname "$0")/llvm-pin.sh"; llvm_pin_check "$1"' "$(dirname "$0")/../build-cxx.sh" "$SRC"
# (the build's own headers, not an installed copy)
export GEXCC_WIN_CXX=/nonexistent
rm -rf "$B"; mkdir -p "$B"
F="-fwasm-exceptions -D_LIBUNWIND_HIDE_SYMBOLS -ffile-prefix-map=$SRC=/llvm"
cmake -G Ninja -S "$SRC/runtimes" -B "$B" -DCMAKE_TOOLCHAIN_FILE="$PREFIX/share/gexcc-win/toolchain.cmake" \
  -DCMAKE_INSTALL_PREFIX="$PREFIX/cxx" -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_FLAGS="$F" -DCMAKE_CXX_FLAGS="$F" \
  -DLLVM_ENABLE_RUNTIMES="libcxx;libcxxabi;libunwind" \
  -DLIBCXX_ENABLE_SHARED=OFF -DLIBCXXABI_ENABLE_SHARED=OFF -DLIBUNWIND_ENABLE_SHARED=OFF \
  -DLIBCXX_CXX_ABI=libcxxabi -DLIBCXXABI_USE_LLVM_UNWINDER=ON \
  -DLIBCXXABI_ENABLE_STATIC_UNWINDER=ON -DLIBCXX_ENABLE_STATIC_ABI_LIBRARY=ON \
  -DLIBCXX_ENABLE_EXCEPTIONS=ON -DLIBCXXABI_ENABLE_EXCEPTIONS=ON \
  -DLIBCXX_HAS_WIN32_THREAD_API=ON -DLIBCXXABI_HAS_WIN32_THREAD_API=ON \
  -DLIBCXX_USE_COMPILER_RT=OFF -DLIBCXXABI_USE_COMPILER_RT=OFF -DLIBUNWIND_USE_COMPILER_RT=OFF \
  -DLIBCXX_ENABLE_ABI_LINKER_SCRIPT=OFF -DLIBCXX_ENABLE_NEW_DELETE_DEFINITIONS=ON -DLIBCXXABI_ENABLE_NEW_DELETE_DEFINITIONS=OFF \
  -DLIBCXX_INCLUDE_BENCHMARKS=OFF -DLIBCXX_INCLUDE_TESTS=OFF -DLIBCXXABI_INCLUDE_TESTS=OFF \
  -DLIBUNWIND_INCLUDE_TESTS=OFF -DLIBCXX_INCLUDE_DOCS=OFF > "$B.cmake.log" 2>&1 || { tail -30 "$B.cmake.log"; exit 1; }
ninja -C "$B" -j"${JOBS:-6}" install > "$B.build.log" 2>&1 || { grep -E 'error' "$B.build.log" | sort | uniq -c | sort -rn | head -20; exit 1; }
echo "libc++ (Windows): $PREFIX/cxx/lib/libc++.a"
