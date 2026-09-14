#!/bin/sh
# Kdenlive, photographed, against llvmpipe on the same display.
#
#   app-kdenlive.sh <out.png> [settle seconds]
#
# A candidate seventh verified application, and it is here for one reason the
# other six do not cover: **Qt Quick**. Krita's canvas is a QOpenGLWidget,
# Blender and Minecraft and SuperTuxKart drive GL directly, FreeCAD draws
# through Coin3D and OBS has a renderer of its own. None of them exercise the
# Qt Quick scene graph, which is a batching renderer with stencil clipping,
# distance-field text and render-to-texture -- a different shape of GL use, and
# the one a great many modern Qt applications share.
#
# Kdenlive puts it on screen twice over: both monitors are QQuickWidgets, and
# MLT feeds the project monitor through them.
#
# Like Krita, it reaches GL through Qt rather than linking libGL, so
# tests/gl-app-sweep.sh never included it -- that list is built from binaries
# with libGL in their NEEDED entries.
set -u
out=$1; settle=${2:-35}
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
export DISPLAY=${DISPLAY:-:78}
ref=${out%.png}-ref.png

# A project, because an empty Kdenlive shows its monitors black and the Quick
# overlays are most of what there is to compare. This is MLT XML -- Kdenlive's
# native format -- with one colour producer, which avoids depending on any
# media file being present and keeps the picture deterministic.
proj=${GEX_KDENLIVE_PROJECT:-/tmp/gex-kdenlive.kdenlive}
[ -f "$proj" ] || cat > "$proj" <<'XML'
<?xml version='1.0' encoding='utf-8'?>
<mlt LC_NUMERIC="C" version="7.0.0" producer="main_bin" profile="atsc_1080p_25">
  <profile description="HD 1080p 25 fps" width="1920" height="1080"
           progressive="1" sample_aspect_num="1" sample_aspect_den="1"
           display_aspect_num="16" display_aspect_den="9"
           frame_rate_num="25" frame_rate_den="1" colorspace="709"/>
  <producer id="producer0" in="00:00:00.000" out="00:00:09.960">
    <property name="length">250</property>
    <property name="mlt_service">color</property>
    <property name="resource">#ff2060a0</property>
    <property name="kdenlive:duration">250</property>
    <property name="kdenlive:clipname">colour</property>
    <property name="kdenlive:id">2</property>
  </producer>
  <playlist id="main_bin">
    <property name="kdenlive:docproperties.version">1.1</property>
    <property name="xml_retain">1</property>
    <entry producer="producer0" in="00:00:00.000" out="00:00:09.960"/>
  </playlist>
  <playlist id="playlist0"/>
  <playlist id="playlist1"/>
  <tractor id="tractor0" in="00:00:00.000" out="00:00:09.960">
    <property name="kdenlive:projectTractor">1</property>
    <track producer="playlist0"/>
    <track producer="playlist1"/>
  </tractor>
</mlt>
XML

shoot() {
  w=$(xwininfo -root -children 2>/dev/null |
      awk 'match($0,/^ *(0x[0-9a-f]+) /,id){if(match($0,/([0-9]+)x([0-9]+)\+/,wh)){a=wh[1]*wh[2]; if(a>b){b=a;p=id[1]}}} END{print p}')
  [ -n "$w" ] && import -window "$w" "$1" 2>/dev/null
}
run() {  # run <shot> <env...>
  shot=$1; shift
  python3 "$(dirname "$0")/killmatch.py" kdenlive >/dev/null 2>&1; sleep 2
  # QT_XCB_GL_INTEGRATION=xcb_egl for the reason recorded in gl-app-targets:
  # Qt does not link libGL, so GLVND sends it to the *system* libGLX.so.0,
  # which finds this build's stub DRI driver and falls back to software
  # without ever reaching the driver -- and from outside that is
  # indistinguishable from a driver bug.
  ( env QT_XCB_GL_INTEGRATION=xcb_egl "$@" \
      timeout -s KILL $((settle * 2 + 40)) kdenlive "$proj" \
      >"$out.log" 2>&1 & )
  sleep "$settle"; shoot "$shot"
  python3 "$(dirname "$0")/killmatch.py" kdenlive >/dev/null 2>&1; sleep 1
}

run "$out" LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib \
    LIBGL_DRIVERS_PATH=$INST/lib/dri LIBGL_ALWAYS_SOFTWARE=1 \
    GALLIUM_DRIVER=${GALLIUM_DRIVER:-gex_wgpu}
# Which driver actually answered. Asking glxinfo under the identical
# environment is the point: a gate that reports only image statistics cannot
# be told apart from the same gate run against the system driver, which is the
# mistake app-blender.sh once made.
echo "driver: $(env LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib \
    LIBGL_DRIVERS_PATH=$INST/lib/dri LIBGL_ALWAYS_SOFTWARE=1 \
    GALLIUM_DRIVER=${GALLIUM_DRIVER:-gex_wgpu} glxinfo -B 2>/dev/null |
    sed -n 's/^OpenGL renderer string: //p' | head -1)"
lines=$(grep -c 'gex-wgpu' "$out.log" 2>/dev/null || echo 0)
refused=$(grep -cE 'gex-wgpu: .*(rejected|refused|unsupported|declined)' "$out.log" 2>/dev/null || echo 0)
noise=$(grep -c 'render target refused' "$out.log" 2>/dev/null || echo 0)
echo "kdenlive driver lines=$lines refusals=$((refused - noise))"
# A count of driver lines is not proof the application rendered -- see
# app-candidates.sh's NO-DRIVER correction -- so say plainly when it is zero.
[ "$lines" -gt 0 ] || echo "kdenlive WARNING: no driver lines; it may never have reached gex_wgpu"

run "$ref" LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe

[ -s "$out" ] || { echo "kdenlive: no shot"; exit 1; }
if [ ! -s "$ref" ]; then
  magick identify -format "kdenlive shot %wx%h mean=%[fx:int(mean*255)]\n" "$out"
  echo "kdenlive: no reference shot, nothing to compare"; exit 0
fi

# The whole window, unlike the Krita gate's quarter. There is no fractionally
# scaled canvas here: the monitors show a flat colour and everything else is
# widget and Quick drawing, so the two drivers should agree closely
# everywhere, and a region that does not is the finding rather than the noise.
rmse=$(magick compare -metric RMSE "$out" "$ref" null: 2>&1 | sed 's/ .*//')
worst=$(magick "$out" "$ref" -compose difference -composite -colorspace Gray \
        -threshold 12% -format '%[fx:int(mean*100)]' info: 2>/dev/null)
echo "kdenlive rmse=$rmse worst_pixels_over_12pct=${worst}%"
# A clock or a frame counter is allowed to differ, which is what the pixel
# allowance is for; a monitor that failed to draw is not, and shows up in the
# RMSE.
awk -v r="$rmse" -v w="$worst" 'BEGIN{exit !(r+0 <= 1500 && w+0 <= 1)}' &&
  echo "kdenlive OK" || { echo "kdenlive FAILED: rendering differs from llvmpipe"; exit 1; }
