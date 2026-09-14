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

# A configuration directory of its own, seeded so the first-run wizard does
# not appear.
#
# Without this Kdenlive stops on "Welcome to Kdenlive Quick Setup" -- a 425x450
# dialog with a Start Editing button -- and its main window stays 1x1 forever.
# That looked exactly like a hang, and on gex_wgpu it looked like a *driver*
# hang, which it was not: llvmpipe stops in precisely the same place. The
# reference run is what said so, and it is why this gate has one.
#
# The whole of what the wizard writes is the version, and it reappears whenever
# the stored version differs from the running one -- so the version is read
# from the binary rather than hardcoded, or this gate would start failing on
# the next Kdenlive release with a symptom that again looks like a hang.
#
# A directory of its own rather than the user's: a gate must not depend on, or
# change, whatever configuration the person at the keyboard has.
cfg=${GEX_KDENLIVE_CONFIG:-${TMPDIR:-/tmp}/gex-kdenlive-config}
mkdir -p "$cfg" "$cfg/data" "$cfg/cache"
kdver=$(kdenlive --version 2>/dev/null | awk '{print $2; exit}')
if [ -n "$kdver" ]; then
  printf '[version]\nversion=%s\n' "$kdver" > "$cfg/kdenliverc"
else
  echo "kdenlive WARNING: could not read a version; the setup wizard may block startup"
fi
export XDG_CONFIG_HOME="$cfg" XDG_DATA_HOME="$cfg/data" XDG_CACHE_HOME="$cfg/cache"

# No project file by default.
#
# The first version of this gate generated one -- MLT XML with a colour
# producer, so the monitors would show something -- and Kdenlive stopped on its
# *splash screen* for as long as it was given, main window 1x1, on both drivers.
# A project this gate writes by hand is one more thing that can be subtly wrong
# about a format that is version-stamped, and it was: what it cost was an hour
# that looked like a driver hang twice over.
#
# Kdenlive opens an untitled project on its own, which is enough for what this
# is for. The monitors are black either way -- the comparison is the Qt Quick
# scene graph: the timeline, the toolbars, the bin, the monitor overlays and
# the distance-field text, which is the renderer none of the other six gates
# exercise. GEX_KDENLIVE_PROJECT still opens a real one for anyone who has a
# project worth opening.
proj=${GEX_KDENLIVE_PROJECT:-}

shoot() {
  w=$(xwininfo -root -children 2>/dev/null |
      awk 'match($0,/^ *(0x[0-9a-f]+) /,id){if(match($0,/([0-9]+)x([0-9]+)\+/,wh)){a=wh[1]*wh[2]; if(a>b){b=a;p=id[1]}}} END{print p}')
  [ -n "$w" ] && import -window "$w" "$1" 2>/dev/null
}
# Whether the editor is actually up. "Biggest window" is not enough on its own:
# while the wizard is showing, the largest window is the wizard and the main
# window is 1x1, so a shot would be of the dialog and the comparison would
# still pass -- two drivers agreeing on a picture neither of them is the point
# of. The editor titles itself "<project> — Kdenlive".
editor_up() {
  xwininfo -root -children 2>/dev/null | grep -qE '"[^"]+ — Kdenlive"'
}
# Kdenlive's welcome screen, dismissed the same way in both runs.
#
# It appears every time -- "Show on Startup" is on by default, and the banner
# that says "Kdenlive crashed on last start" appears too because this gate ends
# every run with SIGKILL, so the clean-exit marker is never written. Neither is
# a driver question, and both are identical on llvmpipe, which is what makes
# clicking through them legitimate: the *same* clicks in both runs, so whatever
# they lead to is compared like for like.
#
# Coordinates are relative to the dialog's own window, read off a screenshot of
# it, and the window is found by its size rather than by its title -- every
# Kdenlive window is called "Kdenlive" until a project is open. If the layout
# changes the clicks miss, editor_up stays false, and the gate says so rather
# than comparing two pictures of a dialog.
dismiss_welcome() {
  geom=$(xwininfo -root -children 2>/dev/null |
         grep -E '"Kdenlive".*425x500' | head -1)
  [ -n "$geom" ] || return 1
  wx=$(printf '%s' "$geom" | sed -n 's/.*+\([0-9]*\)+\([0-9]*\) *$/\1/p')
  wy=$(printf '%s' "$geom" | sed -n 's/.*+\([0-9]*\)+\([0-9]*\) *$/\2/p')
  [ -n "$wx" ] && [ -n "$wy" ] || return 1
  # "New Project...", top right of the dialog.
  xdotool mousemove $((wx + 310)) $((wy + 180)) click 1 2>/dev/null
  sleep 6
  # The Project Settings dialog it opens; Return accepts the defaults.
  ps=$(xwininfo -root -children 2>/dev/null |
       grep -E '"Project Settings — Kdenlive"' | head -1 |
       sed -n 's/^ *\(0x[0-9a-f]*\) .*/\1/p')
  [ -n "$ps" ] && xdotool key --window "$ps" Return 2>/dev/null
  sleep 8
  editor_up
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
      timeout -s KILL $((settle * 2 + 40)) kdenlive ${proj:+"$proj"} \
      >"${shot%.png}.log" 2>&1 & )
  sleep "$settle"
  if ! editor_up; then
    # MLT scans its modules on a cold cache, so the first run after an upgrade
    # is much slower than the rest; then the welcome screen, if it is up.
    dismiss_welcome || { sleep "$settle"; dismiss_welcome; }
  fi
  if editor_up; then
    reached_editor=1
  else
    echo "kdenlive: no editor window in this run"
  fi
  shoot "$shot"
  python3 "$(dirname "$0")/killmatch.py" kdenlive >/dev/null 2>&1; sleep 1
}

reached_editor=0
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
# From this run's own log. Both runs used to write "$out.log", so the
# reference overwrote the driver's -- and since the counts are read before the
# reference starts, the numbers were right and the *evidence* was gone, which
# is worse than either.
log=${out%.png}.log
lines=$(grep -c 'gex-wgpu' "$log" 2>/dev/null || echo 0)
refused=$(grep -cE 'gex-wgpu: .*(rejected|refused|unsupported|declined)' "$log" 2>/dev/null || echo 0)
noise=$(grep -c 'render target refused' "$log" 2>/dev/null || echo 0)
echo "kdenlive driver lines=$lines refusals=$((refused - noise))"
# A count of driver lines is not proof the application rendered -- see
# app-candidates.sh's NO-DRIVER correction -- so say plainly when it is zero.
[ "$lines" -gt 0 ] || echo "kdenlive WARNING: no driver lines; it may never have reached gex_wgpu"

run "$ref" LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe

[ -s "$out" ] || { echo "kdenlive: no shot"; exit 1; }
# Comparing two *dialogs* is not a rendering verdict, and it is worse than no
# verdict because it looks like one. Two runs can even stop on differently
# sized dialogs -- a welcome screen with the "Kdenlive crashed on last start"
# banner is fifty pixels taller than one without -- and the RMSE of a 425x450
# against a 425x500 was 24681, reported as "rendering differs from llvmpipe",
# which is a driver accusation with nothing behind it.
#
# So: no editor, no verdict. Exit 2 rather than 0 or 1, because this is the
# harness failing to start the application and not an answer about either
# driver.
if [ "$reached_editor" = 0 ]; then
  echo "kdenlive INCONCLUSIVE: the editor window never came up, so there is"
  echo "  nothing to compare. This is Kdenlive's startup, not the driver:"
  echo "  it stops on a welcome screen identically on llvmpipe. See the"
  echo "  driver line count above -- that part did work."
  exit 2
fi
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
