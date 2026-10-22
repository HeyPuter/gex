#!/bin/bash
# msvcp120's _Thrd_current on W3 (thrd.c), with and without gex's
# msvcp120_thrd provider (docs/wine-overhead.md F1): every call's id is the
# calling thread's, no handle leaks, and with msvcp120's function-tier unit
# in place the provider answers every call (handle 0). Natively (gexc run)
# and in the page's runtime under Node (run-node.mjs).
#
#   tests/w3/thrd.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wine prefix made for it (copied, never
# written). Run 1 finds where msvcp120 is placed; its unit is built there
# (gexc cgen-image, precompiled, into this test's own cache, with and without
# GEX_PROVIDER_SKIP=_Thrd_current); then each leg runs with the provider
# (want zero == every call) and with the skip (want zero == 0: Wine's own
# code). Needs i686-w64-mingw32-gcc, an x86 gexc, and for the page legs the
# x86 shared-memory shim and seed in web/public/gex.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "FAIL: no $gexc (the units are built with it)"; exit 1; }
n=4 calls=2000 all=$(( (4 + 1) * 2000 ))
w=${WORK:-$root/target/w3-thrd}
rm -rf "$w"; mkdir -p "$w/units"
i686-w64-mingw32-gcc -O1 -o "$w/thrd.exe" "$here/thrd.c" || exit 1
pass=0 fail=0
native() { # LOG [env...]
	local log=$1; shift
	rm -rf "$w/pfx-native"; cp -a "$prefix" "$w/pfx-native"; cp "$w/thrd.exe" "$w/pfx-native/drive_c/"
	(cd "$w/pfx-native/drive_c" && env GEX_CGEN_CACHE="$w/units" "$@" timeout -k 10 "${TMO:-300}" "$gexc" run \
		--env WINEPREFIX="$w/pfx-native" --env WINEDEBUG=err+all,fixme-all \
		"$dist/lib/wine/i386-unix/wine" 'C:\thrd.exe' $n $calls) > "$log" 2>&1
}
page() { # LOG [env...]
	local log=$1; shift
	rm -rf "$w/pfx-page" "$w/t"; cp -a "$prefix" "$w/pfx-page"; mkdir -p "$w/t"
	(cd "$root" && env GEX_ARCH=x86 GEX_SHARED=1 GEX_CGEN_CACHE="$w/units" "$@" timeout -k 10 "${TMO:-300}" node --max-old-space-size=8192 \
		shims/browser/js/run-node.mjs --cgen-prefer --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
		--file "/t/thrd.exe=$w/thrd.exe" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
		--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
		"$dist/lib/wine/i386-unix/wine" /t/thrd.exe $n $calls) > "$log" 2>&1
}
verdict() { # NAME LOG WANT_ZERO
	if grep -q "^thrd ok $((n + 1)) $calls zero=$3[[:space:]]*\$" "$2"; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1 (want zero=$3)"; grep -v '^\[host\]\|gex-console-error' "$2" | tail -8 | sed 's/^/  /'; fi
}
# With and without W3's KUSER fold: which key a run looks for depends on
# whether the dist's Wine folds KUSER_SHARED_DATA.
build() { # BASE [env...]
	local base=$1; shift
	(for k in --w3-kuser ""; do
		env GEX_CGEN_CACHE="$w/units" GEX_CGEN_PRECOMPILE=1 "$@" "$gexc" cgen-image "$dist/lib/wine/i386-windows/msvcp120.dll" --pe-base "$base" $k --jobs 4 || exit 1
	done) > "$w/build.log" 2>&1 ||
		{ echo "FAIL: msvcp120 unit at $base (tail: $(tail -2 "$w/build.log" | tr '\n' ' '))"; exit 1; }
}
GEX_CGEN_CACHE="$w/units" "$gexc" cgen-image --providers > "$w/providers.log" 2>&1 || { echo "FAIL: provider modules (tail: $(tail -2 "$w/providers.log" | tr '\n' ' '))"; exit 1; }

native "$w/n0.log"
nbase=$(sed -n 's/^msvcp120 at \(0x\)\?\([0-9a-fA-F]*\).*/0x\2/p' "$w/n0.log" | head -1)
verdict "native, no unit" "$w/n0.log" 0
if [ -n "$nbase" ]; then
	build "$nbase"; build "$nbase" GEX_PROVIDER_SKIP=_Thrd_current
	native "$w/n1.log"; verdict "native, provider" "$w/n1.log" $all
	native "$w/n2.log" GEX_PROVIDER_SKIP=_Thrd_current; verdict "native, provider skipped" "$w/n2.log" 0
fi
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; echo "$pass passed, $fail failed"; exit $((fail > 0)); }
done
page "$w/p0.log"
pbase=$(sed -n 's/^msvcp120 at \(0x\)\?\([0-9a-fA-F]*\).*/0x\2/p' "$w/p0.log" | head -1)
if [ -n "$pbase" ] && [ "$pbase" != "$nbase" ]; then
	build "$pbase"; build "$pbase" GEX_PROVIDER_SKIP=_Thrd_current
	page "$w/p0.log"
fi
verdict "page, provider" "$w/p0.log" $all
page "$w/p1.log" GEX_PROVIDER_SKIP=_Thrd_current; verdict "page, provider skipped" "$w/p1.log" 0
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
