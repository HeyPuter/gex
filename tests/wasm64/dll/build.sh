#!/bin/sh
# Build abi.dll (abi.c as a lowered wasm64 module behind a winebuild PE32+
# skeleton) and abi_test.exe (MinGW x64).
#   tests/wasm64/dll/build.sh OUT WINE_SRC WINE_TOOLS
# WINE_TOOLS is a native build of Wine's tools from the W64 fork (winebuild,
# whose CPU_WASM64 writes the adapters); GEX_WASM64_SYSROOT names the
# sysroot (build-sysroot64.sh: the LLP64 and ICALL plugins are in it).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
OUT=$1 WINE=$2 TOOLS=$3
WCC=$root/tools/wine-wasm/wcc64
mkdir -p "$OUT"
"$WCC" -O2 -D_WIN32 -fno-stack-protector -c "$here/abi.c" -o "$OUT/abi.o"
"$WCC" -O2 -D_WIN32 -fno-stack-protector -I"$WINE/include" -I"$WINE/include/msvcrt" -c "$WINE/libs/gexrt/wasm64.c" -o "$OUT/gexrt64.o"
# the runtime of clang's wasm SjLj lowering (gexrt's longjmp), as libgexrt.a has it
"$WCC" -O2 -D_WIN32 -fno-stack-protector -I"$WINE/include" -I"$WINE/include/msvcrt" -c "$WINE/libs/gexrt/wasm32_sjlj.c" -o "$OUT/gexrt64_sjlj.o"
"$TOOLS/tools/winebuild/winebuild" -w -b wasm64-unknown-linux-musl --cc-cmd="$WCC -D_WIN32" \
    --dll -E "$here/abi.spec" -F abi.dll -o "$OUT/abi.spec.o" "$OUT/abi.o"
"$WCC" -shared -o "$OUT/abi.dll.so" "$OUT/abi.spec.o" "$OUT/abi.o" "$OUT/gexrt64.o" "$OUT/gexrt64_sjlj.o" \
    -Wl,@"$OUT/abi.spec.o.rsp"
"$TOOLS/tools/winebuild/winebuild" -w -b wasm64-unknown-linux-musl --cc-cmd="$WCC -D_WIN32" \
    --dll --fake-module --wasm-module "$OUT/abi.dll.so" -E "$here/abi.spec" -F abi.dll -o "$OUT/abi.dll"
x86_64-w64-mingw32-gcc -O1 -o "$OUT/abi_test.exe" "$here/abi_test.c"
