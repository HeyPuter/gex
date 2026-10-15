#!/bin/bash
# Code at addresses the NT layer's VirtualFree just gave back, from several
# threads (vmreuse.c): every call runs the code just written there, never
# the old page's translation; prints "vmreuse ok N".
# Natively (gexc run) and in the page's runtime under Node (run-node.mjs).
#
#   tests/w3/vmreuse.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install (lib/wine/i386-unix/wine), PREFIX a Wine prefix
# made for it (copied, never written); PAGE_PREFIX, if set, the page's
# (one whose display driver works without a compositor). Needs i686-w64-mingw32-gcc, the x86
# shared-memory shim and seed in web/public/gex (GEX_ARCH=x86
# shims/browser/build.sh) and, for the native leg, an x86 gexc.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")

w=${WORK:-$root/target/w3-vmreuse}
rm -rf "$w"; mkdir -p "$w"
i686-w64-mingw32-gcc -O1 -o "$w/vmreuse.exe" "$here/vmreuse.c" || exit 1
want="vmreuse ok"
pass=0 fail=0
verdict() { # NAME LOG
	if grep -q "^$want" "$2"; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1"; grep -v '^\[host\]\|gex-console-error' "$2" | tail -8 | sed 's/^/  /'; fi
}
if [ -x "$gexc" ]; then
	cp -a "$prefix" "$w/pfx-native"
	cp "$w/vmreuse.exe" "$w/pfx-native/drive_c/"
	(cd "$w/pfx-native/drive_c" && timeout -k 10 "${TMO:-300}" "$gexc" run \
		--env WINEPREFIX="$w/pfx-native" --env WINEDEBUG=err+all,fixme-all \
		--wayland "$w/wl" --wayland-size 640x480 --wayland-out null \
		"$dist/lib/wine/i386-unix/wine" 'C:\vmreuse.exe') > "$w/native.log" 2>&1
	verdict native "$w/native.log"
else
	echo "skip native: no $gexc"
fi
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; exit $((fail > 0)); }
done
cp -a "${PAGE_PREFIX:-$prefix}" "$w/pfx-page"
mkdir -p "$w/t"
(cd "$root" && GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 "${TMO:-300}" node --max-old-space-size=8192 \
	shims/browser/js/run-node.mjs --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
	--file "/t/vmreuse.exe=$w/vmreuse.exe" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
	--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
	"$dist/lib/wine/i386-unix/wine" /t/vmreuse.exe) > "$w/page.log" 2>&1
verdict page "$w/page.log"
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
