#!/bin/bash
# .NET on W3 (dotnet.cs): the dist carries Wine Mono (w3.pins' WINE_MONO_*,
# tools/wine-wasm/w3/mono.sh), which mscoree finds through WINEDATADIR. The
# program is compiled by Wine Mono's own C# compiler (mcs.exe, itself .NET,
# run natively on W3), then run natively and in the page's runtime under
# Node (run-node.mjs). Prints "dotnet ok".
#
#   tests/w3/dotnet.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install with Wine Mono, PREFIX a Wine prefix made for it
# (copied, never written). Needs an x86 gexc (mcs runs natively) and, for the
# page leg, the x86 shared-memory shim and seed in web/public/gex.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
mono=$(ls -d "$dist"/share/wine/mono/wine-mono-* 2>/dev/null | head -1)
[ -n "$mono" ] || { echo "FAIL no Wine Mono in $dist/share/wine/mono (W3_MONO=0 dist?)"; exit 1; }
[ -x "$gexc" ] || { echo "FAIL no $gexc (mcs runs natively)"; exit 1; }
winpath() { printf 'Z:%s' "$1" | tr / '\\'; }

w=${WORK:-$root/target/w3-dotnet}
rm -rf "$w"; mkdir -p "$w"
pass=0 fail=0
verdict() { # NAME LOG WANT
	if grep -q "^$3" "$2"; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1"; grep -v '^\[host\]\|gex-console-error' "$2" | tail -8 | sed 's/^/  /'; fi
}
rm -rf "$w/pfx-native"; cp -a "$prefix" "$w/pfx-native"; cp "$here/dotnet.cs" "$w/pfx-native/drive_c/"
run() { # LOG ARGS...: wine under gexc in the native prefix's C:\
	local log=$1; shift
	(cd "$w/pfx-native/drive_c" && timeout -k 10 "${TMO:-900}" "$gexc" run \
		--env WINEPREFIX="$w/pfx-native" --env WINEDEBUG=err+all,fixme-all \
		"$dist/lib/wine/i386-unix/wine" "$@") > "$log" 2>&1
}
run "$w/mcs.log" "$(winpath "$mono/lib/mono/4.5/mcs.exe")" -out:'C:\dotnet.exe' 'C:\dotnet.cs'
if [ -f "$w/pfx-native/drive_c/dotnet.exe" ]; then pass=$((pass + 1)); echo "ok   mcs compiles dotnet.cs"
else fail=$((fail + 1)); echo "FAIL mcs"; tail -8 "$w/mcs.log" | sed 's/^/  /'; echo "$pass passed, $fail failed"; exit 1; fi
run "$w/native.log" 'C:\dotnet.exe'
verdict native "$w/native.log" "dotnet ok"
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; echo "$pass passed, $fail failed"; [ "$fail" = 0 ]; exit; }
done
rm -rf "$w/pfx-page" "$w/t"; cp -a "$prefix" "$w/pfx-page"; mkdir -p "$w/t"
(cd "$root" && GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 "${TMO:-900}" node --max-old-space-size=8192 \
	shims/browser/js/run-node.mjs --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
	--file "/t/dotnet.exe=$w/pfx-native/drive_c/dotnet.exe" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
	--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
	"$dist/lib/wine/i386-unix/wine" /t/dotnet.exe) > "$w/page.log" 2>&1
verdict page "$w/page.log" "dotnet ok"
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
