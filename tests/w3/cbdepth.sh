#!/bin/bash
# Guest callbacks from native Wine nested deep (cbdepth.c: qsort whose
# comparator sorts again), on tier-1, where each level is a call-out that
# cannot be left. 100 levels must all return ("cbdepth ok 100", exit 0);
# 450, past the host's limit, must end the process with the "nest deeper"
# line and exit 253 (STATUS_STACK_OVERFLOW's low byte), not hang. Natively
# (gexc run) and in the page's runtime under Node (run-node.mjs). Then in the
# page with the exe's unit, where the levels are direct calls between units
# and native Wine: 1000 levels, past the units' depth guard, must all return
# (gexrt goes on in a new nest).
# Prints "N passed, M failed".
#
#   tests/w3/cbdepth.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wine prefix made for it (copied, never
# written). WORK (default target/w3-cbdepth) must be on disk: gexc's guests
# do not see /dev/shm. Needs i686-w64-mingw32-gcc, an x86 gexc, and the x86
# shared-memory shim and seed in web/public/gex (the page cases are skipped
# without them).
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "FAIL: no $gexc"; exit 1; }
w=${WORK:-$root/target/w3-cbdepth}
rm -rf "$w"; mkdir -p "$w/units" "$w/t"
i686-w64-mingw32-gcc -O1 -o "$w/t/cbdepth.exe" "$here/cbdepth.c" || exit 1
# Providers only: every image runs on tier-1.
GEX_CGEN_CACHE="$w/units" "$gexc" cgen-image --providers > "$w/build.log" 2>&1 ||
	{ echo "FAIL: providers (tail: $(tail -2 "$w/build.log" | tr '\n' ' '))"; exit 1; }
# The exe's unit, guarded for the page, with and without W3's KUSER fold.
cp -a "$w/units" "$w/units-u"
for k in --w3-kuser ""; do
	GEX_CGEN_CACHE="$w/units-u" GEX_CGEN_GUARD=1 "$gexc" cgen-image "$w/t/cbdepth.exe" --pe-base 0x400000 $k --jobs 4 >> "$w/build.log" 2>&1 ||
		{ echo "FAIL: cbdepth.exe's unit (tail: $(tail -2 "$w/build.log" | tr '\n' ' '))"; exit 1; }
done
page=1
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip cbdepth page: no web/public/gex/$f"; page=0; }
done
pass=0 fail=0
# check NAME LOG WANT-LINE WANT-EXIT
check() {
	local code; code=$(sed -n 's/^exit=//p' "$2" | tail -1)
	if grep -aq "$3" "$2" && [ "$code" = "$4" ]; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1: exit $code"; grep -av '^\[host\]' "$2" | tail -6 | sed 's/^/  /'; fi
}
# page NAME DEPTH CACHE: in the page
page() {
	rm -rf "$w/pfx"; cp -a "$prefix" "$w/pfx"
	(cd "$root" && GEX_ARCH=x86 GEX_SHARED=1 GEX_CGEN_CACHE="$3" timeout -k 10 "${TMO:-300}" node --max-old-space-size=8192 \
		shims/browser/js/run-node.mjs --cgen-prefer --dir /w3="$dist" --dir /pfx="$w/pfx" --dir /t="$w/t" \
		--env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
		--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
		"$dist/lib/wine/i386-unix/wine" /t/cbdepth.exe "$2"; echo "exit=$?") > "$w/$1.log" 2>&1
}
# run DEPTH WANT-LINE WANT-EXIT
run() {
	rm -rf "$w/pfx"; cp -a "$prefix" "$w/pfx"; cp "$w/t/cbdepth.exe" "$w/pfx/drive_c/"
	(cd "$w/pfx/drive_c" && GEX_CGEN_CACHE="$w/units" timeout -k 10 "${TMO:-300}" "$gexc" run \
		--env WINEPREFIX="$w/pfx" --env WINEDEBUG=err+all,fixme-all \
		"$dist/lib/wine/i386-unix/wine" 'C:\cbdepth.exe' "$1"; echo "exit=$?") > "$w/native-$1.log" 2>&1
	check "native, $1 levels" "$w/native-$1.log" "$2" "$3"
	[ "$page" = 1 ] || return 0
	page "page-$1" "$1" "$w/units"
	check "page, $1 levels" "$w/page-$1.log" "$2" "$3"
}
run 100 '^cbdepth ok 100' 0
run 450 "native Wine's guest calls nest deeper than" 253
if [ "$page" = 1 ]; then
	page page-units 1000 "$w/units-u"
	if grep -aq 'cbdepth.exe (with its unit)' "$w/page-units.log"; then check "page, units, 1000 levels" "$w/page-units.log" '^cbdepth ok 1000' 0
	else fail=$((fail + 1)); echo "FAIL page, units: cbdepth.exe ran without its unit"; fi
fi
rm -rf "$w/pfx"
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
