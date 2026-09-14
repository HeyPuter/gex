#!/bin/sh
# VLC playing video, photographed, against llvmpipe on the same display.
#
#   app-vlc.sh <out.png> [settle seconds]
#
# The seventh verified application, and it is here for a workload none of the
# other six have: **video**. VLC's `gl` output uploads YUV planes as textures
# every frame and converts them to RGB in a fragment shader, which is a
# streaming-upload path rather than a geometry one -- SuperTuxKart, Minecraft
# and Blender all draw scenes, Krita and FreeCAD draw interfaces, OBS composites
# its own. None of them push a new texture per frame and colour-convert it.
#
# Like Krita and Kdenlive it reaches GL through Qt rather than linking libGL,
# so tests/gl-app-sweep.sh never included it: that list is built from binaries
# with libGL in their NEEDED entries.
set -u
out=$1; settle=${2:-14}
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
export DISPLAY=${DISPLAY:-:78}
ref=${out%.png}-ref.png

# The clip, generated once and kept.
#
# **Every frame is identical**, which is the whole point: the two runs are
# photographed at the same wall-clock offset but not at the same frame, and
# with moving content the comparison would measure the decoder's timing rather
# than the driver's rendering. A still image looped for twenty seconds makes
# any frame the right frame.
#
# VP8 because this VLC has no libavcodec plugin -- its codec directory holds
# theora, vpx, dav1d, jpeg and png and nothing else, so H.264 and MJPEG both
# come back "Codec ... is not supported" and the window shows VLC's idle cone.
# That failure is silent in the picture unless you know what the cone means.
clip=${GEX_VLC_CLIP:-${TMPDIR:-/tmp}/gex-vlc-clip.webm}
if [ ! -s "$clip" ]; then
  src=${clip%.webm}.png
  magick -size 1280x720 gradient:'#103060-#c04020' -fill white -pointsize 96 \
      -gravity center -annotate +0+0 'gex_wgpu' -depth 8 "PNG24:$src" || {
    echo "vlc: could not make the source image"; exit 2; }
  ffmpeg -y -loglevel error -loop 1 -i "$src" -t 20 -r 25 -pix_fmt yuv420p \
      -c:v libvpx -b:v 2M -cpu-used 8 "$clip" || {
    echo "vlc: could not encode the clip (needs ffmpeg with libvpx)"; exit 2; }
fi

shoot() {
  w=$(xwininfo -root -children 2>/dev/null |
      awk 'match($0,/^ *(0x[0-9a-f]+) /,id){if(match($0,/([0-9]+)x([0-9]+)\+/,wh)){a=wh[1]*wh[2]; if(a>b){b=a;p=id[1]}}} END{print p}')
  [ -n "$w" ] && import -window "$w" "$1" 2>/dev/null
}
run() {  # run <shot> <env...>
  shot=$1; shift
  python3 "$(dirname "$0")/killmatch.py" vlc >/dev/null 2>&1; sleep 2
  # --loop so the clip is still playing when the shot is taken. Without it a
  # twenty-second clip has finished by the time a thirty-second settle
  # elapses, and what gets photographed is VLC's idle cone on a black
  # background -- which compares *clean* between the two drivers and so passes
  # while proving nothing.
  ( env "$@" timeout -s KILL $((settle * 3 + 30)) vlc --vout=gl --loop \
      --no-qt-privacy-ask --no-video-title-show "$clip" \
      >"${shot%.png}.log" 2>&1 & )
  sleep "$settle"; shoot "$shot"
  python3 "$(dirname "$0")/killmatch.py" vlc >/dev/null 2>&1; sleep 1
}

run "$out" LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib \
    LIBGL_DRIVERS_PATH=$INST/lib/dri LIBGL_ALWAYS_SOFTWARE=1 \
    GALLIUM_DRIVER=${GALLIUM_DRIVER:-gex_wgpu}
# Which driver actually answered, asked under the identical environment: a
# gate that reports only image statistics cannot be told apart from the same
# gate run against the system driver.
echo "driver: $(env LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib \
    LIBGL_DRIVERS_PATH=$INST/lib/dri LIBGL_ALWAYS_SOFTWARE=1 \
    GALLIUM_DRIVER=${GALLIUM_DRIVER:-gex_wgpu} glxinfo -B 2>/dev/null |
    sed -n 's/^OpenGL renderer string: //p' | head -1)"
log=${out%.png}.log
lines=$(grep -c 'gex-wgpu' "$log" 2>/dev/null || echo 0)
refused=$(grep -cE 'gex-wgpu: .*(rejected|refused|unsupported|declined)' "$log" 2>/dev/null || echo 0)
# The startup format survey is not a refusal. Every application prints a dozen
# "render target refused <format>" lines while st/mesa asks which formats are
# renderable, and counting those as refusals made working applications look
# broken once already.
noise=$(grep -c 'render target refused' "$log" 2>/dev/null || echo 0)
echo "vlc driver lines=$lines refusals=$((refused - noise))"
[ "$lines" -gt 0 ] || echo "vlc WARNING: no driver lines; it may never have reached gex_wgpu"
# A codec VLC cannot decode leaves the idle cone on screen, and the comparison
# would pass. Say so from the log rather than trusting the picture.
if grep -q 'is not supported' "$log" 2>/dev/null; then
  echo "vlc FAILED: the clip's codec is not supported by this VLC build"
  grep -m1 'is not supported' "$log"
  exit 2
fi

run "$ref" LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe

[ -s "$out" ] || { echo "vlc: no shot"; exit 1; }
if [ ! -s "$ref" ]; then
  magick identify -format "vlc shot %wx%h mean=%[fx:int(mean*255)]\n" "$out"
  echo "vlc: no reference shot, nothing to compare"; exit 0
fi

# The whole window. The video fills most of it and the rest is Qt widget
# drawing, so the two drivers should agree everywhere.
rmse=$(magick compare -metric RMSE "$out" "$ref" null: 2>&1 | sed 's/ .*//')
worst=$(magick "$out" "$ref" -compose difference -composite -colorspace Gray \
        -threshold 12% -format '%[fx:int(mean*100)]' info: 2>/dev/null)
echo "vlc rmse=$rmse worst_pixels_over_12pct=${worst}%"
# Measured 2026-09-14: rmse 83.4 (0.13%) with **no** pixel differing by more
# than 12%. The allowance is generous on the RMSE because the two runs decode
# independently, and strict on the worst-pixel count because that is what
# separates "filtered slightly differently" from "drew something else".
awk -v r="$rmse" -v w="$worst" 'BEGIN{exit !(r+0 <= 600 && w+0 == 0)}' &&
  echo "vlc OK" || { echo "vlc FAILED: rendering differs from llvmpipe"; exit 1; }
