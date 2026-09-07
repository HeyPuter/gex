#!/bin/sh
# Krita with a document open, so the OpenGL canvas is exercised, photographed.
#   app-krita.sh <out.png> [settle]
#
# Krita is a named target rather than an arbitrary application: its layer blend
# modes *are* KHR_blend_equation_advanced, which this driver serves through the
# colour-attachment snapshot. It also reaches GL through Qt rather than by
# linking libGL, so the app sweep -- whose list is built from binaries that
# link it directly -- never included it, and nothing else tested it either.
set -u
out=$1; settle=${2:-30}
export DISPLAY=${DISPLAY:-:78}
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
# Say which driver, so this cannot be confused with the same gate run against
# the system one -- see the note in app-blender.sh.
echo "driver: $(glxinfo -B 2>/dev/null | sed -n 's/^OpenGL renderer string: //p' | head -1)"

doc=${GEX_KRITA_DOC:-/tmp/gex-krita-canvas.png}
[ -f "$doc" ] || magick -size 1024x768 \
    gradient:'#204080-#c04020' -fill white -pointsize 72 \
    -draw "text 60,400 'krita on gex_wgpu'" "$doc" 2>/dev/null

python3 "$(dirname "$0")/killmatch.py" krita >/dev/null 2>&1
sleep 2
( timeout -s KILL $((settle * 2 + 40)) krita --nosplash "$doc" \
    >"$out.log" 2>&1 & )
sleep "$settle"
w=$(xwininfo -root -children 2>/dev/null | awk 'match($0,/^ *(0x[0-9a-f]+) /,id){if(match($0,/([0-9]+)x([0-9]+)\+/,wh)){a=wh[1]*wh[2]; if(a>b){b=a;p=id[1]}}} END{print p}')
[ -n "$w" ] && import -window "$w" "$out" 2>/dev/null
refused=$(grep -cE 'gex-wgpu: .*(rejected|refused|unsupported|declined)' "$out.log" 2>/dev/null |
          head -1)
noise=$(grep -c 'render target refused' "$out.log" 2>/dev/null | head -1)
echo "driver lines=$(grep -c 'gex-wgpu' "$out.log" 2>/dev/null) refusals=$((refused - noise)) (startup format queries excluded)"
python3 "$(dirname "$0")/killmatch.py" krita >/dev/null 2>&1
if [ -s "$out" ]; then
  magick identify -format "shot %wx%h mean=%[fx:int(mean*255)] colors=" "$out"
  magick "$out" -depth 4 -format '%k\n' info:
else echo "no window"; fi
