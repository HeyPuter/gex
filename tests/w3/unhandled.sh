#!/bin/bash
# Exceptions nothing handles (unhandled.c): each case's process ends with the
# exception's code, after the program's top-level filter, whichever thread
# raised it and whatever native frames lie between, as on Windows. In the
# page's runtime under Node (run-node.mjs) with the exe's unit (its faults
# are delivered in place, under the native frames), without it (tier-1),
# natively (gexc run). Natively and on tier-1
# only cases 1-12: their guest calls from native code nest one call-out
# each, and past the call-out depth cap such a nest does not come back.
#
#   tests/w3/unhandled.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wine prefix made for it (copied, never
# written). WORK (default target/w3-unhandled) must be on disk: gexc's
# guests do not see /dev/shm. Needs i686-w64-mingw32-gcc, an x86 gexc (the
# exe's unit is built with it, into this test's own cache), and the x86
# shared-memory shim and seed in web/public/gex.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "FAIL: no $gexc (the unit is built with it)"; exit 1; }
w=${WORK:-$root/target/w3-unhandled}
rm -rf "$w"; mkdir -p "$w/units"
i686-w64-mingw32-gcc -O1 -o "$w/unhandled.exe" "$here/unhandled.c" || exit 1
want="case 1 exit c000001d
case 2 exit c000001d
case 3 exit 00000055
case 4 exit c000001d
case 5 exit e0001234
case 6 exit c000001d
case 7 exit c000001d
case 8 exit 80000003
case 9 exit c0000094
case 10 exit c0000005
case 11 exit c000001d
case 12 exit c000001d
case 13 exit c000001d
case 14 exit c000001d"
units() {
	GEX_CGEN_CACHE="$w/units" "$gexc" cgen-image --providers || return 1
	for k in --w3-kuser ""; do
		GEX_CGEN_CACHE="$w/units" GEX_CGEN_PRECOMPILE=1 "$gexc" cgen-image "$w/unhandled.exe" --pe-base 0x400000 $k --jobs 4 &&
			GEX_CGEN_CACHE="$w/units" GEX_CGEN_GUARD=1 "$gexc" cgen-image "$w/unhandled.exe" --pe-base 0x400000 $k --jobs 4 || return 1
	done
}
units > "$w/build.log" 2>&1 && GEX_CGEN_CACHE="$w/units-t1" "$gexc" cgen-image --providers >> "$w/build.log" 2>&1 ||
	{ echo "FAIL: unhandled.exe's unit (tail: $(tail -2 "$w/build.log" | tr '\n' ' '))"; exit 1; }
pass=0 fail=0
# LOST: how many exceptions reach the run loop with their handler's frame
# gone (raised again with no handler): only those no code goes on after
# (case 10's KUSER read). Any other is a fault
# delivered from the run loop under native frames it dropped.
verdict() { # NAME LOG CASES LOST
	local got lost; got=$(grep -a '^case [0-9]* exit' "$2" | tr -d '\r' | head -n "$3")
	lost=$(grep -ac 'handler frame that caught it is gone' "$2")
	[ "$lost" = "$4" ] || { got="$got
$lost lost handler frames, not $4"; }
	if [ "$got" = "$(echo "$want" | head -n "$3")" ] && grep -q '^unhandled done' "$2"; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1"; diff <(echo "$want" | head -n "$3") <(echo "$got") | grep '^[<>]' | sed 's/^/  /'
		grep -a -v '^\[host\]' "$2" | grep -a -E 'trapped|gex: ' | head -3 | sed 's/^/  /'; fi
}
rm -rf "$w/pfx-native"; cp -a "$prefix" "$w/pfx-native"; cp "$w/unhandled.exe" "$w/pfx-native/drive_c/"
(cd "$w/pfx-native/drive_c" && GEX_CGEN_CACHE="$w/units" timeout -k 10 "${TMO:-900}" "$gexc" run \
	--env WINEPREFIX="$w/pfx-native" --env WINEDEBUG=err+all,fixme-all \
	"$dist/lib/wine/i386-unix/wine" 'C:\unhandled.exe' -12) > "$w/native.log" 2>&1
verdict native "$w/native.log" 12 0
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; echo "$pass passed, $fail failed"; exit $((fail > 0)); }
done
page() { # LOG LAST [env...]
	local log=$1 last=$2; shift 2
	rm -rf "$w/pfx-page" "$w/t"; cp -a "$prefix" "$w/pfx-page"; mkdir -p "$w/t"
	(cd "$root" && env GEX_ARCH=x86 GEX_SHARED=1 GEX_LOG=unit GEX_CGEN_CACHE="${cache:-$w/units}" "$@" timeout -k 10 "${TMO:-900}" node --max-old-space-size=8192 \
		shims/browser/js/run-node.mjs --cgen-prefer --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
		--file "/t/unhandled.exe=$w/unhandled.exe" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
		--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
		"$dist/lib/wine/i386-unix/wine" /t/unhandled.exe "-$last") > "$log" 2>&1
}
page "$w/page.log" 14
# (GEX_LOG=unit: the unit's "function tier for" line is debug output.)
if grep -q 'function tier for .*/unhandled.exe' "$w/page.log"; then verdict page "$w/page.log" 14 1
else fail=$((fail + 1)); echo "FAIL page: unhandled.exe ran without its unit"; fi
cache="$w/units-t1" page "$w/page-t1.log" 12
verdict "page, tier-1" "$w/page-t1.log" 12 0
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
