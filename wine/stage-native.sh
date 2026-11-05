#!/usr/bin/env bash
# 64-bit Wine's native PE DLLs: the named DLLs of a
# wasm64 Wine build linked as one module (pelink.py, lowered to wasm32),
# with a PE32+ skeleton per DLL that names it (winebuild --wasm-shared).
#
#   wine/stage-native.sh [--i386] WINE_BUILD WINE_SRC OUTDIR DLL...
#
# WINE_BUILD is a wasm64 build tree of the Wine fork (wine64-dist.sh's b64:
# winegcc there leaves each DLL's relocatable object and export list),
# WINE_SRC its source. OUTDIR gets winepe.wasm and the skeletons, to go in
# place of a 64-bit Wine install's lib/wine/x86_64-windows DLLs; every DLL not named stays x86-64 code.
# GEX_WASM64_SYSROOT must be set (toolchain/sysroot/build64.sh).
# --i386: WoW64's 32-bit side instead: WINE_BUILD a wasm32 build tree of the
# same fork (wine32/wine-build.sh wasm), PE32 skeletons over wasm32 modules,
# for lib/wine/i386-windows; GEX_WASM_SYSROOT must be set (toolchain/sysroot/build32.sh).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
i386=0; [ "${1:-}" != --i386 ] || { i386=1; shift; }
[ $# -ge 4 ] || { sed -n 2,16p "$0" >&2; exit 2; }
build=$(cd "$1" && pwd) src=$(cd "$2" && pwd) out=$3
shift 3
if [ $i386 = 1 ]; then
    : "${GEX_WASM_SYSROOT:?set GEX_WASM_SYSROOT}"
    host=wasm32; target=wasm32-unknown-linux-musl; cc="$here/wcc"; link=(); bin=
else
    : "${GEX_WASM64_SYSROOT:?set GEX_WASM64_SYSROOT}"
    host=wasm64; target=wasm64-unknown-linux-musl; cc="$here/wcc64 -D_WIN32"; link=(--wasm64); bin=$GEX_WASM64_SYSROOT/binaryen/bin:
fi
grep -q "^HOST_ARCH = $host" "$build/Makefile" || { echo "$build is not a $host build tree" >&2; exit 2; }
winebuild=${WINEBUILD:-$(grep -m1 -o '[^ ]*/tools/winebuild/winebuild' "$build/Makefile")}
[ -x "$winebuild" ] || { echo "no winebuild (WINEBUILD=...)" >&2; exit 2; }

rm -rf "$out"
mkdir -p "$out"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/stage-native64.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

# WINE64_EXTRA_DLLS=DIR: DLLs built outside the tree linked in too (dirk's
# d3d11.dll and dxgi.dll,
# third_party/mesa/subprojects/dxvk/tests/dirk/w64/build.sh): for each,
# NAME.so.r.o, NAME.so.exports, NAME.spec and NAME.imports (a comma list) in
# DIR. Their skeletons go in OUTDIR/extra, not beside the others: a prefix
# loads them only when told to (a DLL override), its own DLLs of those names
# staying.
extra=() extras=()
if [ -n "${WINE64_EXTRA_DLLS:-}" ]; then
    for ro in "$WINE64_EXTRA_DLLS"/*.so.r.o; do
        [ -e "$ro" ] || continue
        n=$(basename "$ro" .so.r.o)
        extra+=(--extra "$n" "$ro" "${ro%.r.o}.exports" "$(cat "$WINE64_EXTRA_DLLS/${n%.*}.imports")")
        extras+=("$n")
    done
fi
# WINE64_SPLIT_DLLS="DLL ...": those DLLs each linked as a module of their
# own, DLL.wasm beside winepe.wasm (pelink.py --split), loaded when their
# skeleton is; their imports of the core's DLLs bind to its exports.
split=() splits=()
for dll in ${WINE64_SPLIT_DLLS:-}; do split+=(--split "$dll.wasm" "$dll"); splits+=("$dll"); done
# WINE64_EXPORTS_OF=CORE.wasm: winepe.wasm exports what that core of an
# earlier staging exports, so that the split modules staged with it still
# load beside this one (dirk's core in place of a dist's, its split modules
# kept); name the same DLLs as that staging did.
keep=(); [ -z "${WINE64_EXPORTS_OF:-}" ] || keep=(--exports-of "$WINE64_EXPORTS_OF")
PATH=$bin$PATH TMPDIR=$tmp python3 "$here/pelink.py" "${link[@]}" "${split[@]}" "${keep[@]}" "${extra[@]}" "$build" "$src" "$tmp/winepe.wasm" "$@"
cp "$tmp/winepe.wasm" "$out/winepe.wasm"
for dll in "${splits[@]}"; do cp "$tmp/$dll.wasm" "$out/$dll.wasm"; done
if [ ${#extras[@]} -gt 0 ]; then
    mkdir -p "$out/extra"
    for dll in "${extras[@]}"; do
        "$winebuild" -w -b "$target" --cc-cmd="$cc" --dll -E "$WINE64_EXTRA_DLLS/${dll%.*}.spec" \
            --fake-module --wasm-shared winepe.wasm -F "$dll" -o "$out/extra/$dll"
    done
fi
for dll in "$@" "${splits[@]}"; do
    name=${dll%.*}
    module=winepe.wasm
    for x in "${splits[@]}"; do [ "$x" != "$dll" ] || module=$dll.wasm; done
    # a module named with its extension (msacm32.drv) has its own directory
    dir=$build/dlls/$dll
    [ -d "$dir" ] || dir=$build/dlls/$name
    [ -d "$dir" ] || dir=$build/programs/$dll
    [ -d "$dir" ] || dir=$build/programs/$name
    # (a module named with its extension has its spec named so too: wshom.ocx.spec)
    spec=$src/${dir#$build/}/$dll.spec
    [ -f "$spec" ] || spec=$src/${dir#$build/}/$name.spec
    if [ ! -f "$spec" ] && [ "${dir#$build/dlls/}" != "$dir" ]; then spec=$tmp/$name.spec; : > "$spec"; fi
    if [ -f "$spec" ]; then kind=(--dll -E "$spec"); else kind=(--exe); fi
    # a GUI program's skeleton says so (stage-native.sh)
    if [ ! -f "$spec" ] && grep -q -- '-mwindows' "$src/${dir#$build/}/Makefile.in" 2>/dev/null; then kind+=(--subsystem windows); fi
    # a driver's skeleton is a native image, as its build makes it (-Wl,--subsystem,native):
    # Wine's loader then calls no DllMain at its entry, which is DriverEntry
    if grep -q -- '--subsystem,native' "$src/${dir#$build/}/Makefile.in" 2>/dev/null; then kind+=(--subsystem native); fi
    # shellcheck disable=SC2046
    "$winebuild" -w -b "$target" --cc-cmd="$cc" "${kind[@]}" --fake-module \
        --wasm-shared "$module" -F "$dll" -o "$out/$dll" $(ls "$dir"/*.res 2>/dev/null)
    # (its source directory's specs, whichever the lookup above took)
    [ ! -f "$spec" ] || echo "$out/$dll $src/${dir#$build/}" >> "$tmp/skeletons"
done
# A skeleton without an export table whose spec has exports is a build error,
# not a GetProcAddress that finds nothing at run time (skel-exports.py)
[ ! -f "$tmp/skeletons" ] || python3 "$here/skel-exports.py" "$tmp/skeletons" || exit 1
echo "$out: ${*} native ($(du -h "$out/winepe.wasm" | cut -f1) module)${splits:+, ${splits[*]} as modules of their own}"
