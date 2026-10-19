#!/bin/bash
# An x64 program whose preferred base is out of W64's reach (mingw's
# 0x140000000) starts as the first process, relocated, natively and in the
# page (run-node.mjs): no start.exe /exec restart (mainexe.c checks the
# process list, its base and its relocated pointers), and no second
# process for it (one exec of it). Wine's ntdll accepts the relocated main
# image (STATUS_IMAGE_NOT_AT_BASE) on W64 (Wine branch g3/w64-main-exe).
# Prints "ok   mainexe64 <leg>" per leg.
#
#   tests/wine64/mainexe.sh DIST PREFIX [gexc]
#
# DIST is a W64 install, PREFIX a win64 prefix for it (copied, never
# written). Needs x86_64-w64-mingw32-gcc and an x86 gexc. The page leg needs
# web/public/gex's x86 shared-memory shim and seed (PUB=DIR: another
# directory holding them).
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
w=${WORK:-$root/target/w64-mainexe}
rm -rf "$w"; mkdir -p "$w"
x86_64-w64-mingw32-gcc -O1 -Wall -o "$w/mainexe.exe" "$here/mainexe.c" || exit 1
fail=0
verdict() { # LEG LOG
	local execs; execs=$(grep -ac 'exec .*mainexe\.exe' "$2")
	grep -aE '^(ok  |FAIL)' "$2" | tr -d '\r' | sed "s/^/  $1: /"
	if grep -aq '^mainexe ok' "$2" && [ "$execs" -le 1 ]; then echo "ok   mainexe64 $1"; else
		echo "FAIL mainexe64 $1 (execs of mainexe.exe: $execs)"; grep -av '^\(ok  \|FAIL\)' "$2" | grep -a 'start\.exe\|mainexe\|wine:' | tail -8 | sed 's/^/  /'
		fail=$((fail + 1)); fi
}
if [ -x "$gexc" ]; then
	rm -rf "$w/pfx"; cp -a "$prefix" "$w/pfx"; cp "$w/mainexe.exe" "$w/pfx/drive_c/"
	(cd "$w/pfx/drive_c" && timeout -k 10 "${TMO:-300}" "$gexc" run --no-cgen \
		--env WINEPREFIX="$w/pfx" --env WINEDEBUG=err+all,fixme-all --env WINEGEX_DOOR=0 \
		"$dist/lib/wine/x86_64-unix/wine" 'C:\mainexe.exe') > "$w/native.log" 2>&1
	verdict native "$w/native.log"
else
	echo "skip native: no $gexc"
fi
pub=$(realpath -m "${PUB:-$root/web/public/gex}")
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$pub/$f" ] || { echo "skip page: no $pub/$f"; exit $((fail > 0)); }
done
rm -rf "$w/pfx-page"; cp -a "$prefix" "$w/pfx-page"; cp "$w/mainexe.exe" "$w/pfx-page/drive_c/"
(cd "$root" && GEX_WEB_DIR="$pub" GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 "${TMO:-300}" \
	node --max-old-space-size=8192 shims/browser/js/run-node.mjs --dir /w64="$dist" --dir /pfx="$w/pfx-page" \
	--env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all --env WINEGEX_DOOR=0 \
	--cwd /pfx/drive_c --argv0 /w64/lib/wine/x86_64-unix/wine \
	"$pub/seed.x86.shared.wasm" "$dist/lib/wine/x86_64-unix/wine" 'C:\mainexe.exe') > "$w/page.log" 2>&1
verdict page "$w/page.log"
[ "$fail" = 0 ]
