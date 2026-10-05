#!/bin/sh
# Build abi.dll (abi.c as a wasm32 module behind a winebuild skeleton) and
# abi_test.exe (MinGW i386).
#   tests/wasm32/dll/build.sh OUT WINE_SRC WINE_TOOLS
# WINE_TOOLS is a native build of Wine's tools from the wasm32 fork
# (winebuild, winegcc); GEX_WASM_SYSROOT names the sysroot.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
OUT=$1 WINE=$2 TOOLS=$3
WCC=$root/tools/wine-wasm/wcc
mkdir -p "$OUT"
"$WCC" -O2 -D_WIN32 -fno-stack-protector -c "$here/abi.c" -o "$OUT/abi.o"
"$WCC" -O2 -fno-stack-protector -I"$WINE/include" -c "$WINE/libs/winecrt0/wasm32.c" -o "$OUT/winecrt0_wasm32.o"
"$TOOLS/tools/winebuild/winebuild" -w -b wasm32-unknown-linux-musl --cc-cmd="$WCC" \
    --dll -E "$here/abi.spec" -F abi.dll -o "$OUT/abi.spec.o" "$OUT/abi.o"
"$WCC" -shared -o "$OUT/abi.dll.so" "$OUT/abi.spec.o" "$OUT/abi.o" "$OUT/winecrt0_wasm32.o" \
    -Wl,@"$OUT/abi.spec.o.rsp"
"$TOOLS/tools/winebuild/winebuild" -w -b wasm32-unknown-linux-musl --cc-cmd="$WCC" \
    --dll --fake-module --wasm-module "$OUT/abi.dll.so" -E "$here/abi.spec" -F abi.dll -o "$OUT/abi.dll"
i686-w64-mingw32-gcc -O1 -o "$OUT/abi_test.exe" "$here/abi_test.c"
