#!/bin/sh
# Krita with a document open, photographed, against llvmpipe on the same
# display.
#
#   app-krita.sh <out.png> [settle seconds]
#
# Krita is a named target rather than an arbitrary application. Its layer blend
# modes *are* KHR_blend_equation_advanced, which this driver serves through the
# colour-attachment snapshot, and its canvas is a real OpenGL surface rather
# than a cairo one -- unlike GIMP and Inkscape, which reach the driver and then
# draw in software. It also gets GL through Qt instead of linking libGL, so
# tests/gl-app-sweep.sh never included it: that list is built from binaries
# that link the library directly.
#
# The document matters. Krita with no file open shows a welcome screen and
# never brings its canvas up, so this makes one and passes it on the command
# line.
set -u
out=$1; settle=${2:-30}
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
export DISPLAY=${DISPLAY:-:78}
ref=${out%.png}-ref.png
doc=${GEX_KRITA_DOC:-/tmp/gex-krita-canvas.png}
[ -f "$doc" ] || magick -size 1024x768 \
    gradient:'#204080-#c04020' -fill white -pointsize 72 \
    -draw "text 60,400 'krita on gex_wgpu'" "$doc" 2>/dev/null

shoot() {
  w=$(xwininfo -root -children 2>/dev/null |
      awk 'match($0,/^ *(0x[0-9a-f]+) /,id){if(match($0,/([0-9]+)x([0-9]+)\+/,wh)){a=wh[1]*wh[2]; if(a>b){b=a;p=id[1]}}} END{print p}')
  [ -n "$w" ] && import -window "$w" "$1" 2>/dev/null
}
run() {  # run <shot> <env...>
  shot=$1; shift
  python3 "$(dirname "$0")/killmatch.py" krita >/dev/null 2>&1; sleep 2
  ( env "$@" timeout -s KILL $((settle * 2 + 40)) krita --nosplash "$doc" \
      >"$out.log" 2>&1 & )
  sleep "$settle"; shoot "$shot"
  python3 "$(dirname "$0")/killmatch.py" krita >/dev/null 2>&1; sleep 1
}

run "$out" LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib \
    LIBGL_DRIVERS_PATH=$INST/lib/dri LIBGL_ALWAYS_SOFTWARE=1 \
    GALLIUM_DRIVER=${GALLIUM_DRIVER:-gex_wgpu}
# Which driver actually answered. Krita's own log does not name it, so ask
# glxinfo under the identical environment -- and asking it the same way the run
# did is the point: a gate that reports only image statistics cannot be told
# apart from the same gate run against the system driver, which is exactly the
# mistake app-blender.sh once made.
echo "driver: $(env LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib \
    LIBGL_DRIVERS_PATH=$INST/lib/dri LIBGL_ALWAYS_SOFTWARE=1 \
    GALLIUM_DRIVER=${GALLIUM_DRIVER:-gex_wgpu} glxinfo -B 2>/dev/null |
    sed -n 's/^OpenGL renderer string: //p' | head -1)"
lines=$(grep -c 'gex-wgpu' "$out.log" 2>/dev/null || echo 0)
refused=$(grep -cE 'gex-wgpu: .*(rejected|refused|unsupported|declined)' "$out.log" 2>/dev/null || echo 0)
noise=$(grep -c 'render target refused' "$out.log" 2>/dev/null || echo 0)
echo "krita driver lines=$lines refusals=$((refused - noise))"

run "$ref" LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe

[ -s "$out" ] || { echo "krita: no shot"; exit 1; }
if [ ! -s "$ref" ]; then
  magick identify -format "krita shot %wx%h mean=%[fx:int(mean*255)]\n" "$out"
  echo "krita: no reference shot, nothing to compare"; exit 0
fi

# What to compare, and why not the whole frame. The right-hand panels are pure
# widget drawing -- the colour wheel, the layer thumbnails, the brush presets --
# and they come out *byte-identical* between the two drivers, so they are the
# sharp signal and any difference there is real. The canvas is displayed at a
# fractional zoom, so it is a filtered downscale, and two drivers filter
# slightly differently for reasons that are not defects: measured 0.7% RMSE
# there with no single pixel off by more than 12%.
W=$(magick identify -format '%w' "$out"); H=$(magick identify -format '%h' "$out")
PW=$((W / 4)); PX=$((W - PW))
magick "$out" -crop ${PW}x${H}+${PX}+0 +repage /tmp/krita-cmp-got.png
magick "$ref" -crop ${PW}x${H}+${PX}+0 +repage /tmp/krita-cmp-exp.png
panel=$(magick compare -metric RMSE /tmp/krita-cmp-got.png /tmp/krita-cmp-exp.png null: 2>&1 | sed 's/ .*//')
worst=$(magick "$out" "$ref" -compose difference -composite -colorspace Gray \
        -threshold 12% -format '%[fx:int(mean*100)]' info: 2>/dev/null)
echo "krita panels_rmse=$panel worst_pixels_over_12pct=${worst}%"
# Panels must match closely and nothing anywhere may differ grossly. A canvas
# that failed to draw shows up in both at once.
awk -v p="$panel" -v w="$worst" 'BEGIN{exit !(p+0 <= 60 && w+0 == 0)}' &&
  echo "krita OK" || { echo "krita FAILED: rendering differs from llvmpipe"; exit 1; }
