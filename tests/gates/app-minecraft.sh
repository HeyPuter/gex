#!/bin/bash
# Launch the Minecraft 26.2 instance under a chosen driver and photograph it.
#   app-minecraft.sh <driver> <out.png> [seconds]
#
# 26.2 requires an OpenGL 4.4 context, which is why this exists: it produced
# nothing at all until the driver reached that version.
DRV=${1:-gex_wgpu}; OUT=${2:-/tmp/mc262.png}; WAIT=${3:-120}
S=$(dirname "$0")
python3 "$S/killmatch.py" prismlauncher EntryPoint net.minecraft >/dev/null 2>&1
sleep 3
( GALLIUM_DRIVER=$DRV timeout $((WAIT + 120)) "$S/run-native.sh" \
    prismlauncher --launch "26.2" --profile "velzie_" >"$OUT.log" 2>&1 & )
sleep "$WAIT"
W=$(DISPLAY=:77 xwininfo -root -tree 2>/dev/null \
    | grep -oE '0x[0-9a-f]+ "Minecraft' | grep -oE '0x[0-9a-f]+' | head -1)
if [ -z "$W" ]; then
  echo "no Minecraft window"
else
  DISPLAY=:77 import -window "$W" "$OUT" 2>/dev/null
  [ -s "$OUT" ] && magick identify -format 'shot %wx%h mean=%[fx:int(mean*255)] colors=' "$OUT" && magick "$OUT" -depth 4 -format '%k\n' info:
fi
python3 "$S/killmatch.py" prismlauncher EntryPoint net.minecraft >/dev/null 2>&1
