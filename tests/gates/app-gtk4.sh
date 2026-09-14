#!/bin/sh
# GTK4's OpenGL renderer, rendered offscreen to a PNG, against llvmpipe.
#
#   app-gtk4.sh <out.png>
#
# The eighth verified application, and the first whose workload is a **retained
# scene graph**. GTK4 does not draw with GL calls the way the other seven do:
# GSK builds a tree of render nodes -- fills, gradients, rounded clips, blurs,
# opacity groups, transforms -- and its `opengl` renderer compiles that tree
# into passes, with offscreen targets for anything it cannot draw directly.
# That is the renderer behind every modern GNOME application, so it is a large
# amount of real-world coverage for one binary.
#
# It is also the **strongest** gate here, for a reason that has nothing to do
# with GTK: `gtk4-rendernode-tool render` takes a scene file and writes a PNG.
# There is no window to photograph, no animation, and no settle time, so the
# two runs are comparing the same frame by construction. Every other app gate
# spends most of its tolerance on the fact that it cannot be sure of that --
# see the `self` column in tests/gl-app-compare.sh, and the note in
# tests/gates/app-stk.sh about failing on noise.
#
# GEX_GTK4_KEEP=1 leaves the working files for inspection.
set -u
out=${1:-/tmp/gex-gtk4.png}
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
export DISPLAY=${DISPLAY:-:78}
work=${GEX_GTK4_WORK:-${TMPDIR:-/tmp}/gex-gtk4}
ref=${out%.png}-ref.png
mkdir -p "$work"

command -v gtk4-rendernode-tool >/dev/null 2>&1 || {
  echo "gtk4: no gtk4-rendernode-tool"; exit 2; }
command -v magick >/dev/null 2>&1 || { echo "gtk4: no ImageMagick"; exit 2; }

. "$(dirname "$0")/gpu-exclusive.sh"
gex_require_idle_gpu "app-gtk4" || exit 2

# The scene. Hand-written rather than captured from a running application so
# that what it covers is visible and reviewable here, and so it cannot drift.
# Each node is a different thing for the renderer to do: a gradient is
# interpolation, a rounded clip is a mask, a blur is a separable two-pass
# filter through an offscreen, opacity is a group rendered aside and composited,
# and a rotate is a non-axis-aligned transform that forces resampling.
cat > "$work/scene.node" <<'NODEEOF'
container {
  color {
    bounds: 0 0 256 256;
    color: rgb(24,26,34);
  }
  linear-gradient {
    bounds: 8 8 112 112;
    start: 8 8;
    end: 120 120;
    stops: 0 rgb(220,60,90), 0.5 rgb(250,200,40), 1 rgb(40,180,220);
  }
  rounded-clip {
    clip: 136 8 112 112 / 24;
    child: color {
      bounds: 136 8 112 112;
      color: rgb(90,200,120);
    }
  }
  blur {
    blur: 6;
    child: color {
      bounds: 24 152 80 80;
      color: rgb(250,120,30);
    }
  }
  opacity {
    opacity: 0.45;
    child: color {
      bounds: 120 152 60 80;
      color: rgb(255,255,255);
    }
  }
  transform {
    transform: translate(196, 192) rotate(30);
    child: color {
      bounds: -24 -24 48 48;
      color: rgb(180,120,255);
    }
  }
}
NODEEOF

# Refuse a scene the tool will not parse, rather than comparing two blanks.
gtk4-rendernode-tool info "$work/scene.node" > "$work/info.txt" 2>&1 || {
  echo "gtk4: the scene did not parse"; cat "$work/info.txt"; exit 2; }
nodes=$(awk '/^Number of nodes:/{print $4}' "$work/info.txt")
[ "${nodes:-0}" -ge 10 ] || {
  echo "gtk4: the scene parsed as only ${nodes:-0} nodes -- it is being"
  echo "  silently truncated, and a gate on a truncated scene tests nothing."
  cat "$work/info.txt"; exit 2; }

# GSK_RENDERER is a *request*. When it cannot be honoured GTK falls back, and
# the fallback is the cairo renderer -- which draws on the CPU and never calls
# this driver at all. Both sides would then render identically and the gate
# would pass having tested nothing, which is the failure mode that matters
# most here. So each run's log is kept and checked below.
#
# Both belt and braces: `--renderer=opengl` is the explicit request, and
# GSK_RENDERER is the documented environment one. The flag is on the tool's
# shared option table but not every subcommand is guaranteed to take it, so a
# run rejected for an unknown option is retried without it rather than
# reported as a rendering failure.
run() {  # run <png> <label> [env...]
  png=$1; label=$2; shift 2
  rm -f "$png"
  env "$@" GSK_RENDERER=opengl GSK_DEBUG=renderer \
      timeout -s KILL 120 gtk4-rendernode-tool render --renderer=opengl \
      "$work/scene.node" "$png" > "$work/$label.log" 2>&1
  status=$?
  if [ $status -ne 0 ] && grep -qi "unknown option\|unrecognized option" "$work/$label.log"; then
    env "$@" GSK_RENDERER=opengl GSK_DEBUG=renderer \
        timeout -s KILL 120 gtk4-rendernode-tool render \
        "$work/scene.node" "$png" > "$work/$label.log" 2>&1
    status=$?
  fi
  echo "  $label exit=$status $( [ -s "$png" ] && echo "wrote $(stat -c%s "$png") bytes" || echo "NO IMAGE" )"
}

echo "gtk4: rendering under llvmpipe (reference)"
run "$ref" ref LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe
echo "gtk4: rendering under gex_wgpu"
run "$out" gex LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib \
    LIBGL_DRIVERS_PATH=$INST/lib/dri LIBGL_ALWAYS_SOFTWARE=1 \
    GALLIUM_DRIVER=gex_wgpu

# The control. A cairo fallback on the gex side means the driver was never
# exercised; say so as a failure rather than reporting a clean comparison.
if grep -qi "cairo" "$work/gex.log" 2>/dev/null; then
  echo "gtk4 FAILED: GTK fell back to the cairo renderer, so this driver"
  echo "  drew nothing. The comparison below would have passed regardless."
  sed 's/^/    /' "$work/gex.log" | head -10
  exit 1
fi
grep -qiE "opengl|ngl|gl renderer" "$work/gex.log" 2>/dev/null || {
  echo "gtk4: could not confirm from the log which renderer ran, so this"
  echo "  cannot be called a pass. Log follows:"
  sed 's/^/    /' "$work/gex.log" | head -10
  exit 2; }

[ -s "$out" ] || { echo "gtk4 FAILED: no image from gex_wgpu"; exit 1; }
[ -s "$ref" ] || { echo "gtk4: no reference image, nothing to compare"; exit 2; }

magick identify -format "gtk4 shot %wx%h mean=%[fx:int(mean*255)]\n" "$out"
# A scene of flat fills on a dark ground has a low mean; an all-black image
# has a lower one. Distinguishing them is what stops "it rendered nothing"
# reading as a pass when the reference also fails.
mean=$(magick identify -format "%[fx:int(mean*255)]" "$out")
[ "${mean:-0}" -ge 8 ] || {
  echo "gtk4 FAILED: the image is essentially blank (mean=$mean)"; exit 1; }

rmse=$(magick compare -metric RMSE "$out" "$ref" null: 2>&1 | sed 's/ .*//')
worst=$(magick "$out" "$ref" -compose difference -composite -colorspace Gray \
        -threshold 12% -format '%[fx:int(mean*100)]' info: 2>/dev/null)
echo "gtk4 rmse=$rmse worst_pixels_over_12pct=${worst}%"
gtk4-rendernode-tool compare -q -o "$work/diff.png" "$ref" "$out" 2>/dev/null
[ -s "$work/diff.png" ] && echo "  difference image: $work/diff.png"
[ -n "${GEX_GTK4_KEEP:-}" ] || rm -f "$work/scene.node" "$work/info.txt"

# Tighter than the photographed gates, and deliberately so: there is no
# animation and no capture timing here, so the only legitimate difference is
# filtering and rounding. Anything that moves a pixel by more than an eighth
# of the range is the renderer drawing something else.
awk -v r="$rmse" -v w="$worst" 'BEGIN{exit !(r+0 <= 900 && w+0 == 0)}' &&
  echo "gtk4 OK" || { echo "gtk4 FAILED: rendering differs from llvmpipe"; exit 1; }
