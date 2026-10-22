#!/bin/bash
# A guest function called from native Wine code catches an exception its
# callee raised, in its own frame, and returns (strayret.c): ntdll's qsort
# calls a comparator that has a function-tier unit, the comparator's callee
# (generated code, a call-out on tier-1) raises an exception, and the comparator's SEH
# handler resumes it past the call. The comparator then returns to native
# Wine's return pad with the call-out still open; gex left the call-outs
# and the pad trapped as "breakpoint" at 0x3ffff1 (cgen.rs
# `native_return_stray`). Then the same on new threads, whose start routine
# native Wine calls: a handler resuming in the start routine, and one that
# resumes it past an ntdll qsort whose comparator raised, so the start
# routine returns to the pad for the thread's native caller while the
# comparator's call-out is still open (gex took that return as the
# comparator's). Prints "strayret ok N".
# Natively (gexc run) and in the page's runtime under Node (run-node.mjs),
# with the exe's unit; and once in the page with GEX_STRAY_TRAP=1 (the old
# behaviour, `?straytrap=1`), which must reach the breakpoint, so the test
# is known to take the path.
#
#   tests/w3/strayret.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wine prefix made for it (copied, never
# written). WORK (default target/w3-strayret) must be on disk: gexc's guests
# do not see /dev/shm. Needs i686-w64-mingw32-gcc, an x86 gexc (the exe's unit is built
# with it, into this test's own cache), and the x86 shared-memory shim and
# seed in web/public/gex.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "FAIL: no $gexc (the unit is built with it)"; exit 1; }
w=${WORK:-$root/target/w3-strayret}
rm -rf "$w"; mkdir -p "$w/units"
i686-w64-mingw32-gcc -O1 -o "$w/strayret.exe" "$here/strayret.c" || exit 1
# Unguarded and precompiled for gexc run, guarded for the page; each with and
# without W3's KUSER fold (which key a run looks for depends on whether the
# dist's Wine folds KUSER_SHARED_DATA).
units() {
	GEX_CGEN_CACHE="$w/units" "$gexc" cgen-image --providers || return 1
	for k in --w3-kuser ""; do
		GEX_CGEN_CACHE="$w/units" GEX_CGEN_PRECOMPILE=1 "$gexc" cgen-image "$w/strayret.exe" --pe-base 0x400000 $k --jobs 4 &&
			GEX_CGEN_CACHE="$w/units" GEX_CGEN_GUARD=1 "$gexc" cgen-image "$w/strayret.exe" --pe-base 0x400000 $k --jobs 4 || return 1
	done
}
units > "$w/build.log" 2>&1 && GEX_CGEN_CACHE="$w/units-t1" "$gexc" cgen-image --providers >> "$w/build.log" 2>&1 ||
	{ echo "FAIL: strayret.exe's unit (tail: $(tail -2 "$w/build.log" | tr '\n' ' '))"; exit 1; }
pass=0 fail=0
verdict() { # NAME LOG WANT
	if grep -q "$3" "$2"; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1"; grep -v '^\[host\]' "$2" | tail -8 | sed 's/^/  /'; fi
}
rm -rf "$w/pfx-native"; cp -a "$prefix" "$w/pfx-native"; cp "$w/strayret.exe" "$w/pfx-native/drive_c/"
(cd "$w/pfx-native/drive_c" && GEX_CGEN_CACHE="$w/units" timeout -k 10 "${TMO:-300}" "$gexc" run \
	--env WINEPREFIX="$w/pfx-native" --env WINEDEBUG=err+all,fixme-all \
	"$dist/lib/wine/i386-unix/wine" 'C:\strayret.exe') > "$w/native.log" 2>&1
verdict native "$w/native.log" '^strayret ok [1-9]'
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; echo "$pass passed, $fail failed"; exit $((fail > 0)); }
done
page() { # LOG [env...]
	local log=$1; shift
	rm -rf "$w/pfx-page" "$w/t"; cp -a "$prefix" "$w/pfx-page"; mkdir -p "$w/t"
	(cd "$root" && env GEX_ARCH=x86 GEX_SHARED=1 GEX_LOG=unit GEX_CGEN_CACHE="${cache:-$w/units}" "$@" timeout -k 10 "${TMO:-300}" node --max-old-space-size=8192 \
		shims/browser/js/run-node.mjs --cgen-prefer --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
		--file "/t/strayret.exe=$w/strayret.exe" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
		--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
		"$dist/lib/wine/i386-unix/wine" /t/strayret.exe) > "$log" 2>&1
}
page "$w/page.log"
# (GEX_LOG=unit: the unit's "function tier for" line is debug output.)
if grep -q 'function tier for .*/strayret.exe' "$w/page.log"; then verdict page "$w/page.log" '^strayret ok [1-9]'
else fail=$((fail + 1)); echo "FAIL page: strayret.exe ran without its unit"; grep -i 'strayret.exe' "$w/page.log" | head -3 | sed 's/^/  /'; fi
# Without the exe's unit: its code runs on tier-1, so the guest calls from
# native Wine are call-outs, and one open for the comparator sees the start
# routine's return to the pad.
cache="$w/units-t1" page "$w/page-t1.log"
verdict "page, tier-1" "$w/page-t1.log" '^strayret ok [1-9]'
page "$w/page-trap.log" GEX_STRAY_TRAP=1
# The breakpoint at the pad (0x3ffff1) is Wine's unhandled 0x80000003 there,
# whether or not winedbg then manages to attach and name it.
verdict "page, GEX_STRAY_TRAP=1 reaches the breakpoint" "$w/page-trap.log" 'breakpoint\|exception 0x80000003 .*at address 003FFFF1'
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
