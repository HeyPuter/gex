#!/bin/bash
# A Wayland client with a waiting thread spawns processes (spawnwin.c): its
# windows must keep working after each spawn, and it prints "spawnwin ok N".
# Natively (gexc run, its own compositor) and in the page's runtime under
# Node (run-node.mjs with the shim's compositor, GEX_WL_STUB), where a
# Windows process's child runs in place in the cloning thread's Worker.
#
#   tests/w3/spawnwin.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install (lib/wine/i386-unix/wine), PREFIX a Wine prefix
# made for it (copied, never written; its graphics driver is set to wayland
# in the copy). Needs i686-w64-mingw32-gcc, the x86 shared-memory shim and
# seed in web/public/gex (GEX_ARCH=x86 shims/browser/build.sh) and, for the
# native leg, an x86 gexc.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
n=${N:-4}
w=${WORK:-$root/target/w3-spawnwin}
[ -d "$w" ] && chmod -R u+w "$w"
rm -rf "$w"; mkdir -p "$w"
i686-w64-mingw32-gcc -O1 -o "$w/spawnwin.exe" "$here/spawnwin.c" -mwindows -Wl,--subsystem,console || exit 1
want="spawnwin ok $n"
pass=0 fail=0
verdict() { # NAME LOG
	if grep -q "^$want" "$2"; then pass=$((pass + 1)); echo "ok   $1"
	else fail=$((fail + 1)); echo "FAIL $1"; grep -v '^\[host\]\|gex-console-error' "$2" | tail -8 | sed 's/^/  /'; fi
}
# PREFIX's copy NAME, drawing through winewayland.drv.
pfx() {
	cp -a "$prefix" "$w/$1"
	chmod -R u+w "$w/$1"
	sed -i 's/^"Graphics"=.*/"Graphics"="wayland"/' "$w/$1/user.reg"
	grep -q '^"Graphics"="wayland"' "$w/$1/user.reg" || { echo "no Graphics key in $prefix/user.reg"; exit 1; }
}
if [ -x "$gexc" ]; then
	pfx pfx-native
	cp "$w/spawnwin.exe" "$w/pfx-native/drive_c/"
	mkdir -p "$w/xdg"
	(cd "$w/pfx-native/drive_c" && timeout -k 10 "${TMO:-300}" "$gexc" run \
		--wayland "$w/xdg/wayland-0" --wayland-out null \
		--env WINEPREFIX="$w/pfx-native" --env WINEDEBUG=err+all,fixme-all \
		--env XDG_RUNTIME_DIR="$w/xdg" --env WAYLAND_DISPLAY=wayland-0 \
		"$dist/lib/wine/i386-unix/wine" 'C:\spawnwin.exe' "$n") > "$w/native.log" 2>&1
	verdict native "$w/native.log"
else
	echo "skip native: no $gexc"
fi
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; exit $((fail > 0)); }
done
pfx pfx-page
mkdir -p "$w/t"
(cd "$root" && GEX_ARCH=x86 GEX_SHARED=1 GEX_WL_STUB=1 timeout -k 10 "${TMO:-300}" node --max-old-space-size=8192 \
	shims/browser/js/run-node.mjs --dir /w3="$dist" --dir /pfx="$w/pfx-page" --dir /t="$w/t" \
	--file "/t/spawnwin.exe=$w/spawnwin.exe" --env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all \
	--env XDG_RUNTIME_DIR=/tmp --env WAYLAND_DISPLAY=wayland-0 \
	--cwd /t --argv0 /w3/lib/wine/i386-unix/wine web/public/gex/seed.x86.shared.wasm \
	"$dist/lib/wine/i386-unix/wine" /t/spawnwin.exe "$n") > "$w/page.log" 2>&1
verdict page "$w/page.log"
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
