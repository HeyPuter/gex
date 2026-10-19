#!/bin/sh
# web/build-eject-page.sh carries everything web/public/gex has that the page
# loads: the x86 shared shim, kernel and seed, and the X server (gex/xwayland/,
# and gex/xwayland-st/ when built). Without the X server a site's X11 program
# stopped at "x11 not available" (stream G's linux-x11 game sites, 2026-10-18).
# Prints "ok   eject page files" or FAIL lines.
#
#   tests/eject/page-files.sh
set -u
root=$(cd "$(dirname "$0")/../.." && pwd)
pub=$root/web/public/gex
[ -f "$pub/gex_browser.x86.shared.wasm" ] || { echo "skip eject page files: no web/public/gex x86 shim"; exit 0; }
[ -x "$root/web/node_modules/.bin/vite" ] || { echo "skip eject page files: no web/node_modules (npm ci in web/)"; exit 0; }
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
sh "$root/web/build-eject-page.sh" "$out/page" > "$out/log" 2>&1 || { echo "FAIL eject page files: build-eject-page.sh"; tail -5 "$out/log"; exit 1; }
fail=0
for f in app.mjs gex/gex_browser.x86.shared.wasm gex/gex_kernel.x86.wasm gex/seed.x86.shared.wasm; do
	[ -f "$out/page/$f" ] || { echo "FAIL eject page files: no $f"; fail=1; }
done
for d in xwayland xwayland-st; do
	[ -d "$pub/$d" ] || continue
	for f in "$pub/$d"/*; do
		cmp -s "$f" "$out/page/gex/$d/${f##*/}" || { echo "FAIL eject page files: gex/$d/${f##*/} missing or stale"; fail=1; }
	done
done
[ -d "$pub/xwayland" ] || echo "  (no web/public/gex/xwayland here: the X server's part not checked)"
[ $fail = 0 ] && echo "ok   eject page files"
exit $fail
