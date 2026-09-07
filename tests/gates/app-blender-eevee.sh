#!/bin/sh
# Blender with the viewport in RENDERED shading (EEVEE), splash dismissed.
#   blender-eevee.sh <out.png> [settle]
set -u
out=$1; settle=${2:-45}
export DISPLAY=${DISPLAY:-:78}
# The driver, chosen here rather than inherited. This gate used to take the
# environment from its caller, which reads as a convenience and is a trap:
# running it directly measures whatever the system libGL points at -- on this
# machine radeonsi, the real GPU driver -- and produces a perfect Blender
# screenshot that says nothing about gex_wgpu. It cost two "Blender renders
# correctly" claims that were about the wrong driver entirely, and the tell was
# there to be noticed: a driver change that leaves the output byte-identical
# has probably not been exercised.
#
# GEX_REF=1 runs llvmpipe instead, for a reference shot.
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
if [ "${GEX_REF:-0}" = 1 ]; then
  export LIBGL_ALWAYS_SOFTWARE=1
  unset LD_LIBRARY_PATH LIBGL_DRIVERS_PATH GALLIUM_DRIVER
else
  export LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
  export LIBGL_DRIVERS_PATH=$INST/lib/dri
  export LIBGL_ALWAYS_SOFTWARE=1
  export GALLIUM_DRIVER=${GALLIUM_DRIVER:-gex_wgpu}
fi
# And say so. A gate that reports only image statistics cannot be told apart
# from the same gate run against the wrong driver.
echo "driver: $(glxinfo -B 2>/dev/null | sed -n 's/^OpenGL renderer string: //p' | head -1)"
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
