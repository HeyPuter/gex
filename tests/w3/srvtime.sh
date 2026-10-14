#!/bin/bash
# The wineserver's clock as clients see it (srvtime.c): a server-stamped
# creation time inside the client's window, wait timeouts and relative and
# absolute waitable timers, with time-free requests in between. Under doors
# the server reads its clock lazily (DOOR_VERSION 2, docs/wine-overhead.md).
# Prints "srvtime ok R". Natively (gexc run) and in the page's runtime under
# Node (run-node.mjs).
#
#   tests/w3/srvtime.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wine prefix made for it (copied, never
# written). Needs i686-w64-mingw32-gcc, the x86 shared-memory shim and seed
# in web/public/gex and, for the native legs, an x86 gexc.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
rounds=${ROUNDS:-5}
w=${WORK:-$root/target/w3-srvtime}
rm -rf "$w"; mkdir -p "$w"
i686-w64-mingw32-gcc -O1 -o "$w/srvtime.exe" "$here/srvtime.c" -lntdll || exit 1
want="srvtime ok $rounds"
pass=0 fail=0
verdict() { # NAME LOG
	if grep -q "^$want" "$2"; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1"; grep -v '^\[host\]\|gex-console-error' "$2" | tail -8 | sed 's/^/  /'; fi
}
for mode in doors; do
	extra=()
	if [ -x "$gexc" ]; then
		rm -rf "$w/pfx-native"; cp -a "$prefix" "$w/pfx-native"; cp "$w/srvtime.exe" "$w/pfx-native/drive_c/"
		(cd "$w/pfx-native/drive_c" && timeout -k 10 "${TMO:-300}" "$gexc" run \
			--env WINEPREFIX="$w/pfx-native" --env WINEDEBUG=err+all,fixme-all "${extra[@]}" \
			"$dist/lib/wine/i386-unix/wine" 'C:\srvtime.exe' $rounds) > "$w/native-$mode.log" 2>&1
		verdict "native $mode" "$w/native-$mode.log"
	else
		echo "skip native: no $gexc"
	fi
	for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
		[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; continue 2; }
	done
	rm -rf "$w/pfx-page" "$w/t"; cp -a "$prefix" "$w/pfx-page"; mkdir -p "$w/t"
	(cd "$root" && GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 "${TMO:-300}" node --max-old-space-size=8192 \
		shims/browser/js/run-node.mjs --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
		--file "/t/srvtime.exe=$w/srvtime.exe" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all "${extra[@]}" \
		--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
		"$dist/lib/wine/i386-unix/wine" /t/srvtime.exe $rounds) > "$w/page-$mode.log" 2>&1
	verdict "page $mode" "$w/page-$mode.log"
done
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
