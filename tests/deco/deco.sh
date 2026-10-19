#!/bin/bash
# Wine windows under gex's compositor carry no frame of the compositor's
# (deco.c, deco.wl): Wine draws a captioned window's frame itself and a
# WS_POPUP draws its own, so every Wine toplevel must be placed and
# unmanaged (gex_window_placement_v1.set_unmanaged; the page's compositors
# then draw no titlebar or border round it), at its Win32 position, and a
# drag on the popup's own caption (WM_NCHITTEST HTCAPTION) moves it through
# Win32's move loop. Checks, per leg: the compositor's window list
# ([wl-place]) has both windows unmanaged at 100,100 and 500,100 and P at
# 300,250 after the drag; the program says P moved there; the windows'
# buffers (the dump without a GPU) are P's own caption and body and O's
# Wine-drawn caption over its blue client; a popup centred by Win32
# (GetSystemMetrics) is listed centred, and at 20,600 after SetWindowPos.
#
#   tests/deco/deco.sh W3_DIST W3_PREFIX [gexc]
#
# W3_PREFIX is a Wayland prefix for W3_DIST (copied, never written). With
# W64_DIST, W64_PREFIX and W64_NATIVE in the environment (w64-dist.sh's),
# the same as x64 on W64. Needs i686- and x86_64-w64-mingw32-gcc and an
# x86 gexc.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
w3dist=$(realpath "${1:?W3_DIST}"); w3prefix=$(realpath "${2:?W3_PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "skip deco: no $gexc"; exit 0; }
w=${WORK:-$root/target/deco}
rm -rf "$w"; mkdir -p "$w"
i686-w64-mingw32-gcc -O1 -Wall -o "$w/deco32.exe" "$here/deco.c" -lgdi32 || exit 1
x86_64-w64-mingw32-gcc -O1 -Wall -o "$w/deco64.exe" "$here/deco.c" -lgdi32 || exit 1
pass=0 fail=0
leg() { # NAME DIST PREFIX WINE EXE
	local name=$1 dist=$2 p=$w/pfx-$1 log=$w/$1.log extra=()
	cp -a "$3" "$p"; cp "$w/$5" "$p/drive_c/"
	if [ "$name" = w64 ]; then
		printf '\n[Software\\\\Wine\\\\Drivers] 1791289000\n"Graphics"="wayland"\n' >> "$p/user.reg"
		printf '\n[Software\\\\Wine\\\\X11 Driver] 1791289000\n"EmulateModeset"="Y"\n' >> "$p/user.reg"
		[ -e "$p/drive_c/windows/system32/winewayland.drv" ] ||
			cp "$dist/lib/wine/x86_64-windows/winewayland.drv" "$p/drive_c/windows/system32/"
		extra=(--env WINEGEX_DOOR=1 --env "WINEDLLOVERRIDES=mscoree=;mshtml=")
	fi
	mkdir -p "$w/wl-$name" "$w/frames-$name"
	(cd "$p/drive_c" && GEX_PPM_EVERY=1 GEX_WL_INPUT="$here/deco.wl" timeout -k 10 "${TMO:-300}" "$gexc" run \
		--wayland "$w/wl-$name" --wayland-size 1024x768 --wayland-out "$w/frames-$name/f" \
		--env WINEPREFIX="$p" --env WINEDEBUG=err+all,fixme-all "${extra[@]}" \
		"$dist/lib/wine/$4/wine" "C:\\$5" 120) > "$log" 2>&1
	if python3 - "$log" "$w/frames-$name" <<'PY'
import os, re, sys
log = open(sys.argv[1], errors="replace").read()
fails = []
def check(ok, what):
    if not ok: fails.append(what); print("  missing: " + what)
# (anywhere in a line: the program's stdout shares the log, unsynchronised)
lists = [l[l.index("[wl-place]"):] for l in log.splitlines() if "[wl-place]" in l]
wins = set()
for l in lists:
    for w, h, x, y, u in re.findall(r"id=0x[0-9a-f]+ (\d+)x(\d+)\+(-?\d+)\+(-?\d+) layer=\d( unmanaged)?", l):
        wins.add((int(w), int(h), int(x), int(y), bool(u)))
check((300, 200, 100, 100, True) in wins, "P listed unmanaged at 100,100 300x200")
check((300, 200, 500, 100, True) in wins, "O listed unmanaged at 500,100 300x200")
check(not any(not u for *_, u in wins), "no managed (compositor-framed) window")
check("moved P to 300,250" in log, "P moved to 300,250 by its caption")
check(any((300, 200, 300, 250, True) == g for g in wins), "P listed at 300,250 after the drag")
check((200, 100, 412, 334, True) in wins, "C listed centred, at 412,334 (Win32's position, not the compositor's)")
check((200, 100, 20, 600, True) in wins, "C listed at 20,600 after SetWindowPos")
# Without a GPU the compositor's dump is each client buffer as committed: the
# whole window, Wine's frame included, as the page shows it with no frame of
# its own round it.
def ppm(p):
    d = open(p, "rb").read()
    parts = d.split(b"\n", 3)
    w, h = map(int, parts[1].split())
    return w, h, parts[3]
def px(f, x, y):
    w, h, b = f
    o = (y * w + x) * 3
    return tuple(b[o:o + 3])
R, Y, B = (255, 0, 0), (255, 255, 0), (0, 0, 255)
seen_p = seen_o = False
d = sys.argv[2]
for name in sorted(os.listdir(d)):
    if not name.endswith(".ppm"): continue
    f = ppm(f"{d}/{name}")
    if (f[0], f[1]) != (300, 200): continue
    if px(f, 50, 10) == Y and px(f, 50, 150) == R: seen_p = True
    if px(f, 150, 150) == B and px(f, 150, 5) not in (B, Y, R): seen_o = True
check(seen_p, "P's buffer: its own caption (yellow) and body (red), nothing round them")
check(seen_o, "O's buffer: Wine's caption over its blue client")
sys.exit(1 if fails else 0)
PY
	then pass=$((pass + 1)); echo "ok   deco $name"
	else fail=$((fail + 1)); echo "FAIL deco $name"; grep -a '^P \|^O \|^moved\|^deco\|^\[wl-place\]' "$log" | tail -6 | sed 's/^/  | /'; fi
}
leg w3 "$w3dist" "$w3prefix" i386-unix deco32.exe
if [ -n "${W64_DIST:-}" ] && [ -n "${W64_PREFIX:-}" ] && [ -n "${W64_NATIVE:-}" ]; then
	# W64_DIST with W64_NATIVE's files in place (the native PE core, which
	# has winewayland.drv), as tests/games/matrix.sh runs W64 games
	d=$(realpath "$W64_DIST") o=$w/w64dist
	mkdir -p "$o/lib/wine/x86_64-windows"
	cp -a "$d/lib/wine/x86_64-unix" "$o/lib/wine/"
	ln -s "$d/share" "$o/share"; ln -s ../../share "$o/lib/wine/share"
	for f in "$d"/lib/wine/x86_64-windows/*; do ln -s "$f" "$o/lib/wine/x86_64-windows/"; done
	for f in "$(realpath "$W64_NATIVE")"/*; do rm -f "$o/lib/wine/x86_64-windows/${f##*/}"; cp "$f" "$o/lib/wine/x86_64-windows/"; done
	leg w64 "$o" "$(realpath "$W64_PREFIX")" x86_64-unix deco64.exe
else
	echo "skip deco w64: no W64_DIST/W64_PREFIX/W64_NATIVE"
fi
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
