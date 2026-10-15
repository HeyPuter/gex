#!/bin/bash
# An audio device for a W3 program (audio.c): winmm, mmdevapi, wineoss.drv
# (a pure Unix library linked into W3's loader) and gex-kernel's /dev/dsp,
# a null output. The page's shim had no /dev/dsp, so mmdevapi found the
# driver Unavailable and the program no device. Prints "audio ok N".
# Natively (gexc run) and in the page's runtime under Node (run-node.mjs).
#
#   tests/w3/audio.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wine prefix made for it (copied, never
# written). Needs i686-w64-mingw32-gcc, the x86 shared-memory shim and seed
# in web/public/gex and, for the native legs, an x86 gexc.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
w=${WORK:-$root/target/w3-audio}
rm -rf "$w"; mkdir -p "$w"
i686-w64-mingw32-gcc -O1 -o "$w/audio.exe" "$here/audio.c" -lwinmm || exit 1
want="audio ok [1-9]"
pass=0 fail=0
verdict() { # NAME LOG
	if grep -q "^$want" "$2"; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1"; grep -v '^\[host\]\|gex-console-error' "$2" | tail -8 | sed 's/^/  /'; fi
}
for mode in fast; do
	extra=()
	if [ -x "$gexc" ]; then
		rm -rf "$w/pfx-native"; cp -a "$prefix" "$w/pfx-native"; cp "$w/audio.exe" "$w/pfx-native/drive_c/"
		(cd "$w/pfx-native/drive_c" && timeout -k 10 "${TMO:-300}" "$gexc" run \
			--env WINEPREFIX="$w/pfx-native" --env WINEDEBUG=err+all,fixme-all "${extra[@]}" \
			"$dist/lib/wine/i386-unix/wine" 'C:\audio.exe') > "$w/native-$mode.log" 2>&1
		verdict "native $mode" "$w/native-$mode.log"
	else
		echo "skip native: no $gexc"
	fi
	for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
		[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; continue 2; }
	done
	rm -rf "$w/pfx-page" "$w/t"; cp -a "$prefix" "$w/pfx-page"; mkdir -p "$w/t"
	(cd "$root" && GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 "${TMO:-300}" node --max-old-space-size=8192 \
		shims/browser/js/run-node.mjs --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
		--file "/t/audio.exe=$w/audio.exe" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all "${extra[@]}" \
		--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
		"$dist/lib/wine/i386-unix/wine" /t/audio.exe) > "$w/page-$mode.log" 2>&1
	verdict "page $mode" "$w/page-$mode.log"
done
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
