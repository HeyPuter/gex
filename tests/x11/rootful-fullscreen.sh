#!/bin/bash
# A rootful Xwayland that asks to be fullscreen fills gex's output with no
# frame: the compositor answers xdg_toplevel.set_fullscreen with the output's
# size and the fullscreen state. The X screen is made the output's size
# (-geometry, as the page's X server does; xrandr says so), and the screen's
# frames reach the compositor whole, at that size.
# A session that runs its own X window manager (KWin) lives in that screen.
#
#   tests/x11/rootful-fullscreen.sh ROOTFS [gexc]
#
# ROOTFS: a Debian arm64 tree with xwayland and x11-xserver-utils (xrandr).
# gexc: an AArch64 gexc (default target/release/gexc). A missing piece is a
# skip, not a pass.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
tree=$(realpath -m "${1:?ROOTFS}")
gexc=$(realpath -m "${2:-$root/target/release/gexc}")
[ -x "$gexc" ] || { echo "skip rootful-fullscreen: no $gexc"; exit 0; }
[ -x "$tree/usr/bin/Xwayland" ] && [ -x "$tree/usr/bin/xrandr" ] ||
    { echo "skip rootful-fullscreen: no Xwayland or xrandr in $tree"; exit 0; }
w=${WORK:-$root/target/rootful-fullscreen}
rm -rf "$w"; mkdir -p "$w/frames"
cat > "$w/run.sh" <<'SH'
XWAYLAND_NO_GLAMOR=1 Xwayland :5 -fullscreen -geometry 800x600 -shm -ac -nolisten tcp -noreset > /out/xwayland.log 2>&1 &
i=0
while [ ! -S /tmp/.X11-unix/X5 ]; do
    [ $i -ge 300 ] && { echo "rootful: no X socket"; cat /out/xwayland.log; exit 1; }
    sleep 1; i=$((i + 1))
done
sleep 2
DISPLAY=:5 xrandr 2>&1 | head -3
kill %1
SH
GEX_PPM_EVERY=1 timeout -k 10 "${TMO:-600}" "$gexc" run --sysroot "$tree" --uid 0 --bind "$w=/out" \
    --wayland /tmp/rf-wl --wayland-size 800x600 --wayland-out "$w/frames/f" \
    "$tree/usr/bin/bash" /out/run.sh > "$w/log" 2>&1
fails=()
grep -q 'current 800 x 600' "$w/log" || fails+=("the X screen is not the output's 800x600: $(grep -a 'Screen 0' "$w/log" | head -1)")
big=0 other=0
for f in "$w"/frames/*.ppm; do
    [ -e "$f" ] || continue
    if [ "$(sed -n 2p "$f")" = "800 600" ]; then big=$((big + 1)); else other=$((other + 1)); fi
done
[ $big -gt 0 ] || fails+=("no 800x600 frame ($other of other sizes)")
if [ ${#fails[@]} = 0 ]; then echo "ok   rootful-fullscreen ($big frames at 800x600)"
else echo "FAIL rootful-fullscreen: $(IFS=';'; echo "${fails[*]}")"; tail -5 "$w/log" | sed 's/^/  | /'; exit 1; fi
