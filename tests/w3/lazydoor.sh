#!/bin/bash
# A slow lazy file mapped as an image (lazydoor.c): the wineserver's reads of
# it inside the door call must not hold another process's server calls for
# the fetch; prints "lazydoor ok MAP_MS SLOWEST_MS". In the page's runtime
# under Node (run-node.mjs --lazy, each fetch GEX_LAZY_DELAY_MS late): lazy
# files are the page's, so there is no native leg.
#
#   tests/w3/lazydoor.sh DIST PREFIX
#
# DIST is a W3 Wine install (lib/wine/i386-unix/wine), PREFIX a Wine prefix
# made for it (copied, never written); PAGE_PREFIX, if set, the page's (one
# whose display driver works without a compositor). Needs
# i686-w64-mingw32-gcc and the x86 shared-memory shim and seed in
# web/public/gex (GEX_ARCH=x86 shims/browser/build.sh). DELAY (ms, default
# 3000) is each fetch's delay.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
delay=${DELAY:-3000}

w=${WORK:-$root/target/w3-lazydoor}
rm -rf "$w"; mkdir -p "$w"
i686-w64-mingw32-gcc -O1 -o "$w/lazydoor.exe" "$here/lazydoor.c" || exit 1
# the image the parent maps: a PE of its own (any will do), lazy
cp "$w/lazydoor.exe" "$w/lazy.dll"
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip: no web/public/gex/$f"; exit 0; }
done
cp -a "${PAGE_PREFIX:-$prefix}" "$w/pfx-page"
cp "$w/lazydoor.exe" "$w/pfx-page/drive_c/"
(cd "$root" && GEX_ARCH=x86 GEX_SHARED=1 GEX_LAZY_DELAY_MS=$delay timeout -k 10 "${TMO:-300}" node --max-old-space-size=8192 \
	shims/browser/js/run-node.mjs --dir /w3="$dist" --dir /pfx="$w/pfx-page" \
	--lazy "/pfx/drive_c/lazy.dll=$w/lazy.dll" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
	--cwd /pfx/drive_c --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
	"$dist/lib/wine/i386-unix/wine" 'C:\lazydoor.exe' "$delay") > "$w/page.log" 2>&1
if grep -q '^lazydoor ok' "$w/page.log"; then
	echo "ok   page ($(grep -m1 '^lazydoor ok' "$w/page.log"))"
else
	echo "FAIL page"; grep -v '^\[host\]\|gex-console-error' "$w/page.log" | tail -8 | sed 's/^/  /'
	exit 1
fi
