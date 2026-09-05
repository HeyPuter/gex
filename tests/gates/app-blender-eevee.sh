#!/bin/sh
# Blender with the viewport in RENDERED shading (EEVEE), splash dismissed.
#   blender-eevee.sh <out.png> [settle]  -- driver env from the caller
set -u
out=$1; settle=${2:-45}
export DISPLAY=${DISPLAY:-:78}
python3 "$(dirname "$0")/killmatch.py" blender >/dev/null 2>&1
sleep 2
EXPR='import bpy
for w in bpy.context.window_manager.windows:
    for a in w.screen.areas:
        if a.type == "VIEW_3D":
            a.spaces[0].shading.type = "RENDERED"
'
( timeout -s KILL $((settle * 2 + 40)) blender --factory-startup -noaudio \
    --python-expr "$EXPR" >"$out.log" 2>&1 & )
sleep "$settle"
# Dismiss the splash: it closes on any click outside it.
xdotool mousemove 60 700 click 1 >/dev/null 2>&1
sleep "$settle"
w=$(xwininfo -root -children 2>/dev/null | awk 'match($0,/^ *(0x[0-9a-f]+) /,id){if(match($0,/([0-9]+)x([0-9]+)\+/,wh)){a=wh[1]*wh[2]; if(a>b){b=a;p=id[1]}}} END{print p}')
[ -n "$w" ] && import -window "$w" "$out" 2>/dev/null
python3 "$(dirname "$0")/killmatch.py" blender >/dev/null 2>&1
if [ -s "$out" ]; then
  magick identify -format "shot %wx%h mean=%[fx:int(mean*255)] colors=" "$out"
  magick "$out" -depth 4 -format '%k\n' info:
else echo "no window"; fi
