#!/bin/sh
# Runs inside gex: Xwayland on gex's compositor, then the program on it.
# usage: inguest.sh DISPLAYNUM LOG prog args...
# Xwayland gets no LD_LIBRARY_PATH or LD_PRELOAD: the program's (gex-gl's libGL, a diagnostic) are not its.
n=$1; log=$2; shift 2
env -u LD_LIBRARY_PATH -u LD_PRELOAD Xwayland :$n -shm -ac -nolisten tcp > "$log" 2>&1 &
xw=$!
i=0
while [ ! -S /tmp/.X11-unix/X$n ]; do
    kill -0 $xw 2>/dev/null || { echo "[inguest] Xwayland exited"; exit 1; }
    [ $i -ge 600 ] && { echo "[inguest] no X socket"; exit 1; }
    sleep 1; i=$((i+1))
done
echo "[inguest] X :$n up after $i s"
# GEX_XFOCUS=PROGRAM (tests/games/xfocus.c): there is no window manager, so this gives the
# game's window the input focus; without it SDL games believe they are in the background.
[ -n "${GEX_XFOCUS:-}" ] && DISPLAY=:$n "$GEX_XFOCUS" 2> "$log.xfocus" &
# The program uses X: a Wayland-capable one (Unity 6, SDL) would otherwise pick gex's compositor directly.
DISPLAY=:$n exec env -u WAYLAND_DISPLAY "$@"
