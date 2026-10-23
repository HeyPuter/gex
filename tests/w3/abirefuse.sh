#!/bin/bash
# A W3 native module built with an older gex_abi.h is refused at install, in
# the page's runtime under Node (run-node.mjs): its inline probe would read
# the probe cache in another layout and trap later at a wrong table index.
# The old header had no `gex.abi` custom section, so each case takes DIST's
# module and renames that section (same length, "gex.abX"): a module from
# before the marker. Cases: Wine's loader, then Wine's PE module (winepe.wasm,
# in a hard-linked copy of DIST). Each must end with the refusal line, exit
# 127, and no trap. Prints "N passed, M failed".
#
#   tests/w3/abirefuse.sh DIST PREFIX
#
# DIST is a W3 Wine install built against this tree's header, PREFIX a Wine
# prefix made for it (copied, never written). Needs the x86 shared-memory
# shim and seed in web/public/gex.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
w=${WORK:-$root/target/w3-abirefuse}
rm -rf "$w"; mkdir -p "$w"
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip abirefuse: no web/public/gex/$f"; exit 0; }
done
# old SRC DST: SRC with its gex.abi section renamed; fails if it has none
old() {
	python3 -I -c '
import sys
b = open(sys.argv[1], "rb").read()
n = b.count(b"\x07gex.abi")
if n == 0:
    sys.exit("no gex.abi section in " + sys.argv[1] + ": DIST was built without the header marker")
open(sys.argv[2], "wb").write(b.replace(b"\x07gex.abi", b"\x07gex.abX"))' "$1" "$2"
}
pass=0 fail=0
# run NAME DIST LOADER WHAT
run() {
	local log=$w/$1.log
	rm -rf "$w/pfx"; cp -a "$prefix" "$w/pfx"
	(cd "$root" && GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 "${TMO:-300}" node --max-old-space-size=8192 \
		shims/browser/js/run-node.mjs --dir /w3="$2" --dir /pfx="$w/pfx" \
		--env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
		--cwd /pfx/drive_c --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
		"$3" 'C:\windows\system32\cmd.exe' /c exit 0; echo "exit=$?") > "$log" 2>&1
	local code; code=$(sed -n 's/^exit=//p' "$log" | tail -1)
	if grep -q "$4 .*built for gex's native ABI 1" "$log" && ! grep -qi 'signature mismatch' "$log" && [ "$code" = 127 ]; then
		pass=$((pass + 1)); echo "ok   abirefuse $1 (refused, exit $code)"
	else
		fail=$((fail + 1)); echo "FAIL abirefuse $1: exit $code"; grep -v '^\[host\]' "$log" | tail -8 | sed 's/^/  /'
	fi
}
# Wine's loader, from before the marker
old "$dist/lib/wine/i386-unix/wine" "$w/wine-old" || exit 1
chmod +x "$w/wine-old"
run loader "$dist" "$w/wine-old" "Wine's loader"
# Wine's PE module, from before the marker, in a copy of DIST
cp -al "$dist" "$w/dist"
rm -f "$w/dist/lib/wine/i386-windows/winepe.wasm" "$w/dist/lib/wine/i386-windows/"winepe.*.cwasm
old "$dist/lib/wine/i386-windows/winepe.wasm" "$w/dist/lib/wine/i386-windows/winepe.wasm" || exit 1
run pe "$w/dist" "$dist/lib/wine/i386-unix/wine" "Wine's PE module"
rm -rf "$w/dist" "$w/pfx"
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
