#!/bin/bash
# Dispatch pages numbered past the first dispatch table, natively
# (tables.c). The slot space spans every table: a large unit fills the first
# and the pages numbered after it land in later ones. The native host's
# bridges and call-out watches reached only the first, so a bridged page
# there kept a zero directory entry (a retranslation livelock), and a tier-1
# return to native Wine's return pad went unwatched into the pad's int3
# filler (EXCEPTION_BREAKPOINT at 0x3ffff1). Here every table holds only
# GEX_DISPATCH_TABLE_SLOTS slots (default below), so a small session numbers
# most of its pages in the later tables; a run with the full tables is the
# control. Prints "tables ok N" per run.
#
#   tests/w3/tables.sh DIST PREFIX [gexc]
#
# DIST is a wine32 install for gex, PREFIX a Wine prefix made for it (copied, never
# written). WORK (default target/w3-tables) must be on disk: gexc's guests do
# not see /dev/shm. Needs i686-w64-mingw32-gcc and an x86 gexc (the exe's unit
# is built with it, into this test's own cache).
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "FAIL: no $gexc (the unit is built with it)"; exit 1; }
slots=${SLOTS:-262144}
w=${WORK:-$root/target/w3-tables}
rm -rf "$w"; mkdir -p "$w/units"
i686-w64-mingw32-gcc -O1 -o "$w/tables.exe" "$here/tables.c" || exit 1
i686-w64-mingw32-gcc -O1 -shared -nostdlib -e _DllMain@12 -o "$w/tablesdll.dll" "$here/tablesdll.c" -lkernel32 || exit 1
(GEX_CGEN_CACHE="$w/units" "$gexc" cgen-image --providers &&
	GEX_CGEN_CACHE="$w/units" GEX_CGEN_PRECOMPILE=1 "$gexc" cgen-image "$w/tables.exe" --pe-base 0x400000 --w3-kuser --jobs 4) > "$w/build.log" 2>&1 ||
	{ echo "FAIL: tables.exe's unit (tail: $(tail -2 "$w/build.log" | tr '\n' ' '))"; exit 1; }
pass=0 fail=0
run() { # NAME LOG [env...]
	local name=$1 log=$2; shift 2
	rm -rf "$w/pfx"; cp -a "$prefix" "$w/pfx"; cp "$w/tables.exe" "$w/tablesdll.dll" "$w/pfx/drive_c/"
	(cd "$w/pfx/drive_c" && env GEX_CGEN_CACHE="$w/units" GEX_W3_UNITS=all GEX_W3_TRACE=1 "$@" timeout -k 10 "${TMO:-300}" "$gexc" run \
		--env WINEPREFIX="$w/pfx" --env WINEDEBUG=err+all,fixme-all \
		"$dist/lib/wine/i386-unix/wine" 'C:\tables.exe') > "$log" 2>&1
	if grep -q 'no function-tier unit.*tables\.exe' "$log"; then
		fail=$((fail + 1)); echo "FAIL $name: tables.exe ran without its unit"; grep 'tables\.exe' "$log" | head -2 | sed 's/^/  /'
	elif grep -q '^tables ok [1-9]' "$log"; then pass=$((pass + 1)); echo "ok   $name"
	else fail=$((fail + 1)); echo "FAIL $name"; grep -v '^\[host\]' "$log" | tail -8 | sed 's/^/  /'; fi
}
run "full tables" "$w/full.log"
run "tables of $slots slots" "$w/small.log" GEX_DISPATCH_TABLE_SLOTS="$slots"
grep -q "dispatch tables of $slots slots" "$w/small.log" || { fail=$((fail + 1)); echo "FAIL: the cap was not taken (no \"dispatch tables of $slots slots\" line)"; }
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
