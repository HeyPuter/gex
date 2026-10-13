#!/usr/bin/env bash
# A Wine prefix whose DLLs are native wasm32 (docs/wine-native.md §3.14,
# §3.15): the staged x86 prefix, with the named DLLs replaced by
# skeletons over one module of Wine's PE side (pelink.py), beside them in
# syswow64.
#
#   tools/wine-wasm/stage-native.sh WINE_BUILD WINE_SRC OUT DLL...
#
# WINE_BUILD is a wasm32 build tree of the Wine fork (winegcc there leaves
# each DLL's relocatable object and export list), WINE_SRC its source. The
# rest of the prefix links to the x86 one (GEX_WINE_PREFIX, default
# ~/.cache/gex/wine-11.15), so the DLLs not named stay i386 code. Its
# layout pins are dropped: they are for the x86 DLLs. GEX_WASM_SYSROOT must
# be set (tools/wine-wasm/build-sysroot.sh). Every i386 DLL in syswow64 ends
# at its fixed base from w3-bases.txt (rebase.py): a W3 dist's
# lib/wine/i386-windows takes its DLLs from here, never from the x86 prefix.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
[ $# -ge 4 ] || { sed -n 2,14p "$0" >&2; exit 2; }
build=$(cd "$1" && pwd) src=$(cd "$2" && pwd) out=$3
shift 3
x86=${GEX_WINE_PREFIX:-$HOME/.cache/gex/wine-11.15}
[ -f "$x86/.wine-version" ] || { echo "no staged Wine prefix at $x86 (tools/nt-stage-wine.sh)" >&2; exit 2; }
: "${GEX_WASM_SYSROOT:?set GEX_WASM_SYSROOT}"
winebuild=${WINEBUILD:-$(grep -m1 -o '[^ ]*/tools/winebuild/winebuild' "$build/Makefile")}
[ -x "$winebuild" ] || { echo "no winebuild (WINEBUILD=...)" >&2; exit 2; }

rm -rf "$out"
mkdir -p "$out"
# symbolic links: the prefix may be on another file system. Only for the
# large, read-only parts (the DLLs and programs, fonts, NLS tables): every
# other file is copied, so a run that writes one (the registry, win.ini,
# INetCookies/index.dat) writes this prefix's copy, never the staged
# prefix every other prefix links to.
cp -rs "$x86/." "$out/"
rm -f "$out/gex-layout"
# A staged prefix that carries an installed registry gets the W3 session
# (w3-session.reg: no services that need Unix sides W3 lacks). One with no
# registry yet gets it after wineboot's install makes one:
# `reg-apply.py PFX/system.reg w3-session.reg`.
[ ! -f "$out/system.reg" ] || python3 "$here/reg-apply.py" "$out/system.reg" "$here/w3-session.reg"
find "$out" -type l ! -iregex '.*\.\(dll\|exe\|drv\|sys\|ocx\|acm\|cpl\|ax\|tlb\|nls\|ttf\|ttc\|fon\|mui\|so\|wasm\|dll16\|exe16\|drv16\|vxd\)' -print0 |
    while IFS= read -r -d '' f; do cp --remove-destination "$(readlink -f "$f")" "$f"; done
sys=$out/drive_c/windows/syswow64
tmp=$(mktemp -d "${TMPDIR:-/tmp}/stage-native.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

TMPDIR=$tmp python3 "$here/pelink.py" "$build" "$src" "$tmp/winepe.wasm" "$@"
cp "$tmp/winepe.wasm" "$sys/winepe.wasm"
for dll in "$@"; do
    name=${dll%.*}
    dir=$build/dlls/$name
    [ -d "$dir" ] || dir=$build/dlls/$dll  # (winewayland.drv's directory keeps the extension)
    [ -d "$dir" ] || dir=$build/programs/$name
    spec=$src/${dir#$build/}/$name.spec
    rm -f "$sys/$dll"
    # a program has no .spec: an exe skeleton, its entry point the module's
    # (a DLL with no .spec, a driver like winewayland.drv, exports nothing:
    # an empty one, so its skeleton is still a DLL with DllMain as entry)
    if [ ! -f "$spec" ] && [ "${dir#$build/dlls/}" != "$dir" ]; then spec=$tmp/$name.spec; : > "$spec"; fi
    if [ -f "$spec" ]; then kind=(--dll -E "$spec"); else kind=(--exe); fi
    # shellcheck disable=SC2046
    "$winebuild" -w -b wasm32-unknown-linux-musl --cc-cmd="$here/wcc" "${kind[@]}" --fake-module \
        --wasm-shared winepe.wasm -F "$dll" -o "$sys/$dll" $(ls "$dir"/*.res 2>/dev/null)
done
# Every i386 DLL at its fixed base (rebase.py, w3-bases.txt), so a DLL's
# function-tier unit matches wherever and whenever a process loads it. A DLL
# missing from the table fails the stage: add it (rebase.py table).
python3 "$here/rebase.py" apply "$sys" "$here/w3-bases.txt" > "$tmp/rebase.log" 2>&1 ||
    { echo "stage-native: rebase.py: $(head -3 "$tmp/rebase.log" | tr '\n' ' ')" >&2; exit 1; }
echo "$out: ${*} native ($(du -h "$sys/winepe.wasm" | cut -f1) module), $(tail -1 "$tmp/rebase.log" | sed 's/^rebase.py: //')"
