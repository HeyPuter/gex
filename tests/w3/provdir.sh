#!/bin/bash
# Providers from outside gex on wine32 (provdir/): an export provider from a
# directory (GEX_PROVIDER_DIRS: abs_sentinel.c answers msvcr120's abs with
# 4242) and a stateful provider replacing a program's own DLL (sdemo.dll's
# sd_add answers a + b + 1000, one more each call; natively GEX_WASM_DLLS,
# in the page GEX_STATEFUL, a module under the unit cache). Without them the
# library's own code answers (abs 7, sd_add 5). Natively (gexc run) and in
# the page's runtime under Node (run-node.mjs).
#
#   tests/w3/provdir.sh DIST PREFIX [gexc]
#
# DIST is a wine32 Wine install, PREFIX a Wine prefix made for it (copied,
# never written). Needs i686-w64-mingw32-gcc, clang and wasm-ld for wasm32,
# an x86 gexc, and for the page legs the x86 shared-memory shim and seed in
# web/public/gex.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "FAIL: no $gexc (the units are built with it)"; exit 1; }
w=${WORK:-$root/target/w3-provdir}
rm -rf "$w"; mkdir -p "$w/units/stateful" "$w/gen"
export GEX_PROVIDER_DIRS=$here/provdir/providers
i686-w64-mingw32-gcc -O1 -o "$w/provdir.exe" "$here/provdir/provdir.c" || exit 1
i686-w64-mingw32-gcc -O1 -shared -o "$w/sdemo.dll" "$here/provdir/sdemo.c" || exit 1
"$gexc" cgen-c --header > "$w/gen/gex_abi.h" || exit 1
cp "$root/crates/gex-cgen/providers/provider.h" "$w/gen/"
clang --target=wasm32 -O2 -fPIC -fvisibility=hidden -matomics -mbulk-memory -I"$w/gen" -c "$here/provdir/sdemo_prov.c" -o "$w/sdemo_prov.o" &&
	wasm-ld --experimental-pic -shared --no-entry --import-memory --shared-memory --max-memory=4294901760 --allow-undefined \
		-o "$w/units/stateful/sdemo.wasm" "$w/sdemo_prov.o" || { echo "FAIL: building sdemo's provider"; exit 1; }
GEX_CGEN_CACHE="$w/units" "$gexc" cgen-image --providers > "$w/providers.log" 2>&1 ||
	{ echo "FAIL: provider modules (tail: $(tail -2 "$w/providers.log" | tr '\n' ' '))"; exit 1; }
pass=0 fail=0
native() { # LOG [env...]
	local log=$1; shift
	rm -rf "$w/pfx-native"; cp -a "$prefix" "$w/pfx-native"; cp "$w/provdir.exe" "$w/sdemo.dll" "$w/pfx-native/drive_c/"
	(cd "$w/pfx-native/drive_c" && env GEX_CGEN_CACHE="$w/units" "$@" timeout -k 10 "${TMO:-300}" "$gexc" run \
		--env WINEPREFIX="$w/pfx-native" --env WINEDEBUG=err+all,fixme-all \
		"$dist/lib/wine/i386-unix/wine" 'C:\provdir.exe') > "$log" 2>&1
	sed -i 's/\r$//' "$log"
}
page() { # LOG [env...]
	local log=$1; shift
	rm -rf "$w/pfx-page" "$w/t"; cp -a "$prefix" "$w/pfx-page"; mkdir -p "$w/t"
	(cd "$root" && env GEX_ARCH=x86 GEX_SHARED=1 GEX_CGEN_CACHE="$w/units" "$@" timeout -k 10 "${TMO:-300}" node --max-old-space-size=8192 \
		shims/browser/js/run-node.mjs --cgen-prefer --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
		--file "/t/provdir.exe=$w/provdir.exe" --file "/t/sdemo.dll=$w/sdemo.dll" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
		--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
		"$dist/lib/wine/i386-unix/wine" /t/provdir.exe) > "$log" 2>&1
	sed -i 's/\r$//' "$log"
}
verdict() { # NAME LOG ABS ADD1 ADD2
	local got; got=$(grep -E '^(abs|sd_add) ' "$2" | tr '\n' ' ')
	if grep -q '^provdir done' "$2" && [ "$got" = "abs $3 sd_add $4 sd_add $5 " ]; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1 (want abs $3, sd_add $4 then $5; got '$got')"; grep -v '^\[host\]\|gex-console-error' "$2" | tail -6 | sed 's/^/  /'; fi
}
# With and without W3's KUSER fold: which key a run looks for depends on
# whether the dist's Wine folds KUSER_SHARED_DATA.
build() { # BASE
	(for k in --w3-kuser ""; do
		GEX_CGEN_CACHE="$w/units" GEX_CGEN_PRECOMPILE=1 "$gexc" cgen-image "$dist/lib/wine/i386-windows/msvcr120.dll" --pe-base "$1" $k --jobs 4 || exit 1
	done) > "$w/build.log" 2>&1 ||
		{ echo "FAIL: msvcr120 unit at $1 (tail: $(tail -2 "$w/build.log" | tr '\n' ' '))"; exit 1; }
}
base() { sed -n 's/^msvcr120 at \(0x\)\?\([0-9a-fA-F]*\).*/0x\2/p' "$1" | head -1; }

GEX_PROVIDER_DIRS= native "$w/n0.log"
verdict "native, no providers" "$w/n0.log" 7 5 5
nbase=$(base "$w/n0.log")
[ -n "$nbase" ] && build "$nbase"
native "$w/n1.log" GEX_WASM_DLLS="sdemo.dll=$w/units/stateful/sdemo.wasm"
verdict "native, providers from outside gex" "$w/n1.log" 4242 1005 1006
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; echo "$pass passed, $fail failed"; exit $((fail > 0)); }
done
page "$w/p1.log" GEX_STATEFUL=sdemo.dll=stateful/sdemo.wasm
pbase=$(base "$w/p1.log")
if [ -n "$pbase" ] && [ "$pbase" != "$nbase" ]; then
	build "$pbase"
	page "$w/p1.log" GEX_STATEFUL=sdemo.dll=stateful/sdemo.wasm
fi
verdict "page, providers from outside gex" "$w/p1.log" 4242 1005 1006
GEX_PROVIDER_DIRS= page "$w/p0.log"
verdict "page, no providers" "$w/p0.log" 7 5 5
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
