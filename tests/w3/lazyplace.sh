#!/bin/bash
# An x86 image mapped and never run is never read, hashed or placed; one that
# runs gets its unit at its first miss (cgen.rs w3_may_defer, w3_miss). Both
# DLLs have a unit at their bases: lazyplace_a.dll is only mapped
# (DONT_RESOLVE_DLL_REFERENCES), lazyplace_b.dll is called. Before, every
# image with a unit was placed as it was mapped: wineboot's KnownDlls scan,
# a game's exe in the first process. In the page's runtime under Node
# (run-node.mjs).
#
#   tests/w3/lazyplace.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wine prefix made for it (copied, never
# written). Run 1 finds where the DLLs are mapped, their units are built
# there (gexc cgen-image, into this test's own cache). Needs
# i686-w64-mingw32-gcc, an x86 gexc, and the x86 shared-memory shim and seed
# in web/public/gex (PUB=DIR: another directory holding them).
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "FAIL: no $gexc (the units are built with it)"; exit 1; }
pub=$(realpath -m "${PUB:-$root/web/public/gex}")
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$pub/$f" ] || { echo "skip: no $pub/$f"; exit 0; }
done
w=${WORK:-$root/target/w3-lazyplace}
rm -rf "$w"; mkdir -p "$w/units"
i686-w64-mingw32-gcc -O1 -o "$w/lazyplace.exe" "$here/lazyplace.c" || exit 1
for d in a b; do
	i686-w64-mingw32-gcc -O1 -shared -nostdlib -e _DllMain@12 -o "$w/lazyplace_$d.dll" "$here/lazyplacedll.c" -lkernel32 || exit 1
done
unit() { # IMAGE BASE: the guarded and unguarded units, with W3's KUSER fold (the page's keys)
	local g
	for g in "" 1; do
		GEX_CGEN_GUARD=$g GEX_CGEN_CACHE="$w/units" GEX_CGEN_PRECOMPILE=1 "$gexc" cgen-image "$1" --pe-base "$2" --w3-kuser --jobs 4 >> "$w/build.log" 2>&1 ||
			{ echo "FAIL: unit of $1 at $2 (tail: $(tail -2 "$w/build.log" | tr '\n' ' '))"; exit 1; }
	done
}
page() { # LOG
	rm -rf "$w/pfx-page" "$w/t"; cp -a "$prefix" "$w/pfx-page"; mkdir -p "$w/t"
	(cd "$root" && env GEX_WEB_DIR="$pub" GEX_ARCH=x86 GEX_SHARED=1 GEX_CGEN_CACHE="$w/units" \
		timeout -k 10 "${TMO:-300}" node --max-old-space-size=8192 \
		shims/browser/js/run-node.mjs --cgen-prefer --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
		--file "/t/lazyplace.exe=$w/lazyplace.exe" --file "/t/lazyplace_a.dll=$w/lazyplace_a.dll" \
		--file "/t/lazyplace_b.dll=$w/lazyplace_b.dll" \
		--env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
		--cwd /t --argv0 /w3/lib/wine/i386-unix/wine "$pub/seed.x86.shared.wasm" \
		"$dist/lib/wine/i386-unix/wine" /t/lazyplace.exe) > "$1" 2>&1
}
GEX_CGEN_CACHE="$w/units" "$gexc" cgen-image --providers > "$w/providers.log" 2>&1 ||
	{ echo "FAIL: provider modules (tail: $(tail -2 "$w/providers.log" | tr '\n' ' '))"; exit 1; }
unit "$w/lazyplace.exe" 0x400000
page "$w/p0.log"
for d in a b; do
	eval base_$d=$(sed -n "s/^lazyplace_$d at \(0x\)\?\([0-9a-fA-F]*\).*/0x\2/p" "$w/p0.log" | head -1)
done
[ -n "$base_a" ] && [ -n "$base_b" ] || { echo "FAIL: the DLLs did not load"; grep -v '^\[host\]' "$w/p0.log" | tail -8 | sed 's/^/  /'; exit 1; }
unit "$w/lazyplace_a.dll" "$base_a"
unit "$w/lazyplace_b.dll" "$base_b"
page "$w/p1.log"
name="page MP, a mapped image that never runs is not placed, a called one is"
fail=""
grep -q "^lazyplace ok " "$w/p1.log" || fail="the program did not finish"
grep -q "loaded .*/lazyplace_b.dll (with its unit)" "$w/p1.log" || fail="${fail:+$fail; }lazyplace_b.dll ran without its unit"
grep -q "loaded .*/lazyplace_a.dll" "$w/p1.log" && fail="${fail:+$fail; }lazyplace_a.dll was placed: $(grep -m1 "loaded .*/lazyplace_a.dll" "$w/p1.log")"
if [ -z "$fail" ]; then echo "ok   $name"; echo "1 passed, 0 failed"
else echo "FAIL $name: $fail"; grep -a "lazyplace\|loaded " "$w/p1.log" | grep -v '^\[host\]' | tail -8 | sed 's/^/  /'; echo "0 passed, 1 failed"; exit 1; fi
