#!/bin/sh
# The one-module PE side (docs/wine-native.md §3.14): abi.dll, one.dll and
# two.dll linked into one wasm module (pelink.py) behind winebuild
# skeletons (--wasm-shared), and an i386 program using all three.
#   tests/wasm32/dll/shared.sh GEXC WINE_SRC WINE_TOOLS   (GEX_WASM_SYSROOT set)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
GEXC=$1 WINE=$2 TOOLS=$3
OUT=${WORK:-$(mktemp -d)}
mkdir -p "$OUT"
WCC=$root/tools/wine-wasm/wcc
WB="$TOOLS/tools/winebuild/winebuild -w -b wasm32-unknown-linux-musl --cc-cmd=$WCC"
for d in abi one two; do
    "$WCC" -O2 -D_WIN32 -fno-stack-protector -c "$here/$d.c" -o "$OUT/$d.o"
    $WB --dll -E "$here/$d.spec" -F $d.dll -o "$OUT/$d.spec.o" "$OUT/$d.o"
    "$WCC" -r -o "$OUT/$d.r.o" "$OUT/$d.spec.o" "$OUT/$d.o"
done
"$WCC" -O2 -fno-stack-protector -I"$WINE/include" -c "$WINE/libs/gexrt/wasm32.c" -o "$OUT/rt.o"
llvm-ar crs "$OUT/libgexrt.a" "$OUT/rt.o"
python3 "$root/tools/wine-wasm/pelink.py" --gexrt "$OUT/libgexrt.a" -o "$OUT/winepe.wasm" \
    --dll abi.dll "$OUT/abi.r.o" "$OUT/abi.spec.o.exports" \
    --dll one.dll "$OUT/one.r.o" "$OUT/one.spec.o.exports" \
    --dll two.dll "$OUT/two.r.o" "$OUT/two.spec.o.exports" one.dll >/dev/null
for d in abi one two; do
    $WB --dll --fake-module --wasm-shared winepe.wasm -E "$here/$d.spec" -F $d.dll -o "$OUT/$d.dll"
done
i686-w64-mingw32-gcc -O1 -o "$OUT/shared_test.exe" "$here/shared_test.c"
out=$(cd "$OUT" && "$GEXC" run --no-cgen shared_test.exe 2>/dev/null)
printf '%s\n' "$out" | grep -v '^ok'
printf '%s\n' "$out" | tail -1 | tr -d '\r' | grep -qx PASS && echo "ok   shared_test"
