#!/bin/bash
# gexcc's Windows target: MinGW-named tools building Windows x64 programs and
# DLLs as native wasm modules against 64-bit Wine's native PE side.
#
#   toolchain/gexcc/windows/install.sh PREFIX WINE_BUILD WINE_SRC
#
# WINE_BUILD  a 64-bit Wine build (wasm64, its tools beside it) whose
#             import libraries, headers, libgexrt and winecrt0 programs link
# WINE_SRC    its source tree (headers, nls)
# GEX_WASM64_SYSROOT  the wasm64 sysroot wcc64 compiles with
#                     (toolchain/sysroot/build64.sh)
# Then C++: toolchain/gexcc/windows/build-cxx.sh PREFIX LLVM_SRC.
# Out: PREFIX/bin/x86_64-w64-mingw32-{gcc,g++,cc,c++,windres,ar,ranlib,nm,
# strip,objcopy,objdump}, PREFIX/lib/gexcc-win (the runtime every program
# links, the install's config), PREFIX/share/gexcc-win/toolchain.cmake.
set -eu
PREFIX=$(realpath -m "$1")
WB=$(realpath "$2")
WS=$(realpath "$3")
: "${GEX_WASM64_SYSROOT:?set GEX_WASM64_SYSROOT}"
HERE=$(dirname "$(realpath "$0")")
for f in "$WB/Makefile" "$WB/libs/gexrt/libgexrt.a" "$WB/libs/winecrt0/libwinecrt0.a" "$WS/include/windows.h"; do
  [ -f "$f" ] || { echo "install.sh: no $f" >&2; exit 1; }
done
L=$PREFIX/lib/gexcc-win
mkdir -p "$PREFIX/bin" "$L" "$PREFIX/share/gexcc-win"
printf 'WINE_BUILD=%s\nWINE_SRC=%s\nSYSROOT64=%s\nCXX=%s\n' "$WB" "$WS" "$(realpath "$GEX_WASM64_SYSROOT")" "$PREFIX/cxx" > "$L/config"
for t in gcc g++ cc c++ windres ar ranlib nm strip objcopy objdump; do
  ln -sfn "$HERE/gexcc-win" "$PREFIX/bin/x86_64-w64-mingw32-$t"
done
sed -e "s#@BIN@#$PREFIX/bin#" -e "s#@AR@#$(command -v llvm-ar)#" -e "s#@RANLIB@#$(command -v llvm-ranlib)#" \
  "$HERE/win64.cmake" > "$PREFIX/share/gexcc-win/toolchain.cmake"
# the runtime: the C++ exception tag, the wmem* functions, dirent, the entry
# wrappers that run the constructors first
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
CC=$PREFIX/bin/x86_64-w64-mingw32-gcc
clang --target=wasm64-unknown-unknown -mexception-handling -c "$HERE/rt/cpp_exception.s" -o "$T/cpp_exception.o"
"$CC" -O2 -c "$HERE/rt/wchar.c" -o "$T/wchar.o"
"$CC" -O2 -c "$HERE/rt/dirent.c" -o "$T/dirent.o"
for k in main wmain dll; do GEX_WASM_NO_ICALL=1 "$CC" -O2 -DONLY_$k -c "$HERE/rt/start.c" -o "$L/start_$k.o"; done
for k in 0 3; do "$CC" -O2 -DONLY_$k -c "$HERE/rt/wmain.c" -o "$L/wmain$k.o"; done
rm -f "$L/libgexcc-win.a"
llvm-ar rcs "$L/libgexcc-win.a" "$T/cpp_exception.o" "$T/wchar.o" "$T/dirent.o"
# setjmp/longjmp, a copy of the program's own (a program has no core beside
# it to share the tag with): libgexrt's
(cd "$T" && llvm-ar x "$WB/libs/gexrt/libgexrt.a" wasm32_sjlj.o) && mv "$T/wasm32_sjlj.o" "$L/"
echo "gexcc-win: $PREFIX/bin/x86_64-w64-mingw32-gcc (Wine $WB)"
