#!/bin/bash
# The DLLs a W64 prefix pins load at their pins in every process
# (dllband.c): the prefix's dll-bases from tools/wine-wasm/w64/dll-bases.py,
# which must pass its own check, and four of the dist's DLLs loaded at the
# addresses it gives them, in [0x11000000, 0x20000000), in the program and in
# a second process it starts, natively and in the page (run-node.mjs).
# Prints "ok   dllband64 <leg>" per leg.
#
#   tests/wine64/dllband.sh DIST PREFIX [gexc]
#
# DIST is a W64 install whose server reads dll-bases (Wine branch
# g3/wine64-dll-bases: server/mapping.c, pinned_map_address), PREFIX a win64
# prefix made for it (copied, never written). Needs x86_64-w64-mingw32-gcc
# and an x86 gexc. The page leg needs web/public/gex's x86 shared-memory
# shim and seed (PUB=DIR: another directory holding them).
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
tool=$root/tools/wine-wasm/w64/dll-bases.py
w=${WORK:-$root/target/w64-dllband}
rm -rf "$w"; mkdir -p "$w"
x86_64-w64-mingw32-gcc -O1 -Wall -o "$w/dllband.exe" "$here/dllband.c" || exit 1
dlls="version winmm dbghelp opengl32"
fail=0
verdict() { # LEG LOG
	grep -aE '^(ok  |FAIL)' "$2" | tr -d '\r' | sed "s/^/  $1: /"
	if grep -aq '^dllband ok' "$2"; then echo "ok   dllband64 $1"; else
		echo "FAIL dllband64 $1"; grep -av '^\(ok  \|FAIL\)' "$2" | tail -8 | sed 's/^/  /'; fail=$((fail + 1)); fi
}
# assign PFX PREFIX_OF_PATHS [--map ...]: the prefix's dll-bases, checked
assign() {
	local p=$1 at=$2; shift 2
	for d in $dlls; do echo "$at/lib/wine/x86_64-windows/$d.dll"; done |
		python3 "$tool" assign "$p/dll-bases" "$@" >> "$w/assign.log" 2>&1 &&
		python3 "$tool" check "$p/dll-bases" ||
		{ echo "FAIL dllband64: dll-bases.py"; sed 's/^/  /' "$w/assign.log"; exit 1; }
}
# Natively the server names an image by the dist's own path.
if [ -x "$gexc" ]; then
	rm -rf "$w/pfx"; cp -a "$prefix" "$w/pfx"; cp "$w/dllband.exe" "$w/pfx/drive_c/"
	assign "$w/pfx" "$dist"
	(cd "$w/pfx/drive_c" && timeout -k 10 "${TMO:-300}" "$gexc" run --no-cgen \
		--env WINEPREFIX="$w/pfx" --env WINEDEBUG=err+all,fixme-all --env WINEGEX_DOOR=0 \
		"$dist/lib/wine/x86_64-unix/wine" 'C:\dllband.exe' 'Z:'"$w/pfx/dll-bases") > "$w/native.log" 2>&1
	verdict native "$w/native.log"
else
	echo "skip native: no $gexc"
fi
# The page: the server names images by their page paths.
pub=$(realpath -m "${PUB:-$root/web/public/gex}")
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$pub/$f" ] || { echo "skip page: no $pub/$f"; exit $((fail > 0)); }
done
rm -rf "$w/pfx-page"; cp -a "$prefix" "$w/pfx-page"; cp "$w/dllband.exe" "$w/pfx-page/drive_c/"
assign "$w/pfx-page" /w64 --map /w64="$dist"
(cd "$root" && GEX_WEB_DIR="$pub" GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 "${TMO:-300}" \
	node --max-old-space-size=8192 shims/browser/js/run-node.mjs --dir /w64="$dist" --dir /pfx="$w/pfx-page" \
	--env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all --env WINEGEX_DOOR=0 \
	--cwd /pfx/drive_c --argv0 /w64/lib/wine/x86_64-unix/wine \
	"$pub/seed.x86.shared.wasm" "$dist/lib/wine/x86_64-unix/wine" 'C:\dllband.exe' Z:/pfx/dll-bases) > "$w/page.log" 2>&1
verdict page "$w/page.log"
[ "$fail" = 0 ]
