#!/bin/bash
# The DLLs a W64 prefix pins load at their pins in every process
# (dllband.c): the prefix's dll-bases from tools/wine-wasm/w64/dll-bases.py,
# which must pass its own check, and four of the dist's DLLs loaded at the
# addresses it gives them, in [0x11000000, 0x20000000), in the program and in
# a second process it starts, natively and in the page (run-node.mjs), and a
# program's own DLL in a directory whose name has a space (C:\pin dir), its
# pin a unix path with a space in it, and a fixed-base one there (no dynamic
# base) whose own base the program holds, relocated to its pin.
# Also that PREFIX carries W3's session (w64-dist.sh applies w3-session.reg)
# and that no driver fails to load in either leg.
# Prints "ok   dllband64 <leg>" per leg.
#
#   tests/wine64/dllband.sh DIST PREFIX [gexc]
#
# DIST is a W64 install whose server reads dll-bases (Wine branch
# g3/wine64-dll-bases: server/mapping.c, pinned_map_address), PREFIX a win64
# prefix made for it (copied, never written). Needs x86_64-w64-mingw32-gcc
# and an x86 gexc. The page leg needs web/public/gex's x86 shared-memory
# shim and seed (PUB=DIR: another directory holding them).
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
tool=$root/tools/wine-wasm/w64/dll-bases.py
w=${WORK:-$root/target/w64-dllband}
rm -rf "$w"; mkdir -p "$w"
x86_64-w64-mingw32-gcc -O1 -Wall -o "$w/dllband.exe" "$here/dllband.c" || exit 1
# (relocatable, as a DLL that can be pinned must be)
echo 'int pinme(void) { return 7; }' | x86_64-w64-mingw32-gcc -O1 -shared -Wl,--dynamicbase -o "$w/pinme.dll" -x c - || exit 1
# A fixed-base DLL (no dynamic base, relocations kept) whose own base,
# 0x30000000, the program holds before loading it (dllband.c FIXED_BASE)
echo 'int fixedbase(void) { return 9; }' | x86_64-w64-mingw32-gcc -O1 -shared \
	-Wl,--disable-dynamicbase,--enable-reloc-section,--image-base,0x30000000 -o "$w/fixed.dll" -x c - || exit 1
dlls="version winmm dbghelp opengl32"
fail=0
verdict() { # LEG LOG
	grep -aE '^(ok  |FAIL)' "$2" | tr -d '\r' | sed "s/^/  $1: /"
	if grep -aq '^dllband ok' "$2"; then echo "ok   dllband64 $1"; else
		echo "FAIL dllband64 $1"; grep -av '^\(ok  \|FAIL\)' "$2" | tail -8 | sed 's/^/  /'; fail=$((fail + 1)); fi
}
# assign PFX PREFIX_OF_PATHS PFX_AS_NAMED [--map ...]: the prefix's dll-bases,
# checked; PFX_AS_NAMED is the prefix as the server names its files (it
# resolves links: the drive, not dosdevices/c:)
assign() {
	local p=$1 at=$2 named=$3; shift 3
	mkdir -p "$p/drive_c/pin dir" && cp "$w/pinme.dll" "$w/fixed.dll" "$p/drive_c/pin dir/"
	{ for d in $dlls; do echo "$at/lib/wine/x86_64-windows/$d.dll"; done
	  echo "$named/drive_c/pin dir/pinme.dll"; echo "$named/drive_c/pin dir/fixed.dll"; } |
		python3 "$tool" assign "$p/dll-bases" "$@" >> "$w/assign.log" 2>&1 &&
		python3 "$tool" check "$p/dll-bases" ||
		{ echo "FAIL dllband64: dll-bases.py"; sed 's/^/  /' "$w/assign.log"; exit 1; }
}
# The prefix carries W3's session (w3-session.reg, w64-dist.sh's pfs64):
# no boot-start MountMgr or root bus devices whose drivers fail to load
# (each a winedevice process before the program starts), Event Log on demand.
session() { # PFX: the session's keys, as reg-apply.py writes them
	python3 - "$1/system.reg" <<'PY'
import re, sys
t = open(sys.argv[1], encoding='utf-8').read()
def start(svc):
    m = re.search(r'^\[System\\\\ControlSet001\\\\Services\\\\' + svc + r'\] .*?\n\n', t + '\n\n', re.M | re.S | re.I)
    s = m and re.search(r'^"Start"=dword:([0-9a-f]{8})$', m.group(0), re.M)
    return s and int(s.group(1), 16)
bad = [f'{s} Start={start(s)}' for s in ('MountMgr', 'Eventlog') if start(s) != 3]
bad += [d for d in ('WINEBUS', 'WINEUSB', 'WINEBTH') if re.search(r'^\[System\\\\ControlSet001\\\\Enum\\\\ROOT\\\\WINE\\\\' + d + r'\]', t, re.M | re.I)]
sys.exit('; '.join(bad) if bad else 0)
PY
}
if err=$(session "$prefix" 2>&1); then echo "ok   dllband64 session"; else
	echo "FAIL dllband64 session: $prefix is not W3's session ($err): tools/wine-wasm/w64-dist.sh's W64_PREFIX"; fail=$((fail + 1)); fi
drivers() { # LEG LOG: no driver failed to load (the session left none that would)
	if grep -aq 'err:ntoskrnl:ZwLoadDriver' "$2"; then
		echo "FAIL dllband64 $1 drivers"; grep -a 'err:ntoskrnl:ZwLoadDriver' "$2" | head -4 | sed 's/^/  /'; fail=$((fail + 1)); fi
}
# Natively the server names an image by the dist's own path.
if [ -x "$gexc" ]; then
	rm -rf "$w/pfx"; cp -a "$prefix" "$w/pfx"; cp "$w/dllband.exe" "$w/pfx/drive_c/"
	assign "$w/pfx" "$dist" "$(realpath "$w/pfx")"
	(cd "$w/pfx/drive_c" && timeout -k 10 "${TMO:-300}" "$gexc" run --no-cgen \
		--env WINEPREFIX="$w/pfx" --env WINEDEBUG=err+all,fixme-all --env WINEGEX_DOOR=0 \
		"$dist/lib/wine/x86_64-unix/wine" 'C:\dllband.exe' 'Z:'"$w/pfx/dll-bases") > "$w/native.log" 2>&1
	verdict native "$w/native.log"; drivers native "$w/native.log"
else
	echo "skip native: no $gexc"
fi
# The page: the server names images by their page paths.
pub=$(realpath -m "${PUB:-$root/web/public/gex}")
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
	[ -f "$pub/$f" ] || { echo "skip page: no $pub/$f"; exit $((fail > 0)); }
done
rm -rf "$w/pfx-page"; cp -a "$prefix" "$w/pfx-page"; cp "$w/dllband.exe" "$w/pfx-page/drive_c/"
assign "$w/pfx-page" /w64 /pfx --map /w64="$dist" --map /pfx="$w/pfx-page"
(cd "$root" && GEX_WEB_DIR="$pub" GEX_ARCH=x86 GEX_SHARED=1 timeout -k 10 "${TMO:-300}" \
	node --max-old-space-size=8192 shims/browser/js/run-node.mjs --dir /w64="$dist" --dir /pfx="$w/pfx-page" \
	--env WINEPREFIX=/pfx --env WINEDEBUG=err+all,fixme-all --env WINEGEX_DOOR=0 \
	--cwd /pfx/drive_c --argv0 /w64/lib/wine/x86_64-unix/wine \
	"$pub/seed.x86.shared.wasm" "$dist/lib/wine/x86_64-unix/wine" 'C:\dllband.exe' Z:/pfx/dll-bases) > "$w/page.log" 2>&1
verdict page "$w/page.log"; drivers page "$w/page.log"
[ "$fail" = 0 ]
