#!/bin/sh
# W64's data-model gate (docs/wine-native-x64.md §1.2, §5 W64.0): Wine's PE
# side for x64 is compiled for wasm64 with the GexLLP64 plugin; every
# structure it shares with x64 programs must have the layout x86-64 Windows
# code expects. layout.c names the Win32, NT and wineserver types; clang's
# record layouts (every nested record, sizes, alignments, field offsets) are
# dumped for wasm64+GexLLP64 and for x86_64-w64-mingw32 (Wine's own x64 PE
# target, MSVC bitfields) and must be identical.
#
#   tests/wine64/layout.sh [WINE_SRC]
#
# WINE_SRC: a Wine tree with the W64 headers (branch w64/main; default
# ~/gex-sandbox/wt/wine-w64). Needs only clang; the plugin is built into
# $LAYOUT_WORK (default a temp dir) by tools/wine-wasm/llp64/build.sh.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
wine=${1:-${WINE_SRC:-$HOME/gex-sandbox/wt/wine-w64}}
[ -f "$wine/include/winnt.h" ] || { echo "no Wine tree at $wine" >&2; exit 2; }
grep -q __GEX_LLP64__ "$wine/include/basetsd.h" || { echo "$wine has no W64 headers (branch w64/main)" >&2; exit 2; }
work=${LAYOUT_WORK:-$(mktemp -d)}
mkdir -p "$work"
"$root/tools/wine-wasm/llp64/build.sh" "$work" >/dev/null
res=$(clang -print-resource-dir)/include
common="-mms-bitfields -fshort-wchar -nostdinc -I$wine/include -I$wine/include/msvcrt -isystem $res -D_WIN32 -D__WINESRC__ -fsyntax-only -w -Xclang -fdump-record-layouts"
# shellcheck disable=SC2086
clang --target=x86_64-w64-mingw32 $common "$here/layout.c" > "$work/x86_64.txt"
# shellcheck disable=SC2086
clang --target=wasm64-unknown-unknown -fplugin="$work/GexLLP64.so" -include "$work/llp64.h" -D_WIN64 $common "$here/layout.c" > "$work/wasm64.txt"
n=$(grep -c 'Dumping AST Record Layout' "$work/x86_64.txt" || true)
if [ "$n" -lt 1000 ]; then
    echo "FAIL layout: only $n records dumped (want >= 1000)"; exit 1
fi
if ! diff -u "$work/x86_64.txt" "$work/wasm64.txt" > "$work/diff.txt"; then
    echo "FAIL layout: wasm64 (GexLLP64) differs from x86_64 ($work/diff.txt):"
    head -40 "$work/diff.txt"
    exit 1
fi
echo "ok   layout: $n records identical on wasm64 (GexLLP64) and x86_64"
[ -n "${LAYOUT_WORK:-}" ] || rm -rf "$work"
