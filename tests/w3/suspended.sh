#!/bin/bash
# A process created suspended runs nothing until it is resumed
# (suspended.c): the child of CreateProcess(CREATE_SUSPENDED) must not make
# its marker file while the parent waits, and must make it once resumed,
# ROUNDS times, with the parent's pipe busy meanwhile. Prints "suspended ok
# ROUNDS". Natively (gexc run) and in the page's runtime under Node
# (run-node.mjs, the shim and glue a page loads).
#
#   tests/w3/suspended.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install (lib/wine/i386-unix/wine), PREFIX a Wine prefix
# made for it (copied, never written). ROUNDS (default 50) and WAIT_MS
# (default 2000) set the loop. Needs i686-w64-mingw32-gcc, the x86
# shared-memory shim and seed in web/public/gex (GEX_ARCH=x86
# shims/browser/build.sh) and, for the native leg, an x86 gexc.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
n=${ROUNDS:-50} ms=${WAIT_MS:-2000}
w=${WORK:-$root/target/w3-suspended}
chmod -R u+w "$w" 2>/dev/null; rm -rf "$w"; mkdir -p "$w"
i686-w64-mingw32-gcc -O1 -o "$w/suspended.exe" "$here/suspended.c" || exit 1
want="suspended ok $n"
pass=0 fail=0
verdict() { # NAME LOG
	if grep -q "^$want" "$2"; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1"; grep -v '^\[host\]\|gex-console-error' "$2" | tail -8 | sed 's/^/  /'; fi
}
if [ -x "$gexc" ]; then
	cp -a "$prefix" "$w/pfx-native"
	cp "$w/suspended.exe" "$w/pfx-native/drive_c/"
	(cd "$w/pfx-native/drive_c" && timeout -k 10 "${TMO:-900}" "$gexc" run \
		--env WINEPREFIX="$w/pfx-native" --env WINEDEBUG=err+all,fixme-all \
		"$dist/lib/wine/i386-unix/wine" 'C:\suspended.exe' "$n" "$ms") > "$w/native.log" 2>&1
	verdict native "$w/native.log"
else
	echo "skip native: no $gexc"
fi
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; exit $((fail > 0)); }
done
cp -a "$prefix" "$w/pfx-page"
mkdir -p "$w/t"
(cd "$root" && GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 "${TMO:-900}" node --max-old-space-size=8192 \
	shims/browser/js/run-node.mjs --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
	--file "/t/suspended.exe=$w/suspended.exe" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
	--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
	"$dist/lib/wine/i386-unix/wine" /t/suspended.exe "$n" "$ms") > "$w/page.log" 2>&1
verdict page "$w/page.log"
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
