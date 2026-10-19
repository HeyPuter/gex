#!/bin/bash
# Wheel input reaching a Win32 window through gexc's Wayland compositor and
# Wine's winewayland (wheel.c, wheel.wl): a mouse notch is WM_MOUSEWHEEL
# of -120, a trackpad's 40 small deltas add up to another -120
# (none rounded to nothing), and a horizontal notch is WM_MOUSEHWHEEL +120.
# winewayland reads only wl_pointer.axis_discrete / axis_value120, never the
# continuous axis, so the compositor has to send wheel units. Prints
# "ok   wheel w3" / "ok   wheel w64" or FAIL with the program's output.
#
#   tests/wheel/wheel.sh W3_DIST W3_PREFIX [gexc]
#
# W3_PREFIX is a Wayland prefix for W3_DIST (copied, never written). With
# W64_DIST, W64_PREFIX and W64_NATIVE in the environment (w64-dist.sh's),
# the same program as x64 on W64 as well. Needs i686- and x86_64-w64-mingw32-gcc and
# an x86 gexc. The page's half of the path (the DOM wheel event to the same
# compositor code, web/src/dom-compositor.js) is the browser's.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
w3dist=$(realpath "${1:?W3_DIST}"); w3prefix=$(realpath "${2:?W3_PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "skip wheel: no $gexc"; exit 0; }
w=${WORK:-$root/target/wheel}
rm -rf "$w"; mkdir -p "$w"
i686-w64-mingw32-gcc -O1 -Wall -o "$w/wheel32.exe" "$here/wheel.c" -lgdi32 || exit 1
x86_64-w64-mingw32-gcc -O1 -Wall -o "$w/wheel64.exe" "$here/wheel.c" -lgdi32 || exit 1
pass=0 fail=0
leg() { # NAME DIST PREFIX WINE EXE
	local name=$1 dist=$2 p=$w/pfx-$1 log=$w/$1.log extra=()
	cp -a "$3" "$p"; cp "$w/$5" "$p/drive_c/"
	if [ "$name" = w64 ]; then
		# as tests/wine64/nt-x64.sh WL=1: the Wayland driver (the prefix is
		# made without a display)
		printf '\n[Software\\\\Wine\\\\Drivers] 1791289000\n"Graphics"="wayland"\n' >> "$p/user.reg"
		printf '\n[Software\\\\Wine\\\\X11 Driver] 1791289000\n"EmulateModeset"="Y"\n' >> "$p/user.reg"
		[ -e "$p/drive_c/windows/system32/winewayland.drv" ] ||
			cp "$dist/lib/wine/x86_64-windows/winewayland.drv" "$p/drive_c/windows/system32/"
		extra=(--env WINEGEX_DOOR=1 --env "WINEDLLOVERRIDES=mscoree=;mshtml=")
	fi
	mkdir -p "$w/wl-$name"
	(cd "$p/drive_c" && GEX_WL_INPUT="$here/wheel.wl" timeout -k 10 "${TMO:-400}" "$gexc" run \
		--wayland "$w/wl-$name" --wayland-size 640x480 --wayland-out null \
		--env WINEPREFIX="$p" --env WINEDEBUG=err+all,fixme-all "${extra[@]}" \
		"$dist/lib/wine/$4/wine" "C:\\$5" 300) > "$log" 2>&1
	if grep -aq '^wheel ok' "$log"; then pass=$((pass + 1)); echo "ok   wheel $name"
	else fail=$((fail + 1))
		# The reason on the FAIL line itself: a gate keeps only that line.
		why=$(grep -a '^wheel: vertical' "$log" | tail -1)
		[ -n "$why" ] || why="no result line ($(grep -aq '^wheel: window up' "$log" && echo 'window up' || echo 'window never up'), $(grep -aq "script's clock starts" "$log" && echo 'input clock started' || echo 'input clock never started'), $(grep -ac '^\[gex\] input at' "$log") input events sent)"
		echo "FAIL wheel $name: $why"; grep -a '^wheel\|^hwheel' "$log" | tail -6 | sed 's/^/  /'
		grep -av '^wheel\|^hwheel\|^\[gex\] loaded' "$log" | tail -6 | sed 's/^/  | /'; fi
}
leg w3 "$w3dist" "$w3prefix" i386-unix wheel32.exe
if [ -n "${W64_DIST:-}" ] && [ -n "${W64_PREFIX:-}" ] && [ -n "${W64_NATIVE:-}" ]; then
	# W64_DIST with W64_NATIVE's files in place (the native PE core, which
	# has winewayland.drv), as tests/games/matrix.sh runs W64 games
	d=$(realpath "$W64_DIST") o=$w/w64dist
	mkdir -p "$o/lib/wine/x86_64-windows"
	cp -a "$d/lib/wine/x86_64-unix" "$o/lib/wine/"
	ln -s "$d/share" "$o/share"; ln -s ../../share "$o/lib/wine/share"
	for f in "$d"/lib/wine/x86_64-windows/*; do ln -s "$f" "$o/lib/wine/x86_64-windows/"; done
	for f in "$(realpath "$W64_NATIVE")"/*; do rm -f "$o/lib/wine/x86_64-windows/${f##*/}"; cp "$f" "$o/lib/wine/x86_64-windows/"; done
	leg w64 "$o" "$(realpath "$W64_PREFIX")" x86_64-unix wheel64.exe
else
	echo "skip wheel w64: no W64_DIST/W64_PREFIX/W64_NATIVE"
fi
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
