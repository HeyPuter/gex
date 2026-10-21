#!/bin/bash
# An AOT-only wine32 session (GEX_AOT_ONLY=1, as an ejected project runs:
# units only, nothing translated) faults as a translated one does
# (aotfault.c):
# - a call into a page that is not executable reaches the program's own
#   handler as an access violation, executing ("aotfault dep ok");
# - a call into code made at run time stops the run, naming it as code
#   outside every image;
# - a call through a window-procedure thunk made at run time, of either
#   documented shape, runs; one of another shape stops as above.
# A first, ordinary run of each case names the images it ran without a
# unit; those are built (minutes: Wine's own DLLs) and, with the exe's,
# are all an AOT-only run has.
#
#   tests/w3/aotfault.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wine prefix made for it (copied, never
# written). WORK (default target/w3-aotfault) must be on disk; UNITS
# (default WORK/units) keeps the unit cache across runs. WORK: gexc's guests
# do not see /dev/shm. Needs i686-w64-mingw32-gcc and an x86 gexc.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "FAIL: no $gexc"; exit 1; }
w=${WORK:-$root/target/w3-aotfault}
rm -rf "$w"; u=${UNITS:-$w/units}; mkdir -p "$w" "$u"
i686-w64-mingw32-gcc -O1 -o "$w/aotfault.exe" "$here/aotfault.c" || exit 1
(GEX_CGEN_CACHE="$u" "$gexc" cgen-image --providers &&
	GEX_CGEN_CACHE="$u" GEX_CGEN_PRECOMPILE=1 "$gexc" cgen-image "$w/aotfault.exe" --pe-base 0x400000 --jobs 4) > "$w/build.log" 2>&1 ||
	{ echo "FAIL: aotfault.exe's unit (tail: $(tail -2 "$w/build.log" | tr '\n' ' '))"; exit 1; }
rm -rf "$w/pfx"; cp -a "$prefix" "$w/pfx"; cp "$w/aotfault.exe" "$w/pfx/drive_c/"
run() { # LOG ARG [env...]
	local log=$1 arg=$2; shift 2
	(cd "$w/pfx/drive_c" && env GEX_CGEN_CACHE="$u" GEX_W3_UNITS=all "$@" timeout -k 10 "${TMO:-300}" "$gexc" run \
		--env WINEPREFIX="$w/pfx" --env WINEDEBUG=err+all,fixme-all \
		"$dist/lib/wine/i386-unix/wine" 'C:\aotfault.exe' $arg) > "$log" 2>&1
}
pass=0 fail=0
verdict() { # NAME LOG WANT
	if grep -aq "$3" "$2"; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1"; grep -av '^\[host\]' "$2" | tail -8 | sed 's/^/  /'; fi
}
for c in hello dep gen thunk; do
	a=$c; [ $c = hello ] && a=
	run "$w/warm-$c.log" "$a" GEX_W3_TRACE=1
done
verdict "translated: dep" "$w/warm-dep.log" '^aotfault dep ok'
# Every image the sessions ran without a unit, built at the base it had
# (Wine's processes place the same DLL at the same base).
grep -ahoE '^gex-w3: [^ ]+ at 0x[0-9a-f]+: no function-tier unit' "$w"/warm-*.log | awk '{print $2, $4}' | tr -d ':' | sort -u > "$w/missing.txt"
echo "building $(wc -l < "$w/missing.txt") units"
while read -r path base; do
	# One with no code to build (a data-only DLL: wasm-ld has no input)
	# needs none; a gap this leaves shows in the AOT-only runs.
	GEX_CGEN_CACHE="$u" GEX_CGEN_PRECOMPILE=1 nice -n 19 "$gexc" cgen-image "$path" --pe-base "$base" --jobs "${JOBS:-6}" >> "$w/build.log" 2>&1 ||
		echo "  no unit for $(basename "$path") at $base: $(tail -1 "$w/build.log" | cut -c1-120)"
done < "$w/missing.txt"
run "$w/hello.log" "" GEX_AOT_ONLY=1
verdict "aot-only: hello" "$w/hello.log" '^aotfault hello'
run "$w/dep.log" dep GEX_AOT_ONLY=1
verdict "aot-only: a non-executable page is the program's access violation" "$w/dep.log" '^aotfault dep ok'
run "$w/gen.log" gen GEX_AOT_ONLY=1
verdict "aot-only: code made at run time stops the run" "$w/gen.log" 'outside every image: code generated at run time'
run "$w/thunk.log" thunk GEX_AOT_ONLY=1
verdict "aot-only: window-procedure thunks made at run time run" "$w/thunk.log" '^aotfault thunk ok'
run "$w/near.log" near GEX_AOT_ONLY=1
verdict "aot-only: a thunk of another shape stops the run" "$w/near.log" 'outside every image: code generated at run time'
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
