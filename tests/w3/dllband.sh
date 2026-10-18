#!/bin/bash
# The DLLs a W3 bundle pins load in the band (dllband.c): the prefix's
# dll-bases from tools/wine-wasm/w3/dll-bases.py, which must pass its own
# check, and four of the dist's DLLs loaded at the addresses it gives them,
# in [0x11000000, 0x20000000), natively and in the page, there both in the
# compact space and with ?w3split=1. Prints "ok   dllband <leg>" per leg.
#
#   tests/w3/dllband.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wayland Wine prefix made for it (copied,
# never written). Needs i686-w64-mingw32-gcc and an x86 gexc; the run uses
# gexc's own Wayland compositor (--wayland, frames dropped). The page legs need
# web/public/gex's x86 shared-memory shim and seed (run-node.mjs).
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")

w=${WORK:-$root/target/w3-dllband}
rm -rf "$w"; mkdir -p "$w/wl"
i686-w64-mingw32-gcc -O1 -Wall -o "$w/dllband.exe" "$here/dllband.c" || exit 1
cp -a "$prefix" "$w/pfx"; cp "$w/dllband.exe" "$w/pfx/drive_c/"
# The server names an image by the unix path it opened: natively the dist's.
for d in version winmm dbghelp opengl32; do echo "$dist/lib/wine/i386-windows/$d.dll"; done |
	python3 "$root/tools/wine-wasm/w3/dll-bases.py" assign "$w/pfx/dll-bases" > "$w/assign.log" 2>&1 ||
	{ echo "FAIL dllband: dll-bases.py assign"; sed 's/^/  /' "$w/assign.log"; exit 1; }
python3 "$root/tools/wine-wasm/w3/dll-bases.py" check "$w/pfx/dll-bases" ||
	{ echo "FAIL dllband: dll-bases.py check"; exit 1; }
fail=0
verdict() { # LEG LOG
	grep -aE '^(ok  |FAIL)' "$2" | sed "s/^/  $1: /"
	if grep -aq '^dllband ok' "$2"; then echo "ok   dllband $1"; else
		echo "FAIL dllband $1"; grep -av '^\(ok  \|FAIL\)' "$2" | tail -8 | sed 's/^/  /'; fail=$((fail + 1)); fi
}
if [ -x "$gexc" ]; then
	(cd "$w/pfx/drive_c" && timeout -k 10 "${TMO:-200}" "$gexc" run --no-cgen \
		--wayland "$w/wl" --wayland-size 640x480 --wayland-out null \
		--env WINEPREFIX="$w/pfx" --env WINEDEBUG=err+all,fixme-all \
		"$dist/lib/wine/i386-unix/wine" ./dllband.exe 'Z:'"$w/pfx/dll-bases") > "$w/native.log" 2>&1
	verdict native "$w/native.log"
else
	echo "skip native: no $gexc"
fi

# The page (run-node.mjs), where the band is what keeps the pins clear of the
# shim's heap: once in W3's compact space, and once with ?w3split=1
# (GEX_W3_SPLIT, nt::w3_space), which grows the memory to 2 GiB at start and
# puts the heap above it. The server names images by their page paths here.
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$root/web/public/gex/$f" ] || { echo "skip page: no web/public/gex/$f"; exit $((fail > 0)); }
done
for leg in compact split; do
	rm -rf "$w/pfx-$leg"; cp -a "$prefix" "$w/pfx-$leg"; mkdir -p "$w/t"
	for d in version winmm dbghelp opengl32; do echo "/w3/lib/wine/i386-windows/$d.dll"; done |
		python3 "$root/tools/wine-wasm/w3/dll-bases.py" assign "$w/pfx-$leg/dll-bases" --map /w3="$dist" >> "$w/assign.log" 2>&1
	split=; [ $leg = split ] && split=1
	(cd "$root" && GEX_W3_SPLIT=$split GEX_DEBUG_EXIT=1 GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 "${TMO:-300}" \
		node --max-old-space-size=8192 shims/browser/js/run-node.mjs --dir /w3="$dist" --dir /pfx="$w/pfx-$leg" \
		--dir /t="$w/t" --file "/t/dllband.exe=$w/dllband.exe" --env WINEPREFIX=/pfx \
		--env WINEDEBUG=err+all,fixme-all --cwd /t --argv0 /w3/lib/wine/i386-unix/wine \
		web/public/gex/seed.x86.shared.wasm "$dist/lib/wine/i386-unix/wine" /t/dllband.exe Z:/pfx/dll-bases) > "$w/page-$leg.log" 2>&1
	verdict "page $leg" "$w/page-$leg.log"
	# The program's process: its memory at exit, 2 GiB or more only when split.
	mib=$(grep -a '^\[gex\] memory [0-9]* MiB' "$w/page-$leg.log" | tail -1 | tr -dc 0-9)
	if [ -z "$mib" ]; then echo "FAIL dllband page $leg: no memory line"; fail=$((fail + 1))
	elif [ $leg = split ] && [ "$mib" -lt 2048 ]; then echo "FAIL dllband page split: memory $mib MiB, under 2 GiB"; fail=$((fail + 1))
	elif [ $leg = compact ] && [ "$mib" -ge 2048 ]; then echo "FAIL dllband page compact: memory $mib MiB, grown to 2 GiB"; fail=$((fail + 1))
	else echo "ok   dllband page $leg: memory $mib MiB"; fi
done
[ "$fail" = 0 ]
