#!/bin/bash
# W3's i386 builtins at their fixed bases (tools/wine-wasm/rebase.py,
# w3-bases.txt; docs/wine-overhead.md §7): the dist passes `rebase.py
# check` (every DLL in the table, no overlaps, each at its table base with
# DYNAMIC_BASE cleared), and bases.exe, loading DLLs that Noita's process
# and its helpers load, finds each at its table base, natively (gexc run)
# and in the page's runtime under Node (run-node.mjs).
#
#   tests/w3/bases.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wine prefix made for it (copied, never
# written). Needs i686-w64-mingw32-gcc, the x86 shared-memory shim and seed
# in web/public/gex and, for the native leg, an x86 gexc.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
table=$root/tools/wine-wasm/w3-bases.txt
dlls="msvcp120.dll gdiplus.dll mmdevapi.dll shell32.dll ole32.dll comctl32.dll dinput8.dll xinput1_4.dll ws2_32.dll"
w=${WORK:-$root/target/w3-bases}
rm -rf "$w"; mkdir -p "$w"
pass=0 fail=0
if python3 "$root/tools/wine-wasm/rebase.py" check "$dist/lib/wine/i386-windows" "$table" > "$w/check.log" 2>&1; then
	pass=$((pass + 1)); echo "ok   rebase.py check"
else
	fail=$((fail + 1)); echo "FAIL rebase.py check"; head -8 "$w/check.log" | sed 's/^/  /'
fi
i686-w64-mingw32-gcc -O1 -o "$w/bases.exe" "$here/bases.c" || exit 1
want=$(for d in $dlls; do printf 'base %s %08x\n' $d $((16#$(awk -v n=$d 'tolower($3)==n{print substr($1,3)}' "$table"))); done)
verdict() { # NAME LOG
	if [ "$(grep '^base ' "$2" | tr -d '\r' | tr 'A-F' 'a-f')" = "$want" ]; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1"; diff <(echo "$want") <(grep '^base ' "$2" | tr -d '\r' | tr 'A-F' 'a-f') | head -8 | sed 's/^/  /'; fi
}
if [ -x "$gexc" ]; then
	cp -a "$prefix" "$w/pfx-native"; cp "$w/bases.exe" "$w/pfx-native/drive_c/"
	(cd "$w/pfx-native/drive_c" && timeout -k 10 "${TMO:-300}" "$gexc" run \
		--env WINEPREFIX="$w/pfx-native" --env WINEDEBUG=err+all,fixme-all \
		"$dist/lib/wine/i386-unix/wine" 'C:\bases.exe' $dlls) > "$w/native.log" 2>&1
	verdict native "$w/native.log"
else
	echo "skip native: no $gexc"
fi
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; echo "$pass passed, $fail failed"; exit $((fail > 0)); }
done
cp -a "$prefix" "$w/pfx-page"; mkdir -p "$w/t"
(cd "$root" && GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 "${TMO:-300}" node --max-old-space-size=8192 \
	shims/browser/js/run-node.mjs --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
	--file "/t/bases.exe=$w/bases.exe" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
	--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
	"$dist/lib/wine/i386-unix/wine" /t/bases.exe $dlls) > "$w/page.log" 2>&1
verdict page "$w/page.log"
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
