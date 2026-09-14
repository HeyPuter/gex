#!/bin/sh
# Qt Quick's scene graph, rendered offscreen to a PNG, against llvmpipe.
#
#   app-qtquick.sh <out.png>
#
# The ninth verified application, and the second whose gate renders to a
# *file* rather than photographing a window -- see tests/gates/app-gtk4.sh for
# why that matters. QML's `grabToImage` hands back the rendered item, so the
# scene draws, saves itself and quits with no window to capture, no animation
# and no settle time. The two runs therefore compare the same frame by
# construction, which is what lets this run on a busy desktop.
#
# The renderer is new: Qt Quick keeps a retained scene graph and batches it
# into draw calls of its own, with its own shader pipeline (QSB/RHI). Qt
# *Widgets* -- which Krita and FreeCAD use, and which are already covered --
# do not go through any of that.
#
# GEX_QTQUICK_KEEP=1 leaves the working files.
set -u
out=${1:-/tmp/gex-qtquick.png}
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
export DISPLAY=${DISPLAY:-:78}
work=${GEX_QTQUICK_WORK:-${TMPDIR:-/tmp}/gex-qtquick}
ref=${out%.png}-ref.png
mkdir -p "$work"

QML=$(command -v qml6 2>/dev/null || command -v qml 2>/dev/null)
[ -n "$QML" ] || { echo "qtquick: no qml6"; exit 2; }
command -v magick >/dev/null 2>&1 || { echo "qtquick: no ImageMagick"; exit 2; }

# The scene, written per run because the output path is baked into it: QML
# reads no environment and `qml6 -- args` did not reach the script cleanly, so
# substituting the path is simpler than arguing with the loader.
#
# Each item is a different thing for the scene graph to batch: a gradient is
# an interpolated fill, a large radius is Qt's rounded-rectangle shader, an
# opacity below one forces the item into its own batch, and a rotation makes
# the transform non-axis-aligned.
scene() {  # scene <png path>
  cat > "$work/scene.qml" <<QMLEOF
import QtQuick
Rectangle {
    id: root
    width: 256; height: 256
    color: "#181a22"
    Rectangle {
        x: 8; y: 8; width: 112; height: 112; radius: 24
        gradient: Gradient {
            GradientStop { position: 0.0; color: "#dc3c5a" }
            GradientStop { position: 0.5; color: "#fac828" }
            GradientStop { position: 1.0; color: "#28b4dc" }
        }
    }
    Rectangle { x: 136; y: 8; width: 112; height: 112; radius: 56; color: "#5ac878" }
    Rectangle { x: 24; y: 152; width: 80; height: 80; color: "#fa781e"; opacity: 0.45 }
    Rectangle { x: 172; y: 168; width: 48; height: 48; color: "#b478ff"; rotation: 30 }
    // After a frame exists, not on completion: grabToImage before the first
    // render returns nothing and the scene then never quits, which is a
    // sixty-second timeout rather than an error.
    Timer {
        interval: 400; running: true; repeat: false
        onTriggered: root.grabToImage(function(r) {
            r.saveToFile("$1");
            Qt.callLater(Qt.quit);
        })
    }
}
QMLEOF
}

run() {  # run <png> <label> [env...]
  png=$1; label=$2; shift 2
  rm -f "$png"
  scene "$png"
  env "$@" QT_QPA_PLATFORM=xcb QSG_RHI_BACKEND=opengl QSG_INFO=1 \
      timeout -s KILL 180 "$QML" "$work/scene.qml" \
      > "$work/$label.log" 2>&1
  echo "  $label exit=$? $( [ -s "$png" ] && echo "wrote $(stat -c%s "$png") bytes" || echo "NO IMAGE" )"
}

echo "qtquick: rendering under llvmpipe (reference)"
run "$ref" ref LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe
echo "qtquick: rendering under gex_wgpu"
run "$out" gex LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib \
    LIBGL_DRIVERS_PATH=$INST/lib/dri LIBGL_ALWAYS_SOFTWARE=1 \
    GALLIUM_DRIVER=gex_wgpu

# The control, and it is a direct one rather than a reading of Qt's own
# messages: Qt Quick falls back to a `software` backend when it cannot get a
# GL context, and that backend never calls this driver at all -- both sides
# would then match and the gate would pass having tested nothing. If the
# driver ran, it said so. No driver lines means it did not.
if ! grep -q "^gex-wgpu:" "$work/gex.log" 2>/dev/null; then
  echo "qtquick FAILED: the gex_wgpu driver was never called, so Qt rendered"
  echo "  this some other way -- most likely the software backend. The"
  echo "  comparison below would have passed regardless."
  grep -iE "backend|software|rhi|opengl" "$work/gex.log" | head -6 | sed 's/^/    /'
  exit 1
fi
echo "  driver in use: $(grep -c '^gex-wgpu: shader compiled' "$work/gex.log") shaders compiled"

[ -s "$out" ] || { echo "qtquick FAILED: no image from gex_wgpu"; exit 1; }
[ -s "$ref" ] || { echo "qtquick: no reference image, nothing to compare"; exit 2; }

magick identify -format "qtquick shot %wx%h mean=%[fx:int(mean*255)]\n" "$out"
mean=$(magick identify -format "%[fx:int(mean*255)]" "$out")
[ "${mean:-0}" -ge 8 ] || {
  echo "qtquick FAILED: the image is essentially blank (mean=$mean)"; exit 1; }

rmse=$(magick compare -metric RMSE "$out" "$ref" null: 2>&1 | sed 's/ .*//')
worst=$(magick "$out" "$ref" -compose difference -composite -colorspace Gray \
        -threshold 12% -format '%[fx:int(mean*100)]' info: 2>/dev/null)
echo "qtquick rmse=$rmse worst_pixels_over_12pct=${worst}%"
[ -n "${GEX_QTQUICK_KEEP:-}" ] || rm -f "$work/scene.qml"

# Same reasoning as the GTK4 gate: no animation and no capture timing, so the
# only legitimate difference is filtering and rounding. Measured 2026-09-14:
# rmse 62.0, and not one pixel differing by more than an eighth of the range.
awk -v r="$rmse" -v w="$worst" 'BEGIN{exit !(r+0 <= 900 && w+0 == 0)}' &&
  echo "qtquick OK" || { echo "qtquick FAILED: rendering differs from llvmpipe"; exit 1; }
