#!/usr/bin/env bash
# 64-bit Wine's native PE DLLs: the named DLLs of a
# wasm64 Wine build linked as one module (pelink.py, lowered to wasm32),
# with a PE32+ skeleton per DLL that names it (winebuild --wasm-shared).
#
#   tools/wine-wasm/stage-native64.sh WINE_BUILD WINE_SRC OUTDIR DLL...
#
# WINE_BUILD is a wasm64 build tree of the Wine fork (wine64-dist.sh's b64:
# winegcc there leaves each DLL's relocatable object and export list),
# WINE_SRC its source. OUTDIR gets winepe.wasm and the skeletons, to go in
# place of a 64-bit Wine install's lib/wine/x86_64-windows DLLs; every DLL not named stays x86-64 code.
# GEX_WASM64_SYSROOT must be set (build-sysroot64.sh).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
[ $# -ge 4 ] || { sed -n 2,13p "$0" >&2; exit 2; }
build=$(cd "$1" && pwd) src=$(cd "$2" && pwd) out=$3
shift 3
: "${GEX_WASM64_SYSROOT:?set GEX_WASM64_SYSROOT}"
grep -q '^HOST_ARCH = wasm64' "$build/Makefile" || { echo "$build is not a wasm64 build tree" >&2; exit 2; }
winebuild=${WINEBUILD:-$(grep -m1 -o '[^ ]*/tools/winebuild/winebuild' "$build/Makefile")}
[ -x "$winebuild" ] || { echo "no winebuild (WINEBUILD=...)" >&2; exit 2; }

rm -rf "$out"
mkdir -p "$out"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/stage-native64.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

PATH=$GEX_WASM64_SYSROOT/binaryen/bin:$PATH TMPDIR=$tmp python3 "$here/pelink.py" --wasm64 "$build" "$src" "$tmp/winepe.wasm" "$@"
cp "$tmp/winepe.wasm" "$out/winepe.wasm"
for dll in "$@"; do
    name=${dll%.*}
    dir=$build/dlls/$name
    [ -d "$dir" ] || dir=$build/dlls/$dll
    [ -d "$dir" ] || dir=$build/programs/$name
    spec=$src/${dir#$build/}/$name.spec
    if [ ! -f "$spec" ] && [ "${dir#$build/dlls/}" != "$dir" ]; then spec=$tmp/$name.spec; : > "$spec"; fi
    if [ -f "$spec" ]; then kind=(--dll -E "$spec"); else kind=(--exe); fi
    # a GUI program's skeleton says so (stage-native.sh)
    if [ ! -f "$spec" ] && grep -q -- '-mwindows' "$src/${dir#$build/}/Makefile.in" 2>/dev/null; then kind+=(--subsystem windows); fi
    # shellcheck disable=SC2046
    "$winebuild" -w -b wasm64-unknown-linux-musl --cc-cmd="$here/wcc64 -D_WIN32" "${kind[@]}" --fake-module \
        --wasm-shared winepe.wasm -F "$dll" -o "$out/$dll" $(ls "$dir"/*.res 2>/dev/null)
done
echo "$out: ${*} native ($(du -h "$out/winepe.wasm" | cut -f1) module)"
