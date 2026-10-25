#!/bin/bash
# win32u's session reads against another thread's writes on W3: the W64
# test (tests/wine64/seqrace.c), built for i686. Natively with the door and
# without it (WINEGEX_DOOR=0), and in the page's runtime under Node
# (run-node.mjs). Each prints "0 failures".
#
#   tests/w3/seqrace.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install (lib/wine/i386-unix/wine), PREFIX a Wine prefix
# made for it (copied, never written). Needs i686-w64-mingw32-gcc, the x86
# shared-memory shim and seed in web/public/gex for the page leg, and an x86
# gexc for the native legs. Without the door and on the page every read is a
# server request: 2000 writes a case there, 20000 natively with the door.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")

w=${WORK:-$root/target/w3-seqrace}
rm -rf "$w"; mkdir -p "$w"
i686-w64-mingw32-gcc -O1 -o "$w/seqrace.exe" "$here/../wine64/seqrace.c" || exit 1
pass=0 fail=0
verdict() { # NAME LOG
	if grep -q "^0 failures" "$2"; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1"; grep -a '^FAIL\|failures' "$2" | tail -8 | sed 's/^/  /'; fi
}
if [ -x "$gexc" ]; then
	for door in 1 0; do
		n=20000; [ $door = 0 ] && n=2000
		cp -a "$prefix" "$w/pfx-$door"
		cp "$w/seqrace.exe" "$w/pfx-$door/drive_c/"
		(cd "$w/pfx-$door/drive_c" && timeout -k 10 "${TMO:-900}" "$gexc" run \
			--env WINEPREFIX="$w/pfx-$door" --env WINEDEBUG=err+all,fixme-all --env WINEGEX_DOOR=$door \
			"$dist/lib/wine/i386-unix/wine" 'C:\seqrace.exe' $n) > "$w/native-$door.log" 2>&1
		verdict "native door=$door" "$w/native-$door.log"
	done
else
	echo "skip native: no $gexc"
fi
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; echo "$pass passed, $fail failed"; exit $((fail > 0)); }
done
cp -a "${PAGE_PREFIX:-$prefix}" "$w/pfx-page"
mkdir -p "$w/t"
(cd "$root" && GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 "${TMO:-900}" node --max-old-space-size=8192 \
	shims/browser/js/run-node.mjs --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
	--file "/t/seqrace.exe=$w/seqrace.exe" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
	--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
	"$dist/lib/wine/i386-unix/wine" /t/seqrace.exe 2000) > "$w/page.log" 2>&1
verdict page "$w/page.log"
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
