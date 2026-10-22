#!/bin/bash
# Wine's msvcr120 heap on W3 with and without gex's crt_heap_wine provider
# (crtheap.c): the same answers both ways (sizes as _msize gives them,
# contents kept across realloc, zeroing, alignment, the NULL edges, a
# 200000-operation churn), and the blocks from dlmalloc with the provider
# ("owner other") and from Wine's process heap without it ("owner wine").
# Natively (gexc run) and in the page's runtime under Node (run-node.mjs).
#
#   tests/w3/crtheap.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wine prefix made for it (copied, never
# written). Run 1 finds where msvcr120 is placed; its unit is built there
# with and without GEX_PROVIDER_SKIP for the heap exports, into this test's
# own cache. Needs i686-w64-mingw32-gcc, an x86 gexc, and for the page legs
# the x86 shared-memory shim and seed in web/public/gex.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "FAIL: no $gexc (the units are built with it)"; exit 1; }
skip='malloc,_malloc_crt,calloc,_calloc_crt,free,realloc,_realloc_crt,_recalloc,_msize,_expand,??2@YAPAXI@Z,??_U@YAPAXI@Z,??2@YAPAXIHPBDH@Z,??_U@YAPAXIHPBDH@Z,??3@YAXPAX@Z,??_V@YAXPAX@Z,_aligned_offset_realloc,_aligned_realloc'
w=${WORK:-$root/target/w3-crtheap}
rm -rf "$w"; mkdir -p "$w/units"
i686-w64-mingw32-gcc -O1 -o "$w/crtheap.exe" "$here/crtheap.c" || exit 1
pass=0 fail=0
native() { # LOG [env...]
	local log=$1; shift
	rm -rf "$w/pfx-native"; cp -a "$prefix" "$w/pfx-native"; cp "$w/crtheap.exe" "$w/pfx-native/drive_c/"
	(cd "$w/pfx-native/drive_c" && env GEX_CGEN_CACHE="$w/units" "$@" timeout -k 10 "${TMO:-300}" "$gexc" run \
		--env WINEPREFIX="$w/pfx-native" --env WINEDEBUG=err+all,fixme-all \
		"$dist/lib/wine/i386-unix/wine" 'C:\crtheap.exe') > "$log" 2>&1
	sed -i 's/\r$//' "$log"
}
page() { # LOG [env...]
	local log=$1; shift
	rm -rf "$w/pfx-page" "$w/t"; cp -a "$prefix" "$w/pfx-page"; mkdir -p "$w/t"
	(cd "$root" && env GEX_ARCH=x86 GEX_SHARED=1 GEX_CGEN_CACHE="$w/units" "$@" timeout -k 10 "${TMO:-300}" node --max-old-space-size=8192 \
		shims/browser/js/run-node.mjs --cgen-prefer --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
		--file "/t/crtheap.exe=$w/crtheap.exe" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
		--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
		"$dist/lib/wine/i386-unix/wine" /t/crtheap.exe) > "$log" 2>&1
	sed -i 's/\r$//' "$log"
}
answers() { grep -E '^(size |crt|calloc|recalloc|expand|realloc |msize |new:|aligned|churn)' "$1"; }
verdict() { # NAME LOG OWNER [REFERENCE_LOG]
	local why=
	grep -q '^crtheap done' "$2" || why="no 'crtheap done'"
	[ -z "$why" ] && ! grep -q "^owner $3\$" "$2" && why="want owner $3, got '$(grep '^owner' "$2")'"
	[ -z "$why" ] && grep -q '^churn: [0-9]* bytes, [1-9]' "$2" && why="churn: $(grep '^churn' "$2")"
	if [ -z "$why" ] && [ -n "${4:-}" ] && ! diff <(answers "$4") <(answers "$2") > "$w/diff.txt"; then why="answers differ from Wine's own: $(head -4 "$w/diff.txt" | tr '\n' ' ')"; fi
	if [ -z "$why" ]; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1 ($why)"; grep -v '^\[host\]\|gex-console-error' "$2" | tail -6 | sed 's/^/  /'; fi
}
# With and without W3's KUSER fold: which key a run looks for depends on
# whether the dist's Wine folds KUSER_SHARED_DATA.
build() { # BASE [env...]
	local base=$1; shift
	(for k in --w3-kuser ""; do
		env GEX_CGEN_CACHE="$w/units" GEX_CGEN_PRECOMPILE=1 "$@" "$gexc" cgen-image "$dist/lib/wine/i386-windows/msvcr120.dll" --pe-base "$base" $k --jobs 4 || exit 1
	done) > "$w/build.log" 2>&1 ||
		{ echo "FAIL: msvcr120 unit at $base (tail: $(tail -2 "$w/build.log" | tr '\n' ' '))"; exit 1; }
}
GEX_CGEN_CACHE="$w/units" "$gexc" cgen-image --providers > "$w/providers.log" 2>&1 || { echo "FAIL: provider modules (tail: $(tail -2 "$w/providers.log" | tr '\n' ' '))"; exit 1; }

native "$w/n0.log"
nbase=$(sed -n 's/^msvcr120 at \(0x\)\?\([0-9a-fA-F]*\).*/0x\2/p' "$w/n0.log" | head -1)
verdict "native, no unit" "$w/n0.log" wine
if [ -n "$nbase" ]; then
	build "$nbase"; build "$nbase" GEX_PROVIDER_SKIP="$skip"
	native "$w/n2.log" GEX_PROVIDER_SKIP="$skip"; verdict "native, provider skipped" "$w/n2.log" wine "$w/n0.log"
	native "$w/n1.log"; verdict "native, provider" "$w/n1.log" other "$w/n0.log"
fi
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; echo "$pass passed, $fail failed"; exit $((fail > 0)); }
done
page "$w/p0.log"
pbase=$(sed -n 's/^msvcr120 at \(0x\)\?\([0-9a-fA-F]*\).*/0x\2/p' "$w/p0.log" | head -1)
if [ -n "$pbase" ] && [ "$pbase" != "$nbase" ]; then
	build "$pbase"; build "$pbase" GEX_PROVIDER_SKIP="$skip"
	page "$w/p0.log"
fi
verdict "page, provider" "$w/p0.log" other "$w/n0.log"
page "$w/p1.log" GEX_PROVIDER_SKIP="$skip"; verdict "page, provider skipped" "$w/p1.log" wine "$w/n0.log"
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
