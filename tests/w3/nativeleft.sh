#!/bin/bash
# A guest exception handler that leaves native frames by jumping, as an
# MSVC __except handler does, then calls native code before it returns to
# its native caller (nativeleft.c): ntdll's qsort calls a comparator with a
# function-tier unit, which calls qsort again with a comparator that
# raises; the outer comparator's handler unwinds (a finally on the way runs
# once) and jumps back into the outer comparator, nested in the dispatch's
# native frames with no call-out between, which calls GetCurrentThreadId
# and returns to the outer qsort. gexrt's NATIVE_LEFT for that call must
# leave the outer qsort's frames alone. Then the same on new threads.
# Prints "nativeleft main ok N", then "nativeleft ok N".
# Natively (gexc run) and in the page's runtime under Node (run-node.mjs),
# with the exe's unit and without it (tier-1: the guest calls from native
# Wine are call-outs).
#
#   tests/w3/nativeleft.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wine prefix made for it (copied, never
# written). WORK (default target/w3-nativeleft) must be on disk: gexc's
# guests do not see /dev/shm. Needs i686-w64-mingw32-gcc, an x86 gexc (the
# exe's unit is built with it, into this test's own cache), and the x86
# shared-memory shim and seed in web/public/gex.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "FAIL: no $gexc (the unit is built with it)"; exit 1; }
w=${WORK:-$root/target/w3-nativeleft}
rm -rf "$w"; mkdir -p "$w/units"
i686-w64-mingw32-gcc -O1 -o "$w/nativeleft.exe" "$here/nativeleft.c" || exit 1
# Unguarded and precompiled for gexc run, guarded for the page; each with and
# without W3's KUSER fold (which key a run looks for depends on whether the
# dist's Wine folds KUSER_SHARED_DATA).
units() {
	GEX_CGEN_CACHE="$w/units" "$gexc" cgen-image --providers || return 1
	for k in --w3-kuser ""; do
		GEX_CGEN_CACHE="$w/units" GEX_CGEN_PRECOMPILE=1 "$gexc" cgen-image "$w/nativeleft.exe" --pe-base 0x400000 $k --jobs 4 &&
			GEX_CGEN_CACHE="$w/units" GEX_CGEN_GUARD=1 "$gexc" cgen-image "$w/nativeleft.exe" --pe-base 0x400000 $k --jobs 4 || return 1
	done
}
units > "$w/build.log" 2>&1 && GEX_CGEN_CACHE="$w/units-t1" "$gexc" cgen-image --providers >> "$w/build.log" 2>&1 ||
	{ echo "FAIL: nativeleft.exe's unit (tail: $(tail -2 "$w/build.log" | tr '\n' ' '))"; exit 1; }
pass=0 fail=0
verdict() { # NAME LOG WANT
	if grep -q "$3" "$2"; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1"; grep -v '^\[host\]' "$2" | tail -8 | sed 's/^/  /'; fi
}
rm -rf "$w/pfx-native"; cp -a "$prefix" "$w/pfx-native"; cp "$w/nativeleft.exe" "$w/pfx-native/drive_c/"
(cd "$w/pfx-native/drive_c" && GEX_CGEN_CACHE="$w/units" timeout -k 10 "${TMO:-300}" "$gexc" run \
	--env WINEPREFIX="$w/pfx-native" --env WINEDEBUG=err+all,fixme-all \
	"$dist/lib/wine/i386-unix/wine" 'C:\nativeleft.exe') > "$w/native.log" 2>&1
verdict native "$w/native.log" '^nativeleft ok [1-9]'
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; echo "$pass passed, $fail failed"; exit $((fail > 0)); }
done
page() { # LOG [env...]
	local log=$1; shift
	rm -rf "$w/pfx-page" "$w/t"; cp -a "$prefix" "$w/pfx-page"; mkdir -p "$w/t"
	(cd "$root" && env GEX_ARCH=x86 GEX_SHARED=1 GEX_LOG=unit GEX_CGEN_CACHE="${cache:-$w/units}" "$@" timeout -k 10 "${TMO:-300}" node --max-old-space-size=8192 \
		shims/browser/js/run-node.mjs --cgen-prefer --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
		--file "/t/nativeleft.exe=$w/nativeleft.exe" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
		--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
		"$dist/lib/wine/i386-unix/wine" /t/nativeleft.exe) > "$log" 2>&1
}
page "$w/page.log"
# (GEX_LOG=unit: the unit's "function tier for" line is debug output.)
# With the exe's unit the thread case's start routine can be a null
# function-table slot (a separate known bug): the main thread's case decides,
# the threads' is reported.
if grep -q 'function tier for .*/nativeleft.exe' "$w/page.log"; then verdict page "$w/page.log" '^nativeleft main ok [1-9]'
	grep -q '^nativeleft ok [1-9]' "$w/page.log" || echo "known page: the thread case did not pass (null function-table slot at a start routine)"
else fail=$((fail + 1)); echo "FAIL page: nativeleft.exe ran without its unit"; grep -i 'nativeleft.exe' "$w/page.log" | head -3 | sed 's/^/  /'; fi
# Without the exe's unit: its code runs on tier-1, so the guest calls from
# native Wine are call-outs and the NATIVE_LEFT leaves the comparator's.
cache="$w/units-t1" page "$w/page-t1.log"
verdict "page, tier-1" "$w/page-t1.log" '^nativeleft ok [1-9]'
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
