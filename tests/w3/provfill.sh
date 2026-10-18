#!/bin/bash
# A new thread's provider stack in a split W3 page process (?w3split=1)
# whose guest address space is full (provfill.c): the space filled, given
# back only until Wine can make a thread, and the thread's Worker must still
# get its 1 MiB provider stack, from the shim's heap above 2 GiB (cgen.rs
# gex_provider_stack), not the full guest space ("no memory for a provider
# stack"). Prints "ok   provfill page split".
#
#   tests/w3/provfill.sh DIST PREFIX
#
# DIST is a W3 Wine install, PREFIX a Wayland Wine prefix made for it (copied,
# never written). Needs i686-w64-mingw32-gcc and web/public/gex's x86
# shared-memory shim and seed (run-node.mjs).
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip provfill: no web/public/gex/$f"; exit 0; }
done

w=${WORK:-$root/target/w3-provfill}
rm -rf "$w"; mkdir -p "$w/t"
i686-w64-mingw32-gcc -O1 -Wall -o "$w/provfill.exe" "$here/provfill.c" || exit 1
cp -a "$prefix" "$w/pfx"
(cd "$root" && GEX_W3_SPLIT=1 GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 "${TMO:-300}" \
	node --max-old-space-size=8192 shims/browser/js/run-node.mjs --dir /w3="$dist" --dir /pfx="$w/pfx" \
	--dir /t="$w/t" --file "/t/provfill.exe=$w/provfill.exe" --env WINEPREFIX=/pfx \
	--env WINEDEBUG=err+all,fixme-all,-virtual --cwd /t --argv0 /w3/lib/wine/i386-unix/wine \
	web/public/gex/seed.x86.shared.wasm "$dist/lib/wine/i386-unix/wine" /t/provfill.exe) > "$w/page.log" 2>&1
grep -aE '^(ok  |FAIL)' "$w/page.log" | sed 's/^/  /'
if grep -aq '^provfill ok' "$w/page.log"; then echo "ok   provfill page split"; exit 0; fi
echo "FAIL provfill page split"; grep -av '^\(ok  \|FAIL\)' "$w/page.log" | grep -av 'err:\|fixme:' | tail -8 | sed 's/^/  /'
exit 1
