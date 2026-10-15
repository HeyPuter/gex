#!/bin/bash
# The desktop as a thread of the program (WINEGEX_DESKTOP_THREAD=1), with no
# explorer process beside it (desktopthread.c; Wine's win32u
# get_desktop_window, fix/desktop-thread-no-explorer). Natively (gexc run,
# gex's Wayland compositor with no output). Prints "ok   native" or FAIL with
# the processes the program saw.
#
#   tests/w3/desktopthread.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wine prefix made for it (copied, never
# written). WORK (default target/w3-desktopthread) must be on disk: gexc's
# guests do not see /dev/shm. Needs i686-w64-mingw32-gcc and an x86 gexc.
# About a minute natively with a PREFIX already updated for DIST. A stale
# one makes wineboot update it first, minutes, and Wine Mono's install prompt
# (control.exe appwiz.cpl install_mono) held that update until run_wineboot's
# 5-minute boot-event timeout, with or without the desktop thread: hence
# WINEDLLOVERRIDES=mscoree,mshtml= (no Mono or Gecko prompts).
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "FAIL: no $gexc"; exit 1; }
w=${WORK:-$root/target/w3-desktopthread}
rm -rf "$w"; mkdir -p "$w"
i686-w64-mingw32-gcc -O1 -o "$w/desktopthread.exe" "$here/desktopthread.c" || exit 1
cp -a "$prefix" "$w/pfx"; cp "$w/desktopthread.exe" "$w/pfx/drive_c/"
(cd "$w/pfx/drive_c" && timeout -k 10 "${TMO:-300}" "$gexc" run \
	--env WINEPREFIX="$w/pfx" --env WINEDEBUG=err+all,fixme-all --env WINEDLLOVERRIDES=mscoree,mshtml= --env WINEGEX_DESKTOP_THREAD=1 \
	--wayland "$w/wl" --wayland-size 640x480 --wayland-out null \
	"$dist/lib/wine/i386-unix/wine" 'C:\desktopthread.exe') > "$w/native.log" 2>&1
sed -i 's/\r$//' "$w/native.log"
if grep -q '^desktopthread ok' "$w/native.log"; then echo "ok   native ($(grep -m1 '^desktopthread ok' "$w/native.log" | sed 's/^desktopthread ok (\(.*\))/\1/'))"; exit 0; fi
echo "FAIL native"
if grep -q '^desktopthread' "$w/native.log"; then grep '^process \|^desktopthread' "$w/native.log" | sed 's/^/  /'
else grep -v '^\[' "$w/native.log" | tail -6 | sed 's/^/  /'; fi
exit 1
